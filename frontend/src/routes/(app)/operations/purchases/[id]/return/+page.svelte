<script lang="ts">
	import { untrack } from 'svelte';
	import { invalidateAll } from '$app/navigation';
	import {
		api,
		ApiError,
		type PurchaseReturn,
		type SurplusMode
	} from '$lib/api/client';
	import { formatCents, maxCents, minCents, parseCents } from '$lib/money';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import {
		Table,
		TableBody,
		TableCell,
		TableHead,
		TableHeader,
		TableRow
	} from '$lib/components/ui/table';

	let { data } = $props();

	const summary = $derived(data.summary);

	// Quantities are local input state; they start at zero and reset after a
	// successful return.
	let quantities = $state<Record<string, number>>(
		untrack(() =>
			Object.fromEntries((data.summary?.items ?? []).map((line) => [line.product_id, 0]))
		)
	);
	let surplusMode = $state<SurplusMode>('wallet_refund');
	let reason = $state('');
	let error = $state<string | null>(null);
	let refreshNotice = $state<string | null>(null);
	let busy = $state(false);
	let created = $state<PurchaseReturn | null>(null);

	// One idempotency key per intended return, kept across failures and cleared
	// only after the POST succeeds so a retry replays instead of double-writing.
	let idempotencyKey: string | null = null;

	const isCompleted = $derived(summary?.purchase_status === 'completed');

	const selectedUnits = $derived(
		Object.values(quantities).reduce(
			(sum, quantity) => sum + (Number.isFinite(quantity) ? quantity : 0),
			0
		)
	);

	// Preview of estimated return total, debt cancellation, and paid surplus.
	// The server always remains the authoritative calculator.
	const preview = $derived.by(() => {
		if (!summary) {
			return { totalCents: 0n, debtCents: 0n, surplusCents: 0n, walletCapCents: 0n };
		}
		let totalFloat = 0;
		for (const line of summary.items) {
			const qty = quantities[line.product_id] ?? 0;
			if (!Number.isFinite(qty) || qty <= 0) continue;
			const costCents = parseCents(line.unit_cost) ?? 0n;
			totalFloat += qty * Number(costCents);
		}
		const totalCents = BigInt(Math.round(totalFloat));
		const outstandingCents = maxCents(parseCents(summary.purchase_outstanding) ?? 0n, 0n);
		const debtCents = minCents(totalCents, outstandingCents);
		const surplusCents = maxCents(totalCents - debtCents, 0n);
		const walletCapCents = maxCents(parseCents(summary.wallet_refundable_capacity) ?? 0n, 0n);
		return { totalCents, debtCents, surplusCents, walletCapCents };
	});

	const walletRefundExceedsCapacity = $derived(
		surplusMode === 'wallet_refund' &&
			preview.surplusCents > 0n &&
			preview.surplusCents > preview.walletCapCents
	);

	const canSubmit = $derived(
		!busy && isCompleted && reason.trim().length > 0 && selectedUnits > 0
	);

	function failure(err: unknown, fallback: string): string {
		if (err instanceof ApiError) {
			return err.problem?.detail ?? `The server rejected the return (HTTP ${err.status}).`;
		}
		return fallback;
	}

	function when(iso: string): string {
		const date = new Date(iso);
		return Number.isNaN(date.getTime())
			? iso
			: date.toLocaleString('en-US', { dateStyle: 'short', timeStyle: 'short' });
	}

	function unitLabel(units: number): string {
		const count = new Intl.NumberFormat('en-US', { maximumFractionDigits: 3 }).format(units);
		return `${count} ${units === 1 ? 'unit' : 'units'}`;
	}

	function setQuantity(productId: string, raw: string): void {
		const parsed = Number(raw);
		// No clamping to the returnable maximum: the server owns that check and
		// its 409 detail is what the operator sees.
		quantities[productId] = Number.isFinite(parsed) ? Math.max(0, parsed) : 0;
	}

	/** Plain-HTTP-safe idempotency key generator (works on LAN/tailnet without HTTPS). */
	function newIdempotencyKey(): string {
		if (typeof crypto.randomUUID === 'function') return crypto.randomUUID();

		const bytes = new Uint8Array(16);
		if (typeof crypto.getRandomValues === 'function') {
			crypto.getRandomValues(bytes);
			return Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('');
		}

		return `${Date.now().toString(36)}-${Math.random().toString(36).slice(2)}`;
	}

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		if (!canSubmit || !summary) return;

		error = null;
		refreshNotice = null;
		busy = true;
		let ret: PurchaseReturn;
		try {
			idempotencyKey ??= newIdempotencyKey();
			const items = Object.entries(quantities)
				.filter(
					([productId, quantity]) =>
						summary.items.some((line) => line.product_id === productId) && quantity > 0
				)
				.map(([productId, quantity]) => ({ product_id: productId, quantity: String(quantity) }));

			ret = await api.createPurchaseReturn(data.purchaseId, {
				reason: reason.trim(),
				items,
				surplus_mode: surplusMode,
				idempotency_key: idempotencyKey
			});
		} catch (e) {
			error = failure(e, 'Could not issue the purchase return.');
			busy = false;
			return;
		}

		created = ret;
		idempotencyKey = null;
		reason = '';
		for (const productId of Object.keys(quantities)) quantities[productId] = 0;
		try {
			await invalidateAll();
		} catch {
			refreshNotice =
				'The return was recorded, but the summary could not be refreshed. Reload the page to see the updated returnable quantities and history.';
		} finally {
			busy = false;
		}
	}
