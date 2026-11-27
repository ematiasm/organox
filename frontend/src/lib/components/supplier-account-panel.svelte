<script lang="ts">
	import {
		api,
		ApiError,
		type OutstandingPurchase,
		type PaymentMethod,
		type SupplierAccount,
		type SupplierAccountEntryKind,
		type SupplierAccountStatementEntry,
		type SupplierAdjustment,
		type SupplierAdjustmentDirection,
		type SupplierCreditRefund,
		type SupplierPayment
	} from '$lib/api/client';
	import {
		allocatePurchasesOldestFirst,
		formatCents,
		maxRefundableCents,
		parseCents
	} from '$lib/money';
	import AccountDocumentDialog, {
		type AccountDocumentTarget
	} from '$lib/components/account-document-dialog.svelte';
	import ChevronRightIcon from '@lucide/svelte/icons/chevron-right';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import * as Select from '$lib/components/ui/select';
	import { cn } from '$lib/utils';

	let {
		supplierId,
		balance,
		availableCredit,
		onMutated
	}: {
		supplierId: string;
		balance: string;
		availableCredit: string;
		/** Refresh the parent supplier summary and list after recording an account mutation. */
		onMutated: () => Promise<void>;
	} = $props();

	const ENTRY_LABELS: Record<SupplierAccountEntryKind, string> = {
		purchase_charge: 'Purchase',
		payment: 'Payment',
		purchase_return: 'Purchase return',
		purchase_return_credit: 'Return credit',
		credit_refund: 'Credit refund',
		adjustment_in: 'Adjustment (+)',
		adjustment_out: 'Adjustment (−)'
	};

	const ADJUSTMENT_DIRECTION_LABELS: Record<SupplierAdjustmentDirection, string> = {
		in: 'Increase payable (+)',
		out: 'Decrease payable (−)'
	};

	type ActiveForm = 'pay' | 'refund' | 'adjust' | null;

	type ActionOutcome = {
		heading: string;
		balance: string;
		unapplied_credit: string;
	};

	type LedgerItem = {
		entry: SupplierAccountStatementEntry;
		openPurchase: OutstandingPurchase | null;
		purchaseReturns: SupplierAccountStatementEntry[];
	};

	const STATEMENT_PAGE_SIZE = 100;

	let activeForm = $state<ActiveForm>(null);
	let selectedDoc = $state<AccountDocumentTarget | null>(null);
	let loading = $state(false);
	let loadError = $state<string | null>(null);
	let account = $state<SupplierAccount | null>(null);
	let statementPage = $state(1);
	let methods = $state<PaymentMethod[]>([]);
	let submitting = $state(false);
	let lastOutcome = $state<ActionOutcome | null>(null);

	// --- Payment form state ---
	let paymentMethodId = $state('');
	let amount = $state('');
	let note = $state('');
	let allocations = $state<Record<string, string>>({});
	let paymentError = $state<string | null>(null);

	// --- Credit refund form state ---
	let refundPaymentMethodId = $state('');
	let refundAmount = $state('');
	let refundNote = $state('');
	let refundError = $state<string | null>(null);

	// --- Manual adjustment form state ---
	let adjustmentDirection = $state<SupplierAdjustmentDirection>('in');
	let adjustmentAmount = $state('');
	let adjustmentNote = $state('');
	let adjustmentError = $state<string | null>(null);

	// One idempotency key per intended payment or credit refund, kept across
	// failures and cleared only after the POST succeeds so a retry replays
	// instead of writing a duplicate wallet movement.
	let idempotencyKey: string | null = null;
	let refundIdempotencyKey: string | null = null;
	let loadedSupplierId: string | null = null;
	let loadSeq = 0;

	const displayBalance = $derived(account?.balance ?? balance);
	const displayCredit = $derived(account?.unapplied_credit ?? availableCredit);
	const selectedMethod = $derived(methods.find((m) => m.id === paymentMethodId) ?? null);
	const selectedRefundMethod = $derived(
		methods.find((m) => m.id === refundPaymentMethodId) ?? null
	);
	const amountCents = $derived(amount.trim() === '' ? null : parseCents(amount));
	const refundAmountCents = $derived(refundAmount.trim() === '' ? null : parseCents(refundAmount));
	const refundableCents = $derived(
		account ? maxRefundableCents(account.balance, account.unapplied_credit) : 0n
	);
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

	/**
	 * Build a unified balance ledger: each purchase charge carries its live open
	 * breakdown and nests any associated purchase returns right underneath it,
	 * while payments, return credits, credit refunds, and manual adjustments
	 * appear inline.
	 */
	const ledgerItems = $derived.by<LedgerItem[]>(() => {
		if (!account) return [];
		const openByPurchase = new Map<string, OutstandingPurchase>();
		for (const purchase of account.outstanding_purchases) {
			openByPurchase.set(purchase.purchase_id, purchase);
		}

		const chargePurchaseIds = new Set<string>();
		const returnsByPurchase = new Map<string, SupplierAccountStatementEntry[]>();
		for (const entry of account.entries) {
			if (entry.kind === 'purchase_charge' && entry.purchase_id) {
				chargePurchaseIds.add(entry.purchase_id);
			}
			if (entry.kind === 'purchase_return' && entry.purchase_id) {
				const list = returnsByPurchase.get(entry.purchase_id) ?? [];
				list.push(entry);
				returnsByPurchase.set(entry.purchase_id, list);
			}
		}

		const items: LedgerItem[] = [];
		for (const entry of account.entries) {
			if (
				entry.kind === 'purchase_return' &&
				entry.purchase_id &&
				chargePurchaseIds.has(entry.purchase_id)
			) {
				continue;
			}
			if (entry.kind === 'purchase_charge' && entry.purchase_id) {
				items.push({
					entry,
					openPurchase: openByPurchase.get(entry.purchase_id) ?? null,
					purchaseReturns: returnsByPurchase.get(entry.purchase_id) ?? []
				});
				continue;
			}
			items.push({ entry, openPurchase: null, purchaseReturns: [] });
		}
		return items;
	});

	function balanceHint(value: string): string {
		if (value.startsWith('-')) return 'Credit with supplier';
		if (value === '0.00' || value === '0') return 'Settled';
		return 'We owe supplier';
	}

	function isPartiallyPaid(purchase: OutstandingPurchase): boolean {
		const allocated = parseCents(purchase.allocated) ?? 0n;
		const cancelled = parseCents(purchase.cancelled) ?? 0n;
		return allocated > 0n || cancelled > 0n;
	}

	function isNonZero(value: string): boolean {
		const cents = parseCents(value);
		return cents !== null && cents !== 0n;
	}

	function formatSigned(value: string): string {
		return value.startsWith('-') || value.startsWith('+') ? value : `+${value}`;
	}

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

	function resetPaymentForm() {
		amount = '';
		note = '';
		allocations = {};
		paymentError = null;
		idempotencyKey = null;
	}

	function resetRefundForm() {
		refundAmount = '';
		refundNote = '';
		refundError = null;
		refundIdempotencyKey = null;
	}

	function resetAdjustmentForm() {
		adjustmentDirection = 'in';
		adjustmentAmount = '';
		adjustmentNote = '';
		adjustmentError = null;
	}

	function toggleForm(next: Exclude<ActiveForm, null>) {
		activeForm = activeForm === next ? null : next;
	}

	const statementLastPage = $derived(
		account ? Math.max(1, Math.ceil(account.entries_total / STATEMENT_PAGE_SIZE)) : 1
	);
	const statementRange = $derived.by(() => {
		if (!account || account.entries_total === 0) return '';
		const from = (statementPage - 1) * STATEMENT_PAGE_SIZE + 1;
		const to = (statementPage - 1) * STATEMENT_PAGE_SIZE + account.entries.length;
		return `${from}–${to} of ${account.entries_total}`;
	});

	async function loadAccount(id: string, requestedPage?: number) {
		const seq = ++loadSeq;
		loading = true;
		loadError = null;
		try {
			if (requestedPage !== undefined) {
				const targetPage = Math.max(1, requestedPage);
				const nextAccount = await api.getSupplierAccount(id, {
					limit: STATEMENT_PAGE_SIZE,
					offset: (targetPage - 1) * STATEMENT_PAGE_SIZE
				});
				if (seq !== loadSeq) return;
				account = nextAccount;
				statementPage = targetPage;
				return;
			}

			// Default to the latest page of the oldest-first ledger so the operator
			// always sees recent activity first. Accounts with <= 100 entries finish
			// in one request.
			const [firstPage, nextMethods] = await Promise.all([
				api.getSupplierAccount(id, { limit: STATEMENT_PAGE_SIZE, offset: 0 }),
				api.listPaymentMethods()
			]);
			if (seq !== loadSeq) return;

			const lastPage = Math.max(1, Math.ceil(firstPage.entries_total / STATEMENT_PAGE_SIZE));
			let resolvedAccount = firstPage;
			if (lastPage > 1) {
				resolvedAccount = await api.getSupplierAccount(id, {
					limit: STATEMENT_PAGE_SIZE,
					offset: (lastPage - 1) * STATEMENT_PAGE_SIZE
				});
				if (seq !== loadSeq) return;
			}

			account = resolvedAccount;
			statementPage = lastPage;
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
		// Only reset active form, banner, and inputs when switching to a different
		// supplier — `onMutated()` refreshes the parent `supplier` object with
		// the same id and must not close the operator's active form.
		if (id === loadedSupplierId) return;
		loadedSupplierId = id;
		activeForm = null;
		selectedDoc = null;
		account = null;
		statementPage = 1;
		lastOutcome = null;
		paymentMethodId = '';
		refundPaymentMethodId = '';
		resetPaymentForm();
		resetRefundForm();
		resetAdjustmentForm();
		void loadAccount(id);
	});

	function prefillOldestFirst() {
		if (!account || amountCents === null || amountCents <= 0n) return;
		allocations = allocatePurchasesOldestFirst(amountCents, account.outstanding_purchases);
	}

	function clearAllocations() {
		allocations = {};
	}

	function fillMaxRefund() {
		if (refundableCents <= 0n) return;
		refundAmount = formatCents(refundableCents);
	}

	async function submitPayment(event: SubmitEvent) {
		event.preventDefault();
		if (submitting || !selectedMethod || !amount.trim()) return;

		paymentError = null;
		lastOutcome = null;
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
		lastOutcome = {
			heading: 'Payment recorded.',
			balance: recorded.balance,
			unapplied_credit: recorded.unapplied_credit
		};
		resetPaymentForm();
		try {
			await Promise.all([loadAccount(supplierId), onMutated()]);
		} catch (error) {
			paymentError = failure(error, 'Payment recorded, but the screen could not refresh.');
		} finally {
			submitting = false;
		}
	}

	async function submitCreditRefund(event: SubmitEvent) {
		event.preventDefault();
		if (submitting || !selectedRefundMethod || !refundAmount.trim() || !refundNote.trim()) return;

		refundError = null;
		lastOutcome = null;
		submitting = true;

		let recorded: SupplierCreditRefund;
		try {
			refundIdempotencyKey ??= newIdempotencyKey();
			recorded = await api.refundSupplierCredit(supplierId, {
				payment_method_id: selectedRefundMethod.id,
				amount: refundAmount.trim(),
				note: refundNote.trim(),
				idempotency_key: refundIdempotencyKey
			});
		} catch (error) {
			// Keep the idempotency key so a retry replays this intended refund.
			refundError = failure(error, 'Could not record the credit refund.');
			submitting = false;
			return;
		}

		// Clear the key and form before refreshing so a refresh failure cannot
		// invite a second POST for a refund that already landed.
		lastOutcome = {
			heading: 'Credit refund recorded.',
			balance: recorded.balance,
			unapplied_credit: recorded.unapplied_credit
		};
		resetRefundForm();
		try {
			await Promise.all([loadAccount(supplierId), onMutated()]);
		} catch (error) {
			refundError = failure(error, 'Credit refund recorded, but the screen could not refresh.');
		} finally {
			submitting = false;
		}
	}

	async function submitAdjustment(event: SubmitEvent) {
		event.preventDefault();
		if (submitting || !adjustmentAmount.trim() || !adjustmentNote.trim()) return;

		adjustmentError = null;
		lastOutcome = null;
		submitting = true;

		let recorded: SupplierAdjustment;
		try {
			recorded = await api.recordSupplierAdjustment(supplierId, {
				direction: adjustmentDirection,
				amount: adjustmentAmount.trim(),
				note: adjustmentNote.trim()
			});
		} catch (error) {
			adjustmentError = failure(error, 'Could not record the manual adjustment.');
			submitting = false;
			return;
		}

		lastOutcome = {
			heading: 'Adjustment recorded.',
			balance: recorded.balance,
			unapplied_credit: recorded.unapplied_credit
		};
		resetAdjustmentForm();
		try {
			await Promise.all([loadAccount(supplierId), onMutated()]);
		} catch (error) {
			adjustmentError = failure(error, 'Adjustment recorded, but the screen could not refresh.');
		} finally {
			submitting = false;
		}
	}
