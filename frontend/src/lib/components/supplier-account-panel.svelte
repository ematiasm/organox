<script lang="ts">
	import {
		api,
		ApiError,
		type PaymentMethod,
		type SupplierAccount,
		type SupplierAccountEntryKind,
		type SupplierPayment
	} from '$lib/api/client';
	import { allocatePurchasesOldestFirst, formatCents, parseCents } from '$lib/money';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import * as Select from '$lib/components/ui/select';
	import {
		Table,
		TableBody,
		TableCell,
		TableHead,
		TableHeader,
		TableRow
	} from '$lib/components/ui/table';
	import { Tabs, TabsContent, TabsList, TabsTrigger } from '$lib/components/ui/tabs';
	import { cn } from '$lib/utils';

	let {
		supplierId,
		onMutated
	}: {
		supplierId: string;
		/** Refresh the parent supplier summary and list after recording a payment. */
		onMutated: () => Promise<void>;
	} = $props();

	const ENTRY_LABELS: Record<SupplierAccountEntryKind, string> = {
		purchase_charge: 'Purchase charge',
		payment: 'Payment',
		credit_refund: 'Credit refund',
		adjustment_in: 'Adjustment (+)',
		adjustment_out: 'Adjustment (−)'
	};

	let tab = $state<'statement' | 'pay'>('statement');
	let loading = $state(false);
	let loadError = $state<string | null>(null);
	let account = $state<SupplierAccount | null>(null);
	let methods = $state<PaymentMethod[]>([]);

	// --- Payment form state ---
	let paymentMethodId = $state('');
	let amount = $state('');
	let note = $state('');
	let allocations = $state<Record<string, string>>({});
	let submitting = $state(false);
	let paymentError = $state<string | null>(null);
	let lastPayment = $state<SupplierPayment | null>(null);

	// One idempotency key per intended payment, kept across failures and cleared
	// only after the POST succeeds so a retry replays instead of double-paying.
	let idempotencyKey: string | null = null;
	let loadSeq = 0;

	const selectedMethod = $derived(methods.find((m) => m.id === paymentMethodId) ?? null);
	const amountCents = $derived(amount.trim() === '' ? null : parseCents(amount));
	const allocationSummary = $derived.by(() => {
		let total = 0n;
		let invalid = false;
		for (const raw of Object.values(allocations)) {
			const trimmed = raw.trim();
			if (!trimmed) continue;
			const parsed = parseCents(trimmed);
			if (parsed === null || parsed < 0n) {
				invalid = true;
				continue;
			}
			total += parsed;
		}
		const unapplied = amountCents !== null && !invalid ? amountCents - total : null;
		return { total, invalid, unapplied };
	});

	function failure(error: unknown, fallback: string): string {
		if (error instanceof ApiError) {
			return error.problem?.detail ?? `The server rejected the request (HTTP ${error.status}).`;
		}
		return fallback;
	}

	function when(iso: string): string {
		const date = new Date(iso);
		return Number.isNaN(date.getTime())
			? iso
			: date.toLocaleString('en-US', { dateStyle: 'short', timeStyle: 'short' });
	}

	/** Plain-HTTP-safe idempotency key (LAN/tailnet may lack `crypto.randomUUID`). */
	function newIdempotencyKey(): string {
		if (typeof crypto.randomUUID === 'function') return crypto.randomUUID();

		const bytes = new Uint8Array(16);
		if (typeof crypto.getRandomValues === 'function') {
			crypto.getRandomValues(bytes);
			return Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('');
		}

		return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
	}

	function resetForm() {
		amount = '';
		note = '';
		allocations = {};
		paymentError = null;
		idempotencyKey = null;
	}

	async function loadAccount(id: string) {
		const seq = ++loadSeq;
		loading = true;
		loadError = null;
		try {
			const [nextAccount, nextMethods] = await Promise.all([
				api.getSupplierAccount(id),
				api.listPaymentMethods()
			]);
			if (seq !== loadSeq) return;
			account = nextAccount;
			methods = nextMethods.filter((m) => m.is_active);
		} catch (error) {
			if (seq !== loadSeq) return;
			loadError = failure(error, 'Could not load the supplier account.');
		} finally {
			if (seq === loadSeq) loading = false;
		}
	}

	$effect(() => {
		const id = supplierId;
		tab = 'statement';
		account = null;
		lastPayment = null;
		paymentMethodId = '';
		resetForm();
		void loadAccount(id);
	});

	function prefillOldestFirst() {
		if (!account || amountCents === null || amountCents <= 0n) return;
		allocations = allocatePurchasesOldestFirst(amountCents, account.outstanding_purchases);
	}

	function clearAllocations() {
		allocations = {};
	}

	async function submitPayment(event: SubmitEvent) {
		event.preventDefault();
		if (submitting || !selectedMethod || !amount.trim()) return;

		paymentError = null;
		lastPayment = null;
		submitting = true;

		const openPurchaseIds = new Set(
			account?.outstanding_purchases.map((p) => p.purchase_id) ?? []
		);
		const allocationList = Object.entries(allocations)
			.map(([purchase_id, raw]) => ({ purchase_id, amount: raw.trim() }))
			.filter((item) => {
				if (!openPurchaseIds.has(item.purchase_id) || !item.amount) return false;
				const parsed = parseCents(item.amount);
				return parsed === null || parsed !== 0n;
			});

		let recorded: SupplierPayment;
		try {
			idempotencyKey ??= newIdempotencyKey();
			recorded = await api.recordSupplierPayment(supplierId, {
				payment_method_id: selectedMethod.id,
				amount: amount.trim(),
				allocations: allocationList,
				note: note.trim() || undefined,
				idempotency_key: idempotencyKey
			});
		} catch (error) {
			// Keep the idempotency key so a retry replays this intended payment.
			paymentError = failure(error, 'Could not record the payment.');
			submitting = false;
			return;
		}

		// Clear the key and form before refreshing so a refresh failure cannot
		// invite a second POST for a payment that already landed.
		lastPayment = recorded;
		resetForm();
		try {
			await Promise.all([loadAccount(supplierId), onMutated()]);
		} catch (error) {
			paymentError = failure(error, 'Payment recorded, but the screen could not refresh.');
		} finally {
			submitting = false;
		}
	}