</script>

{#if data.notFound || !summary}
	<div class="mb-4">
		<h1 class="text-xl font-semibold tracking-tight">Purchase return</h1>
	</div>
	<div
		class="rounded-lg border border-border p-10 text-center text-sm text-muted-foreground"
		role="status"
	>
		Purchase not found.
	</div>
	<div class="mt-4">
		<Button variant="outline" href="/operations/purchases">Back to purchases</Button>
	</div>
{:else if !isCompleted}
	<div class="mb-4 flex items-center justify-between gap-4">
		<div>
			<h1 class="text-xl font-semibold tracking-tight">Purchase return</h1>
			<p class="mt-1 text-sm text-muted-foreground">
				Purchase <span class="font-mono text-xs" title={data.purchaseId}>{data.purchaseId.slice(0, 8)}</span>
				({summary.purchase_status})
			</p>
		</div>
		<Button variant="outline" href="/operations/purchases">Back to purchases</Button>
	</div>
	<div
		class="rounded-lg border border-border p-10 text-center text-sm text-muted-foreground"
		role="status"
	>
		Only completed purchases can be returned to a supplier.
	</div>
{:else}
	<div class="mb-4 flex flex-wrap items-start justify-between gap-4">
		<div>
			<h1 class="text-xl font-semibold tracking-tight">Purchase return</h1>
			<p class="mt-1 text-sm text-muted-foreground">
				Purchase <span class="font-mono text-xs" title={data.purchaseId}>{data.purchaseId.slice(0, 8)}</span>
				{#if summary.supplier_name}
					· {summary.supplier_name}
				{/if}
				— return goods to the supplier using the purchase's frozen unit costs.
			</p>
		</div>
		<Button variant="outline" href="/operations/purchases">Back to purchases</Button>
	</div>

	{#if error}
		<p class="mb-4 text-sm text-destructive" role="alert">{error}</p>
	{/if}

	{#if refreshNotice}
		<p class="mb-4 text-sm text-muted-foreground" role="status">{refreshNotice}</p>
	{/if}

	{#if created}
		<div class="issued mb-6 rounded-lg border border-border p-4 text-sm" role="status">
			<strong>Purchase return recorded.</strong> Total {created.total}.
			<ul class="mt-2 space-y-0.5 text-muted-foreground">
				{#if Number(created.debt_cancelled) > 0}
					<li>Payable cancelled: {created.debt_cancelled}</li>
				{/if}
				{#if Number(created.credit_granted) > 0}
					<li>Supplier credit granted: {created.credit_granted}</li>
				{/if}
				{#each created.refunds as refund, i (refund.wallet_id + ':' + refund.payment_method_id + ':' + i)}
					<li>
						Wallet refund — {refund.wallet_name} ({refund.payment_method_name}): {refund.amount}
					</li>
				{/each}
			</ul>
		</div>
	{/if}

	<section class="mb-6 grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
		<div class="rounded-lg border border-border p-4">
			<div class="text-xs text-muted-foreground">Purchase total</div>
			<div class="mt-1 text-lg font-semibold tabular-nums">{summary.purchase_total}</div>
		</div>
		<div class="rounded-lg border border-border p-4">
			<div class="text-xs text-muted-foreground">Already returned</div>
			<div class="mt-1 text-lg font-semibold tabular-nums">{summary.returned_total}</div>
		</div>
		<div class="rounded-lg border border-border p-4">
			<div class="text-xs text-muted-foreground">Open payable</div>
			<div class="mt-1 text-lg font-semibold tabular-nums">{summary.purchase_outstanding}</div>
		</div>
		<div class="rounded-lg border border-border p-4">
			<div class="text-xs text-muted-foreground">Wallet refund capacity</div>
			<div class="mt-1 text-lg font-semibold tabular-nums">
				{summary.wallet_refundable_capacity}
			</div>
		</div>
	</section>

	<form onsubmit={submit} novalidate>
		<div class="mb-4 overflow-hidden rounded-lg border border-border">
			<Table>
				<TableHeader>
					<TableRow>
						<TableHead>Product</TableHead>
						<TableHead class="text-right">Unit cost</TableHead>
						<TableHead class="text-right">Purchased</TableHead>
						<TableHead class="text-right">Already returned</TableHead>
						<TableHead>Return now</TableHead>
					</TableRow>
				</TableHeader>
				<TableBody>
					{#each summary.items as line (line.product_id)}
						{@const fullyReturned = Number(line.returnable_quantity) === 0}
						<TableRow
							id={`return-line-${line.product_id}`}
							class={fullyReturned ? 'opacity-60' : ''}
						>
							<TableCell class="font-medium">{line.product_name}</TableCell>
							<TableCell class="text-right tabular-nums">{line.unit_cost}</TableCell>
							<TableCell class="text-right tabular-nums">{line.quantity}</TableCell>
							<TableCell class="text-right tabular-nums">{line.returned_quantity}</TableCell>
							<TableCell>
								<div class="flex flex-wrap items-center gap-2">
									<Input
										id={`return-qty-${line.product_id}`}
										type="number"
										min="0"
										max={line.returnable_quantity}
										step="any"
										value={quantities[line.product_id] ?? 0}
										disabled={busy || fullyReturned}
										class="w-24"
										aria-label={`Units to return of ${line.product_name}`}
										oninput={(event) => setQuantity(line.product_id, event.currentTarget.value)}
									/>
									{#if fullyReturned}
										<Badge variant="secondary">Fully returned</Badge>
									{:else}
										<span class="text-xs text-muted-foreground tabular-nums">
											{line.returnable_quantity} left
										</span>
									{/if}
								</div>
							</TableCell>
						</TableRow>
					{/each}
				</TableBody>
			</Table>
		</div>

		<fieldset class="mb-4 max-w-xl rounded-lg border border-border p-4">
			<legend class="px-1 text-sm font-medium">Paid surplus handling</legend>
			<p class="mb-3 text-xs text-muted-foreground">
				Open payable debt ({summary.purchase_outstanding}) is always cancelled first. Choose what to
				do with any already-paid remainder:
			</p>
			<div class="grid gap-2">
				<label
					class="flex cursor-pointer items-start gap-2.5 rounded-md border border-border p-2.5 text-sm"
				>
					<input
						id="surplus-mode-wallet"
						type="radio"
						name="surplus_mode"
						value="wallet_refund"
						bind:group={surplusMode}
						disabled={busy}
						class="mt-0.5"
					/>
					<span>
						<span class="font-medium">Refund to original wallets</span>
						<span class="block text-xs text-muted-foreground">
							Returns cash/bank inflows proportionally across the payment methods that paid this
							purchase (up to {summary.wallet_refundable_capacity}).
						</span>
					</span>
				</label>
				<label
					class="flex cursor-pointer items-start gap-2.5 rounded-md border border-border p-2.5 text-sm"
				>
					<input
						id="surplus-mode-credit"
						type="radio"
						name="surplus_mode"
						value="supplier_credit"
						bind:group={surplusMode}
						disabled={busy}
						class="mt-0.5"
					/>
					<span>
						<span class="font-medium">Keep as supplier credit</span>
						<span class="block text-xs text-muted-foreground">
							Leaves the paid remainder as unapplied credit on the supplier account for future
							on-account purchases.
						</span>
					</span>
				</label>
			</div>
			{#if walletRefundExceedsCapacity}
				<p class="mt-2 text-xs text-destructive" role="alert">
					Estimated paid surplus ({formatCents(preview.surplusCents)}) exceeds wallet refund
					capacity ({summary.wallet_refundable_capacity}). Choose "Keep as supplier credit" or
					reduce the returned quantity.
				</p>
			{/if}
		</fieldset>

		<div class="mb-4 grid max-w-xl gap-1">
			<Label for="return-reason">Reason</Label>
			<textarea
				id="return-reason"
				rows="3"
				maxlength="500"
				bind:value={reason}
				placeholder="Why are these goods being returned to the supplier?"
				class="flex min-h-20 w-full rounded-md border border-input bg-transparent px-2.5 py-2 text-sm shadow-xs outline-none placeholder:text-muted-foreground focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50"
			></textarea>
			<span id="reason-counter" class="text-xs text-muted-foreground tabular-nums">
				{reason.length}/500 characters
			</span>
		</div>

		<div id="return-summary" class="mb-4 rounded-md border border-border bg-muted/30 p-3 text-sm">
			Returning {unitLabel(selectedUnits)} — estimated total
			<span class="font-semibold tabular-nums">{formatCents(preview.totalCents)}</span>
			{#if preview.totalCents > 0n}
				(cancels <span class="tabular-nums">{formatCents(preview.debtCents)}</span> debt,
				<span class="tabular-nums">{formatCents(preview.surplusCents)}</span>
				{surplusMode === 'supplier_credit' ? 'as supplier credit' : 'to wallets'}).
			{/if}
		</div>

		<Button type="submit" disabled={!canSubmit}>
			{busy ? 'Recording…' : 'Record purchase return'}
		</Button>
	</form>

	<section class="mt-8">
		<div class="mb-3 flex items-center justify-between gap-4">
			<h2 class="text-lg font-semibold">Issued returns</h2>
			<span class="text-sm text-muted-foreground">
				{summary.returns.length}
				{summary.returns.length === 1 ? 'return' : 'returns'}
			</span>
		</div>

		{#if summary.returns.length === 0}
			<div class="rounded-lg border border-border p-10 text-center text-sm text-muted-foreground">
				No returns have been recorded for this purchase yet.
			</div>
		{:else}
			<div class="overflow-hidden rounded-lg border border-border">
				<Table id="purchase-returns-table">
					<TableHeader>
						<TableRow>
							<TableHead>When</TableHead>
							<TableHead>Reason</TableHead>
							<TableHead class="text-right">Total</TableHead>
							<TableHead>Lines</TableHead>
							<TableHead>Settlement</TableHead>
						</TableRow>
					</TableHeader>
					<TableBody>
						{#each summary.returns as ret (ret.id)}
							<TableRow>
								<TableCell class="text-muted-foreground">{when(ret.created_at)}</TableCell>
								<TableCell class="whitespace-normal">{ret.reason}</TableCell>
								<TableCell class="text-right font-medium tabular-nums">{ret.total}</TableCell>
								<TableCell class="whitespace-normal">
									<ul class="space-y-0.5 text-xs">
										{#each ret.items as item (item.product_id)}
											<li>
												{item.product_name}: {item.quantity} × {item.unit_cost} = {item.subtotal}
											</li>
										{/each}
									</ul>
								</TableCell>
								<TableCell class="whitespace-normal">
									<ul class="space-y-0.5 text-xs">
										{#if Number(ret.debt_cancelled) > 0}
											<li>Debt cancelled: {ret.debt_cancelled}</li>
										{/if}
										{#if Number(ret.credit_granted) > 0}
											<li>Supplier credit: {ret.credit_granted}</li>
										{/if}
										{#each ret.refunds as refund, i (refund.wallet_id + ':' + refund.payment_method_id + ':' + i)}
											<li>
												{refund.wallet_name} ({refund.payment_method_name}): {refund.amount}
											</li>
										{/each}
										{#if Number(ret.debt_cancelled) === 0 && Number(ret.credit_granted) === 0 && ret.refunds.length === 0}
											<li class="text-muted-foreground">Zero-cost return</li>
										{/if}
									</ul>
								</TableCell>
							</TableRow>
						{/each}
					</TableBody>
				</Table>
			</div>
		{/if}
	</section>
{/if}