</script>

<!-- Balance & Available credit + payment action toolbar -->
<section class="grid gap-3 border-b border-border px-4 pb-4">
	<div class="grid grid-cols-2 gap-3">
		<div class="rounded-lg border border-border bg-muted/30 p-3">
			<div class="text-xs text-muted-foreground">Balance</div>
			<div class="mt-0.5 text-xl font-semibold tracking-tight tabular-nums">{displayBalance}</div>
			<div class="text-[11px] text-muted-foreground">{balanceHint(displayBalance)}</div>
		</div>
		<div class="rounded-lg border border-border bg-muted/30 p-3">
			<div class="text-xs text-muted-foreground">Available credit</div>
			<div class="mt-0.5 text-xl font-semibold tracking-tight tabular-nums">{displayCredit}</div>
			<div class="text-[11px] text-muted-foreground">Unapplied payments</div>
		</div>
	</div>

	<div class="flex flex-wrap items-center gap-2" data-slot="account-actions">
		<Button
			type="button"
			size="sm"
			variant="default"
			aria-pressed={activeForm === 'pay'}
			class={cn(activeForm === 'pay' && 'ring-2 ring-ring')}
			onclick={() => toggleForm('pay')}
		>
			Pay supplier
		</Button>
		<Button
			type="button"
			size="sm"
			variant={activeForm === 'refund' ? 'secondary' : 'outline'}
			aria-pressed={activeForm === 'refund'}
			class={cn(activeForm === 'refund' && 'border-primary')}
			onclick={() => toggleForm('refund')}
		>
			Refund credit
		</Button>
		<Button
			type="button"
			size="sm"
			variant={activeForm === 'adjust' ? 'secondary' : 'outline'}
			aria-pressed={activeForm === 'adjust'}
			class={cn(activeForm === 'adjust' && 'border-primary')}
			onclick={() => toggleForm('adjust')}
		>
			Manual adjustment
		</Button>
	</div>

	{#if loadError}
		<p class="text-sm text-destructive" role="alert">{loadError}</p>
	{/if}

	{#if lastOutcome}
		<div
			class="rounded-md border border-primary/40 bg-primary/10 px-3 py-2 text-xs"
			role="status"
		>
			<strong>{lastOutcome.heading}</strong>
			Balance: <span class="tabular-nums font-medium">{lastOutcome.balance}</span>
			· Available credit:
			<span class="tabular-nums font-medium">{lastOutcome.unapplied_credit}</span>.
		</div>
	{/if}

	{#if activeForm === 'pay'}
		<form
			class="grid gap-3 rounded-lg border border-border bg-muted/20 p-3.5"
			onsubmit={submitPayment}
		>
			<div class="flex items-center justify-between gap-2">
				<h3 class="text-sm font-semibold">Pay supplier</h3>
				<Button
					type="button"
					variant="ghost"
					size="xs"
					disabled={submitting}
					onclick={() => (activeForm = null)}
				>
					Close
				</Button>
			</div>

			<div class="grid gap-3 sm:grid-cols-2">
				<div class="grid gap-1.5">
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

				<div class="grid gap-1.5">
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
			</div>

			<div class="grid gap-1.5">
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
								size="xs"
								disabled={submitting || amountCents === null || amountCents <= 0n}
								onclick={prefillOldestFirst}
							>
								Oldest first
							</Button>
							<Button
								type="button"
								variant="ghost"
								size="xs"
								disabled={submitting}
								onclick={clearAllocations}
							>
								Clear
							</Button>
						</div>
					</div>

					<div class="grid gap-2 rounded-md border border-border bg-background/40 p-2.5">
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
				<Button type="submit" size="sm" disabled={submitting || !selectedMethod || !amount.trim()}>
					{submitting ? 'Recording…' : 'Record payment'}
				</Button>
			</div>
		</form>
	{:else if activeForm === 'refund'}
		<form
			class="grid gap-3 rounded-lg border border-border bg-muted/20 p-3.5"
			onsubmit={submitCreditRefund}
		>
			<div class="flex items-center justify-between gap-2">
				<h3 class="text-sm font-semibold">Refund credit</h3>
				<Button
					type="button"
					variant="ghost"
					size="xs"
					disabled={submitting}
					onclick={() => (activeForm = null)}
				>
					Close
				</Button>
			</div>

			{#if account}
				<div
					class="flex items-center justify-between gap-2 rounded-md border border-border bg-background/40 px-3 py-2 text-xs"
				>
					<div>
						<span class="text-muted-foreground">Refundable credit:</span>
						<span class="ml-1 font-medium tabular-nums">{formatCents(refundableCents)}</span>
					</div>
					{#if refundableCents > 0n}
						<Button
							type="button"
							variant="outline"
							size="xs"
							disabled={submitting}
							onclick={fillMaxRefund}
						>
							Use full credit
						</Button>
					{/if}
				</div>
				{#if refundableCents === 0n}
					<p class="text-xs text-muted-foreground">
						No refundable credit available right now. Refunds require unapplied credit and a
						negative balance.
					</p>
				{/if}
			{/if}

			<div class="grid gap-3 sm:grid-cols-2">
				<div class="grid gap-1.5">
					<Label for="supplier-refund-method">Payment method</Label>
					{#if methods.length === 0}
						<p class="text-xs text-destructive">
							No active payment methods.
							<a href="/settings/payments" class="text-primary underline">Add one in settings</a>
							first.
						</p>
					{:else}
						<Select.Root type="single" bind:value={refundPaymentMethodId}>
							<Select.Trigger id="supplier-refund-method" class="w-full" disabled={submitting}>
								{selectedRefundMethod?.name ?? 'Choose a payment method'}
							</Select.Trigger>
							<Select.Content>
								{#each methods as method (method.id)}
									<Select.Item value={method.id} label={method.name}>
										{method.name}
										<span class="text-xs text-muted-foreground">→ {method.wallet_name}</span>
									</Select.Item>
								{/each}
							</Select.Content>
						</Select.Root>
					{/if}
				</div>

				<div class="grid gap-1.5">
					<Label for="supplier-refund-amount">Amount</Label>
					<Input
						id="supplier-refund-amount"
						type="text"
						inputmode="decimal"
						placeholder="0.00"
						bind:value={refundAmount}
						required
						disabled={submitting}
						class="tabular-nums"
					/>
					{#if account && refundAmountCents !== null && refundAmountCents > refundableCents}
						<p class="text-xs text-destructive tabular-nums">
							Exceeds refundable credit ({formatCents(refundableCents)}).
						</p>
					{/if}
				</div>
			</div>

			<div class="grid gap-1.5">
				<Label for="supplier-refund-note">Note</Label>
				<Input
					id="supplier-refund-note"
					type="text"
					placeholder="Reason for supplier credit refund"
					bind:value={refundNote}
					required
					maxlength={240}
					disabled={submitting}
				/>
			</div>

			{#if refundError}
				<p class="text-sm text-destructive" role="alert">{refundError}</p>
			{/if}

			<div class="flex justify-end">
				<Button
					type="submit"
					size="sm"
					disabled={submitting ||
						!selectedRefundMethod ||
						!refundAmount.trim() ||
						!refundNote.trim()}
				>
					{submitting ? 'Recording…' : 'Record credit refund'}
				</Button>
			</div>
		</form>
	{:else if activeForm === 'adjust'}
		<form
			class="grid gap-3 rounded-lg border border-border bg-muted/20 p-3.5"
			onsubmit={submitAdjustment}
		>
			<div class="flex items-center justify-between gap-2">
				<h3 class="text-sm font-semibold">Manual adjustment</h3>
				<Button
					type="button"
					variant="ghost"
					size="xs"
					disabled={submitting}
					onclick={() => (activeForm = null)}
				>
					Close
				</Button>
			</div>

			<p class="text-xs text-muted-foreground">
				Adjusts the supplier payables ledger directly without touching any wallet. Balance must
				remain 0.00 or above.
			</p>

			<div class="grid gap-3 sm:grid-cols-2">
				<div class="grid gap-1.5">
					<Label for="supplier-adjustment-direction">Direction</Label>
					<Select.Root type="single" bind:value={adjustmentDirection}>
						<Select.Trigger
							id="supplier-adjustment-direction"
							class="w-full"
							disabled={submitting}
						>
							{ADJUSTMENT_DIRECTION_LABELS[adjustmentDirection]}
						</Select.Trigger>
						<Select.Content>
							<Select.Item value="in" label={ADJUSTMENT_DIRECTION_LABELS.in}>
								{ADJUSTMENT_DIRECTION_LABELS.in}
							</Select.Item>
							<Select.Item value="out" label={ADJUSTMENT_DIRECTION_LABELS.out}>
								{ADJUSTMENT_DIRECTION_LABELS.out}
							</Select.Item>
						</Select.Content>
					</Select.Root>
				</div>

				<div class="grid gap-1.5">
					<Label for="supplier-adjustment-amount">Amount</Label>
					<Input
						id="supplier-adjustment-amount"
						type="text"
						inputmode="decimal"
						placeholder="0.00"
						bind:value={adjustmentAmount}
						required
						disabled={submitting}
						class="tabular-nums"
					/>
				</div>
			</div>

			<div class="grid gap-1.5">
				<Label for="supplier-adjustment-note">Note</Label>
				<Input
					id="supplier-adjustment-note"
					type="text"
					placeholder="Reason for manual adjustment"
					bind:value={adjustmentNote}
					required
					maxlength={240}
					disabled={submitting}
				/>
			</div>

			{#if adjustmentError}
				<p class="text-sm text-destructive" role="alert">{adjustmentError}</p>
			{/if}

			<div class="flex justify-end">
				<Button
					type="submit"
					size="sm"
					disabled={submitting || !adjustmentAmount.trim() || !adjustmentNote.trim()}
				>
					{submitting ? 'Recording…' : 'Record adjustment'}
				</Button>
			</div>
		</form>
	{/if}
</section>

<!-- Unified balance ledger: purchases (+ nested returns), payments, refunds, adjustments -->
<section class="grid gap-3 px-4 pb-6" data-slot="balance-ledger">
	<div class="flex flex-wrap items-center justify-between gap-2">
		<h3 class="text-xs font-semibold uppercase tracking-wide text-muted-foreground">
			Balance ledger
			{#if account}
				({account.entries_total})
			{/if}
		</h3>
		{#if account && account.outstanding_purchases.length > 0}
			<span class="text-xs text-muted-foreground tabular-nums">
				{account.outstanding_purchases.length} open
				{account.outstanding_purchases.length === 1 ? 'purchase' : 'purchases'} ·
				<strong class="font-medium text-foreground">{account.outstanding_total}</strong> due
			</span>
		{/if}
	</div>

	{#if loading && !account}
		<p class="py-4 text-center text-sm text-muted-foreground">Loading account…</p>
	{:else if account && ledgerItems.length === 0}
		<div class="rounded-lg border border-border p-6 text-center text-sm text-muted-foreground">
			No account movements yet.
		</div>
	{:else if account}
		<div class="grid gap-2">
			{#each ledgerItems as item (item.entry.id)}
				<div class="overflow-hidden rounded-lg border border-border bg-muted/20">
					<button
						type="button"
						class="flex w-full cursor-pointer items-start justify-between gap-3 px-3.5 py-2.5 text-left transition-colors hover:bg-muted/50 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring"
						onclick={() => {
							selectedDoc = {
								party: 'supplier',
								supplierId,
								entry: item.entry,
								openPurchase: item.openPurchase
							};
						}}
					>
						<div class="min-w-0 grid gap-1">
							{#if item.entry.kind === 'purchase_charge' && item.entry.purchase_id}
								<div class="flex flex-wrap items-center gap-1.5">
									<span class="font-mono text-xs font-medium" title={item.entry.purchase_id}>
										Purchase #{item.entry.purchase_id.slice(0, 8)}
									</span>
									{#if item.openPurchase}
										{#if isPartiallyPaid(item.openPurchase)}
											<Badge
												variant="outline"
												class="border-warn/40 bg-warn/10 text-[11px] text-warn"
											>
												Partially paid
											</Badge>
										{:else}
											<Badge variant="secondary" class="text-[11px]">Unpaid</Badge>
										{/if}
									{:else}
										<Badge variant="outline" class="text-[11px] text-muted-foreground">
											Settled
										</Badge>
									{/if}
								</div>
								<div class="text-xs text-muted-foreground tabular-nums">
									{when(item.entry.created_at)}
									{#if item.openPurchase}
										· Due
										<span class="font-medium text-foreground">{item.openPurchase.outstanding}</span>
										{#if isNonZero(item.openPurchase.allocated)}
											· Paid {item.openPurchase.allocated}
										{/if}
										{#if isNonZero(item.openPurchase.cancelled)}
											· Returned {item.openPurchase.cancelled}
										{/if}
									{/if}
								</div>
							{:else}
								<div class="flex flex-wrap items-center gap-1.5">
									<Badge variant="outline" class="text-[11px]">
										{ENTRY_LABELS[item.entry.kind]}
									</Badge>
									<span class="text-xs text-muted-foreground">
										{when(item.entry.created_at)}
									</span>
								</div>
								{#if item.entry.note}
									<div class="text-xs text-muted-foreground">{item.entry.note}</div>
								{/if}
							{/if}
						</div>

						<div class="flex shrink-0 items-center gap-2">
							<div class="text-right tabular-nums">
								<div
									class={cn(
										'text-sm font-medium',
										item.entry.amount.startsWith('-') && 'text-primary'
									)}
								>
									{formatSigned(item.entry.amount)}
								</div>
								<div class="text-[11px] text-muted-foreground">
									Bal {item.entry.running_balance}
								</div>
							</div>
							<ChevronRightIcon class="size-4 text-muted-foreground/70" aria-hidden="true" />
						</div>
					</button>

					{#if item.purchaseReturns.length > 0}
						<div class="grid border-t border-border bg-background/30">
							{#each item.purchaseReturns as retEntry (retEntry.id)}
								<button
									type="button"
									class="flex w-full cursor-pointer items-start justify-between gap-3 border-t border-border/60 px-3.5 py-2 text-left text-xs transition-colors first:border-t-0 hover:bg-muted/40 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring"
									onclick={() => {
										selectedDoc = {
											party: 'supplier',
											supplierId,
											entry: retEntry,
											openPurchase: item.openPurchase
										};
									}}
								>
									<div class="flex min-w-0 flex-wrap items-center gap-1.5">
										<span class="font-mono text-muted-foreground" aria-hidden="true">↳</span>
										<Badge variant="outline" class="text-[10px]">Purchase return</Badge>
										{#if retEntry.purchase_return_id}
											<span
												class="font-mono text-muted-foreground"
												title={retEntry.purchase_return_id}
											>
												#{retEntry.purchase_return_id.slice(0, 8)}
											</span>
										{/if}
										<span class="text-muted-foreground">{when(retEntry.created_at)}</span>
										{#if retEntry.note}
											<span class="truncate text-muted-foreground">· {retEntry.note}</span>
										{/if}
									</div>
									<div class="flex shrink-0 items-center gap-1.5">
										<div class="text-right tabular-nums">
											<span class="font-medium text-primary">{retEntry.amount}</span>
											<span class="ml-1 text-[11px] text-muted-foreground">
												· Bal {retEntry.running_balance}
											</span>
										</div>
										<ChevronRightIcon class="size-3.5 text-muted-foreground/70" aria-hidden="true" />
									</div>
								</button>
							{/each}
						</div>
					{/if}
				</div>
			{/each}
		</div>

		{#if statementLastPage > 1}
			<div class="flex items-center justify-between gap-2 pt-1 text-xs text-muted-foreground">
				<span class="tabular-nums">{statementRange}</span>
				<div class="flex items-center gap-1.5">
					<Button
						variant="outline"
						size="sm"
						disabled={loading || statementPage <= 1}
						onclick={() => loadAccount(supplierId, statementPage - 1)}>Older</Button
					>
					<span class="tabular-nums">Page {statementPage} of {statementLastPage}</span>
					<Button
						variant="outline"
						size="sm"
						disabled={loading || statementPage >= statementLastPage}
						onclick={() => loadAccount(supplierId, statementPage + 1)}>Newer</Button
					>
				</div>
			</div>
		{/if}
	{/if}
</section>

<AccountDocumentDialog target={selectedDoc} onClose={() => (selectedDoc = null)} />