</script>

<div class="border-t border-border px-4 pt-4 pb-6">
	<Tabs bind:value={tab}>
		<TabsList class="mb-3 w-full">
			<TabsTrigger value="statement" class="flex-1">Statement</TabsTrigger>
			<TabsTrigger value="pay" class="flex-1">Pay supplier</TabsTrigger>
		</TabsList>

		{#if loadError}
			<p class="mb-3 text-sm text-destructive" role="alert">{loadError}</p>
		{/if}

		{#if lastPayment}
			<div
				class="mb-3 rounded-md border border-primary/40 bg-primary/10 px-3 py-2 text-xs"
				role="status"
			>
				<strong>Payment recorded.</strong>
				Balance: <span class="tabular-nums font-medium">{lastPayment.balance}</span>
				· Available credit:
				<span class="tabular-nums font-medium">{lastPayment.unapplied_credit}</span>.
			</div>
		{/if}

		<TabsContent value="statement" class="grid gap-4">
			{#if loading && !account}
				<p class="py-4 text-center text-sm text-muted-foreground">Loading account…</p>
			{:else if account}
				{#if account.outstanding_purchases.length > 0}
					<div class="grid gap-2">
						<h3 class="text-xs font-medium uppercase tracking-wide text-muted-foreground">
							Open purchases ({account.outstanding_purchases.length})
						</h3>
						<div class="overflow-hidden rounded-md border border-border">
							<Table>
								<TableHeader>
									<TableRow>
										<TableHead>Purchase</TableHead>
										<TableHead>Date</TableHead>
										<TableHead class="text-right">Charged</TableHead>
										<TableHead class="text-right">Outstanding</TableHead>
									</TableRow>
								</TableHeader>
								<TableBody>
									{#each account.outstanding_purchases as purchase (purchase.purchase_id)}
										<TableRow>
											<TableCell class="font-mono text-xs" title={purchase.purchase_id}>
												{purchase.purchase_id.slice(0, 8)}
											</TableCell>
											<TableCell class="text-xs text-muted-foreground">
												{when(purchase.completed_at)}
											</TableCell>
											<TableCell class="text-right text-xs tabular-nums text-muted-foreground">
												{purchase.charged}
											</TableCell>
											<TableCell class="text-right text-xs font-medium tabular-nums">
												{purchase.outstanding}
											</TableCell>
										</TableRow>
									{/each}
								</TableBody>
							</Table>
						</div>
					</div>
				{/if}

				<div class="grid gap-2">
					<h3 class="text-xs font-medium uppercase tracking-wide text-muted-foreground">
						Account statement
					</h3>
					{#if account.entries.length === 0}
						<div
							class="rounded-md border border-border p-6 text-center text-sm text-muted-foreground"
						>
							No account movements yet.
						</div>
					{:else}
						<div class="overflow-hidden rounded-md border border-border">
							<Table>
								<TableHeader>
									<TableRow>
										<TableHead>Date</TableHead>
										<TableHead>Type</TableHead>
										<TableHead class="text-right">Amount</TableHead>
										<TableHead class="text-right">Balance</TableHead>
									</TableRow>
								</TableHeader>
								<TableBody>
									{#each account.entries as entry (entry.id)}
										<TableRow>
											<TableCell class="text-xs text-muted-foreground">
												<div>{when(entry.created_at)}</div>
												{#if entry.note}
													<div class="mt-0.5 text-[11px] text-muted-foreground/80">
														{entry.note}
													</div>
												{/if}
											</TableCell>
											<TableCell>
												<Badge variant="outline" class="text-[11px]">
													{ENTRY_LABELS[entry.kind]}
												</Badge>
											</TableCell>
											<TableCell
												class={cn(
													'text-right text-xs tabular-nums',
													entry.amount.startsWith('-') ? 'text-primary' : 'font-medium'
												)}
											>
												{entry.amount}
											</TableCell>
											<TableCell class="text-right text-xs font-medium tabular-nums">
												{entry.running_balance}
											</TableCell>
										</TableRow>
									{/each}
								</TableBody>
							</Table>
						</div>
					{/if}
				</div>
			{/if}
		</TabsContent>

		<TabsContent value="pay">
			<form class="grid gap-4" onsubmit={submitPayment}>
				<div class="grid gap-2">
					<Label for="supplier-payment-method">Payment method</Label>
					{#if methods.length === 0}
						<p class="text-xs text-destructive">
							No active payment methods.
							<a href="/settings/payments" class="text-primary underline">Add one in settings</a>
							first.
						</p>
					{:else}
						<Select.Root type="single" bind:value={paymentMethodId}>
							<Select.Trigger id="supplier-payment-method" class="w-full" disabled={submitting}>
								{selectedMethod?.name ?? 'Choose a payment method'}
							</Select.Trigger>
							<Select.Content>
								{#each methods as method (method.id)}
									<Select.Item value={method.id} label={method.name}>
										{method.name}
										<span class="text-xs text-muted-foreground">← {method.wallet_name}</span>
									</Select.Item>
								{/each}
							</Select.Content>
						</Select.Root>
					{/if}
				</div>

				<div class="grid gap-2">
					<Label for="supplier-payment-amount">Amount</Label>
					<Input
						id="supplier-payment-amount"
						type="text"
						inputmode="decimal"
						placeholder="0.00"
						bind:value={amount}
						required
						disabled={submitting}
						class="tabular-nums"
					/>
				</div>

				<div class="grid gap-2">
					<Label for="supplier-payment-note">Note</Label>
					<Input
						id="supplier-payment-note"
						type="text"
						placeholder="Optional reference or payment note"
						bind:value={note}
						maxlength={240}
						disabled={submitting}
					/>
				</div>

				{#if account && account.outstanding_purchases.length > 0}
					<div class="grid gap-2">
						<div class="flex items-center justify-between gap-2">
							<span class="text-xs font-medium text-muted-foreground">
								Allocate to open purchases (optional)
							</span>
							<div class="flex gap-1.5">
								<Button
									type="button"
									variant="outline"
									size="sm"
									disabled={submitting || amountCents === null || amountCents <= 0n}
									onclick={prefillOldestFirst}
								>
									Oldest first
								</Button>
								<Button
									type="button"
									variant="ghost"
									size="sm"
									disabled={submitting}
									onclick={clearAllocations}
								>
									Clear
								</Button>
							</div>
						</div>

						<div class="grid gap-2 rounded-md border border-border p-2.5">
							{#each account.outstanding_purchases as purchase (purchase.purchase_id)}
								<div class="flex items-center justify-between gap-3 text-xs">
									<div class="min-w-0">
										<div class="font-mono" title={purchase.purchase_id}>
											{purchase.purchase_id.slice(0, 8)}
										</div>
										<div class="text-muted-foreground">
											{when(purchase.completed_at)} · Due
											<span class="tabular-nums font-medium">{purchase.outstanding}</span>
										</div>
									</div>
									<Input
										type="text"
										inputmode="decimal"
										placeholder="0.00"
										aria-label={`Allocation for purchase ${purchase.purchase_id.slice(0, 8)}`}
										class="h-8 w-28 text-right text-xs tabular-nums"
										disabled={submitting}
										value={allocations[purchase.purchase_id] ?? ''}
										oninput={(e) => {
											allocations[purchase.purchase_id] = (e.target as HTMLInputElement).value;
										}}
									/>
								</div>
							{/each}
						</div>

						<div class="flex items-center justify-between text-xs text-muted-foreground tabular-nums">
							<span>Allocated: {formatCents(allocationSummary.total)}</span>
							{#if allocationSummary.unapplied !== null}
								<span class={cn(allocationSummary.unapplied < 0n && 'text-destructive')}>
									{allocationSummary.unapplied < 0n
										? `Exceeds amount by ${formatCents(-allocationSummary.unapplied)}`
										: `Unapplied credit: ${formatCents(allocationSummary.unapplied)}`}
								</span>
							{/if}
						</div>
					</div>
				{:else if account}
					<p class="text-xs text-muted-foreground">
						No open purchases. The full payment amount will remain as available credit.
					</p>
				{/if}

				{#if paymentError}
					<p class="text-sm text-destructive" role="alert">{paymentError}</p>
				{/if}

				<div class="flex justify-end">
					<Button
						type="submit"
						disabled={submitting || !selectedMethod || !amount.trim()}
					>
						{submitting ? 'Recording…' : 'Record payment'}
					</Button>
				</div>
			</form>
		</TabsContent>
	</Tabs>
</div>
