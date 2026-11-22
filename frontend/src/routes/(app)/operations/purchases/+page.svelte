<script lang="ts">
	import { untrack } from 'svelte';
	import { goto, invalidateAll } from '$app/navigation';
	import {
		api,
		ApiError,
		type CompletedPurchase,
		type Purchase,
		type PurchasePayment
	} from '$lib/api/client';
	import { formatCents, parseCents } from '$lib/money';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import {
		Sheet,
		SheetContent,
		SheetDescription,
		SheetHeader,
		SheetTitle
	} from '$lib/components/ui/sheet';
	import {
		Table,
		TableBody,
		TableCell,
		TableHead,
		TableHeader,
		TableRow
	} from '$lib/components/ui/table';

	let { data } = $props();

	let draft = $state<Purchase>(untrack(() => data.draft));
	let busy = $state(false);
	let error = $state<string | null>(null);
	let completedBanner = $state<CompletedPurchase | null>(null);

	// Add-item inputs
	let selectedProductId = $state('');
	let addQuantity = $state('1');
	let addUnitCost = $state('');

	// Inline line edit state
	let editingProductId = $state<string | null>(null);
	let editQuantity = $state('');
	let editUnitCost = $state('');

	// Payment legs for completion
	type PaymentRow = { key: number; paymentMethodId: string; amount: string };
	let nextPaymentKey = 1;
	let paymentRows = $state<PaymentRow[]>([{ key: 0, paymentMethodId: '', amount: '' }]);
	let completionNote = $state('');

	// History detail drawer
	let inspected = $state<Purchase | null>(null);

	// Keep local draft synchronized when navigation/invalidation refreshes data.draft.
	$effect(() => {
		draft = data.draft;
	});

	const totalCents = $derived(parseCents(draft.total) ?? 0n);
	const paidCents = $derived.by(() => {
		let sum = 0n;
		for (const row of paymentRows) {
			const c = parseCents(row.amount.trim());
			if (c !== null && c > 0n) sum += c;
		}
		return sum;
	});
	const remainingCents = $derived(totalCents - paidCents);

	function failure(err: unknown, fallback: string): string {
		if (err instanceof ApiError) {
			return err.problem?.detail ?? `The server rejected the request (HTTP ${err.status}).`;
		}
		return fallback;
	}

	function when(iso: string | null | undefined): string {
		if (!iso) return '—';
		const date = new Date(iso);
		return Number.isNaN(date.getTime())
			? iso
			: date.toLocaleString('en-US', { dateStyle: 'short', timeStyle: 'short' });
	}

	function purchasesUrl(params: { status?: string; page?: number }): string {
		const query = new URLSearchParams();
		const status = params.status ?? data.statusTab;
		if (status && status !== 'completed') query.set('status', status);
		const page = params.page ?? data.page;
		if (page > 1) query.set('page', String(page));
		const qs = query.toString();
		return qs ? `/operations/purchases?${qs}` : '/operations/purchases';
	}

	async function onSupplierChange(event: Event) {
		const value = (event.target as HTMLSelectElement).value;
		busy = true;
		error = null;
		try {
			await api.setPurchaseSupplier(draft.id, value || null);
			draft = await api.getPurchase(draft.id);
		} catch (err) {
			error = failure(err, 'Could not update the supplier.');
		} finally {
			busy = false;
		}
	}

	async function onAddItem(event: Event) {
		event.preventDefault();
		if (!selectedProductId) {
			error = 'Select a product first.';
			return;
		}
		busy = true;
		error = null;
		completedBanner = null;
		try {
			draft = await api.addPurchaseItem(
				draft.id,
				selectedProductId,
				addQuantity.trim(),
				addUnitCost.trim()
			);
			selectedProductId = '';
			addQuantity = '1';
			addUnitCost = '';
		} catch (err) {
			error = failure(err, 'Could not add the item.');
		} finally {
			busy = false;
		}
	}

	function startEditLine(productId: string, quantity: string, unitCost: string) {
		editingProductId = productId;
		editQuantity = quantity;
		editUnitCost = unitCost;
		error = null;
	}

	async function saveEditLine(productId: string) {
		busy = true;
		error = null;
		try {
			draft = await api.setPurchaseItem(
				draft.id,
				productId,
				editQuantity.trim(),
				editUnitCost.trim()
			);
			editingProductId = null;
		} catch (err) {
			error = failure(err, 'Could not update the item.');
		} finally {
			busy = false;
		}
	}

	async function removeLine(productId: string) {
		busy = true;
		error = null;
		try {
			draft = await api.removePurchaseItem(draft.id, productId);
			if (editingProductId === productId) editingProductId = null;
		} catch (err) {
			error = failure(err, 'Could not remove the item.');
		} finally {
			busy = false;
		}
	}

	function addPaymentRow() {
		const defaultAmount = remainingCents > 0n ? formatCents(remainingCents) : '';
		paymentRows = [
			...paymentRows,
			{ key: nextPaymentKey++, paymentMethodId: '', amount: defaultAmount }
		];
	}

	function removePaymentRow(key: number) {
		paymentRows = paymentRows.filter((row) => row.key !== key);
		if (paymentRows.length === 0) {
			paymentRows = [{ key: nextPaymentKey++, paymentMethodId: '', amount: '' }];
		}
	}

	function fillExactTotal() {
		const first = paymentRows[0];
		if (!first) {
			paymentRows = [{ key: nextPaymentKey++, paymentMethodId: '', amount: draft.total }];
		} else {
			paymentRows = [{ key: first.key, paymentMethodId: first.paymentMethodId, amount: draft.total }];
		}
	}

	async function completePurchase() {
		busy = true;
		error = null;
		try {
			const payments: PurchasePayment[] =
				totalCents === 0n
					? []
					: paymentRows
							.filter((row) => row.paymentMethodId !== '' || row.amount.trim() !== '')
							.map((row) => ({
								payment_method_id: row.paymentMethodId,
								amount: row.amount.trim()
							}));

			const result = await api.completePurchase(
				draft.id,
				payments,
				completionNote.trim() || undefined
			);
			completedBanner = result;
			completionNote = '';
			paymentRows = [{ key: nextPaymentKey++, paymentMethodId: '', amount: '' }];
			editingProductId = null;
			await invalidateAll();
		} catch (err) {
			error = failure(err, 'Could not complete the purchase.');
		} finally {
			busy = false;
		}
	}

	async function cancelDraft() {
		busy = true;
		error = null;
		try {
			await api.cancelPurchase(draft.id);
			completionNote = '';
			paymentRows = [{ key: nextPaymentKey++, paymentMethodId: '', amount: '' }];
			editingProductId = null;
			await invalidateAll();
		} catch (err) {
			error = failure(err, 'Could not cancel the draft.');
		} finally {
			busy = false;
		}
	}

	let historyCounter = $derived.by(() => {
		if (data.total === 0) return '0 purchases';
		const from = data.offset + 1;
		const to = data.offset + data.historyItems.length;
		const noun = data.total === 1 ? 'purchase' : 'purchases';
		return `${from}–${to} of ${data.total} ${noun}`;
	});
</script>

<div class="mb-6 flex items-center justify-between gap-4">
	<div>
		<h1 class="text-xl font-semibold tracking-tight">Purchases</h1>
		<p class="mt-1 text-sm text-muted-foreground">
			Build a draft purchase from a supplier, then confirm to enter stock and deduct wallets.
		</p>
	</div>
</div>

{#if completedBanner}
	<div class="mb-4 rounded-lg border border-primary/40 bg-primary/5 px-4 py-3 text-sm">
		<div class="font-medium">
			Purchase {completedBanner.purchase_id.slice(0, 8)} completed — total {completedBanner.total}
		</div>
		<div class="text-xs text-muted-foreground">
			{completedBanner.movement_ids.length} stock
			{completedBanner.movement_ids.length === 1 ? 'entry' : 'entries'} and
			{completedBanner.payment_movement_ids.length} wallet
			{completedBanner.payment_movement_ids.length === 1 ? 'payment' : 'payments'} recorded.
		</div>
	</div>
{/if}

<!-- Active Draft Workspace -->
<section class="mb-8 rounded-lg border border-border p-4">
	<div class="mb-4 flex flex-wrap items-center justify-between gap-3 border-b border-border pb-3">
		<div class="flex items-center gap-2">
			<h2 class="text-base font-semibold">Open draft</h2>
			<span class="font-mono text-xs text-muted-foreground" title={draft.id}>
				#{draft.id.slice(0, 8)}
			</span>
			<Badge variant="secondary">
				{draft.item_count}
				{draft.item_count === 1 ? 'item' : 'items'}
			</Badge>
		</div>
		<div class="flex items-center gap-3">
			<div class="text-right">
				<div class="text-xs text-muted-foreground">Server total</div>
				<div class="text-lg font-semibold tabular-nums">{draft.total}</div>
			</div>
			<Button variant="outline" size="sm" disabled={busy} onclick={cancelDraft}>
				Cancel draft
			</Button>
		</div>
	</div>

	<!-- Supplier selection -->
	<div class="mb-4 grid gap-2 sm:max-w-md">
		<Label for="draft-supplier">Supplier</Label>
		<select
			id="draft-supplier"
			class="flex h-9 w-full rounded-md border border-input bg-background px-3 py-1 text-sm shadow-xs focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring"
			value={draft.supplier_id ?? ''}
			disabled={busy}
			onchange={onSupplierChange}
		>
			<option value="">Select a supplier…</option>
			{#if draft.supplier_id && !data.suppliers.some((s) => s.id === draft.supplier_id)}
				<option value={draft.supplier_id}>
					{draft.supplier_name ?? 'Inactive supplier'} (inactive)
				</option>
			{/if}
			{#each data.suppliers as s (s.id)}
				<option value={s.id}>
					{s.name}{s.tax_id ? ` (${s.tax_id})` : ''}
				</option>
			{/each}
		</select>
		{#if data.suppliers.length === 0}
			<p class="text-xs text-muted-foreground">
				No active suppliers yet. <a href="/suppliers?new=1" class="underline">Create a supplier</a> first.
			</p>
		{/if}
	</div>

	<!-- Add item form -->
	<form class="mb-4 grid gap-3 sm:grid-cols-[1fr_7rem_9rem_auto] sm:items-end" onsubmit={onAddItem}>
		<div class="grid gap-1.5">
			<Label for="add-product">Product</Label>
			<select
				id="add-product"
				class="flex h-9 w-full rounded-md border border-input bg-background px-3 py-1 text-sm shadow-xs focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring"
				bind:value={selectedProductId}
				disabled={busy}
			>
				<option value="">Select a physical product…</option>
				{#each data.products as p (p.id)}
					<option value={p.id}>
						{p.name}{p.sku ? ` · ${p.sku}` : ''}
					</option>
				{/each}
			</select>
		</div>

		<div class="grid gap-1.5">
			<Label for="add-qty">Quantity</Label>
			<Input
				id="add-qty"
				type="text"
				inputmode="decimal"
				bind:value={addQuantity}
				placeholder="1"
				class="tabular-nums"
				required
			/>
		</div>

		<div class="grid gap-1.5">
			<Label for="add-cost">Unit cost</Label>
			<Input
				id="add-cost"
				type="text"
				inputmode="decimal"
				bind:value={addUnitCost}
				placeholder="0.00"
				class="tabular-nums"
				required
			/>
		</div>

		<Button type="submit" size="sm" class="h-9" disabled={busy}>Add item</Button>
	</form>

	<!-- Draft lines table -->
	{#if draft.items.length === 0}
		<div class="mb-4 rounded-md border border-dashed border-border p-6 text-center text-sm text-muted-foreground">
			No items in this draft yet. Add physical products with their quantity and unit cost above.
		</div>
	{:else}
		<div class="mb-4 overflow-hidden rounded-md border border-border">
			<Table>
				<TableHeader>
					<TableRow>
						<TableHead>Product</TableHead>
						<TableHead class="text-right">Quantity</TableHead>
						<TableHead class="text-right">Unit cost</TableHead>
						<TableHead class="text-right">Subtotal</TableHead>
						<TableHead class="text-right">Actions</TableHead>
					</TableRow>
				</TableHeader>
				<TableBody>
					{#each draft.items as item (item.product_id)}
						<TableRow>
							<TableCell class="font-medium">{item.product_name}</TableCell>
							{#if editingProductId === item.product_id}
								<TableCell class="text-right">
									<Input
										type="text"
										inputmode="decimal"
										bind:value={editQuantity}
										class="ml-auto h-8 w-24 text-right tabular-nums"
									/>
								</TableCell>
								<TableCell class="text-right">
									<Input
										type="text"
										inputmode="decimal"
										bind:value={editUnitCost}
										class="ml-auto h-8 w-28 text-right tabular-nums"
									/>
								</TableCell>
								<TableCell class="text-right tabular-nums text-muted-foreground">
									{item.subtotal}
								</TableCell>
								<TableCell class="text-right">
									<div class="flex justify-end gap-1">
										<Button
											size="sm"
											variant="outline"
											disabled={busy}
											onclick={() => saveEditLine(item.product_id)}>Save</Button
										>
										<Button
											size="sm"
											variant="ghost"
											onclick={() => (editingProductId = null)}>Cancel</Button
										>
									</div>
								</TableCell>
							{:else}
								<TableCell class="text-right tabular-nums">{item.quantity}</TableCell>
								<TableCell class="text-right tabular-nums">{item.unit_cost}</TableCell>
								<TableCell class="text-right font-medium tabular-nums">
									{item.subtotal}
								</TableCell>
								<TableCell class="text-right">
									<div class="flex justify-end gap-1">
										<Button
											size="sm"
											variant="ghost"
											disabled={busy}
											onclick={() =>
												startEditLine(item.product_id, item.quantity, item.unit_cost)}
											>Edit</Button
										>
										<Button
											size="sm"
											variant="ghost"
											disabled={busy}
											onclick={() => removeLine(item.product_id)}>Remove</Button
										>
									</div>
								</TableCell>
							{/if}
						</TableRow>
					{/each}
				</TableBody>
			</Table>
		</div>
	{/if}

	<!-- Payment & confirmation -->
	<div class="border-t border-border pt-4">
		<div class="mb-2 flex items-center justify-between gap-2">
			<h3 class="text-sm font-semibold">Cash payment</h3>
			{#if totalCents > 0n}
				<div class="flex items-center gap-2">
					<Button type="button" variant="ghost" size="sm" onclick={fillExactTotal}>
						Fill exact total ({draft.total})
					</Button>
					<Button type="button" variant="outline" size="sm" onclick={addPaymentRow}>
						+ Split payment
					</Button>
				</div>
			{/if}
		</div>

		{#if totalCents === 0n}
			<p class="mb-3 text-xs text-muted-foreground">
				Total is 0.00 — this purchase completes without writing wallet movements.
			</p>
		{:else}
			<div class="mb-3 grid gap-2">
				{#each paymentRows as row (row.key)}
					<div class="grid gap-2 sm:grid-cols-[1fr_10rem_auto] sm:items-center">
						<select
							aria-label="Payment method"
							class="flex h-9 w-full rounded-md border border-input bg-background px-3 py-1 text-sm shadow-xs focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-ring"
							bind:value={row.paymentMethodId}
							disabled={busy}
						>
							<option value="">Select payment method…</option>
							{#each data.methods as m (m.id)}
								<option value={m.id}>{m.name} ({m.wallet_name})</option>
							{/each}
						</select>

						<Input
							type="text"
							inputmode="decimal"
							placeholder="0.00"
							bind:value={row.amount}
							class="tabular-nums"
							disabled={busy}
						/>

						{#if paymentRows.length > 1}
							<Button
								type="button"
								variant="ghost"
								size="sm"
								onclick={() => removePaymentRow(row.key)}>Remove</Button
							>
						{/if}
					</div>
				{/each}
			</div>

			<p class="mb-3 text-xs text-muted-foreground tabular-nums">
				Covered: {formatCents(paidCents)} / {draft.total}
				{#if remainingCents !== 0n}
					· Remaining: {formatCents(remainingCents)}
				{/if}
			</p>
		{/if}

		<div class="flex flex-wrap items-end justify-between gap-3">
			<div class="grid grow gap-1.5 sm:max-w-md">
				<Label for="purchase-note">Note (optional)</Label>
				<Input
					id="purchase-note"
					bind:value={completionNote}
					placeholder="Invoice number or delivery reference"
					disabled={busy}
				/>
			</div>

			<Button
				type="button"
				disabled={busy || draft.items.length === 0 || !draft.supplier_id}
				onclick={completePurchase}
			>
				{busy ? 'Processing…' : `Complete purchase (${draft.total})`}
			</Button>
		</div>

		{#if error}
			<p class="mt-3 text-sm text-destructive">{error}</p>
		{/if}
	</div>
</section>

<!-- History Section -->
<section>
	<div class="mb-3 flex flex-wrap items-center justify-between gap-3">
		<h2 class="text-base font-semibold">History</h2>
		<div class="flex items-center gap-1">
			{#each [
				{ id: 'completed', label: 'Completed' },
				{ id: 'cancelled', label: 'Cancelled' },
				{ id: 'all', label: 'All' }
			] as tab (tab.id)}
				<Button
					size="sm"
					variant={data.statusTab === tab.id ? 'default' : 'outline'}
					onclick={() => goto(purchasesUrl({ status: tab.id, page: 1 }))}
				>
					{tab.label}
				</Button>
			{/each}
		</div>
	</div>

	{#if data.historyItems.length === 0}
		<div class="rounded-lg border border-border p-10 text-center text-sm text-muted-foreground">
			No purchases match this filter yet.
		</div>
	{:else}
		<div class="overflow-hidden rounded-lg border border-border">
			<Table>
				<TableHeader>
					<TableRow>
						<TableHead>When</TableHead>
						<TableHead>Purchase</TableHead>
						<TableHead>Supplier</TableHead>
						<TableHead>Items</TableHead>
						<TableHead>Status</TableHead>
						<TableHead class="text-right">Total</TableHead>
					</TableRow>
				</TableHeader>
				<TableBody>
					{#each data.historyItems as purchase (purchase.id)}
						<TableRow class="cursor-pointer" onclick={() => (inspected = purchase)}>
							<TableCell class="text-muted-foreground">
								{when(purchase.completed_at ?? purchase.created_at)}
							</TableCell>
							<TableCell class="font-mono text-xs" title={purchase.id}>
								<button type="button" class="text-left">{purchase.id.slice(0, 8)}</button>
							</TableCell>
							<TableCell>{purchase.supplier_name ?? '—'}</TableCell>
							<TableCell>
								<Badge variant="secondary">
									{purchase.item_count}
									{purchase.item_count === 1 ? 'item' : 'items'}
								</Badge>
							</TableCell>
							<TableCell>
								<Badge variant={purchase.status === 'completed' ? 'outline' : 'secondary'}>
									{purchase.status}
								</Badge>
							</TableCell>
							<TableCell class="text-right font-medium tabular-nums">
								{purchase.total}
							</TableCell>
						</TableRow>
					{/each}
				</TableBody>
			</Table>
		</div>

		<div class="mt-3 flex items-center justify-between gap-4 text-sm text-muted-foreground">
			<span>{historyCounter}</span>

			{#if data.lastPage > 1}
				<div class="flex items-center gap-2">
					<Button
						variant="outline"
						size="sm"
						disabled={data.page <= 1}
						onclick={() => goto(purchasesUrl({ page: data.page - 1 }))}>Previous</Button
					>
					<span class="tabular-nums">Page {data.page} of {data.lastPage}</span>
					<Button
						variant="outline"
						size="sm"
						disabled={data.page >= data.lastPage}
						onclick={() => goto(purchasesUrl({ page: data.page + 1 }))}>Next</Button
					>
				</div>
			{/if}
		</div>
	{/if}
</section>

<!-- Detail Sheet -->
<Sheet open={inspected !== null} onOpenChange={(o) => !o && (inspected = null)}>
	<SheetContent side="right" class="overflow-y-auto sm:max-w-xl">
		{#if inspected}
			<SheetHeader>
				<SheetTitle>Purchase #{inspected.id.slice(0, 8)}</SheetTitle>
				<SheetDescription>
					{inspected.supplier_name ?? 'No supplier'} · {inspected.status}
				</SheetDescription>
			</SheetHeader>

			<dl class="grid grid-cols-[7rem_1fr] gap-x-4 gap-y-2 px-4 text-sm">
				<dt class="text-muted-foreground">Supplier</dt>
				<dd>{inspected.supplier_name ?? '—'}</dd>
				<dt class="text-muted-foreground">Status</dt>
				<dd>
					<Badge variant={inspected.status === 'completed' ? 'outline' : 'secondary'}>
						{inspected.status}
					</Badge>
				</dd>
				<dt class="text-muted-foreground">Total</dt>
				<dd class="font-semibold tabular-nums">{inspected.total}</dd>
				<dt class="text-muted-foreground">Note</dt>
				<dd>{inspected.note ?? '—'}</dd>
				<dt class="text-muted-foreground">Created</dt>
				<dd class="text-muted-foreground">{when(inspected.created_at)}</dd>
				{#if inspected.completed_at}
					<dt class="text-muted-foreground">Completed</dt>
					<dd class="text-muted-foreground">{when(inspected.completed_at)}</dd>
				{/if}
			</dl>

			<div class="px-4 pt-2">
				<h3 class="mb-2 text-sm font-semibold">Items</h3>
				{#if inspected.items.length === 0}
					<p class="text-sm text-muted-foreground">No items.</p>
				{:else}
					<div class="overflow-hidden rounded-md border border-border">
						<Table>
							<TableHeader>
								<TableRow>
									<TableHead>Product</TableHead>
									<TableHead class="text-right">Qty</TableHead>
									<TableHead class="text-right">Unit cost</TableHead>
									<TableHead class="text-right">Subtotal</TableHead>
								</TableRow>
							</TableHeader>
							<TableBody>
								{#each inspected.items as item (item.product_id)}
									<TableRow>
										<TableCell>{item.product_name}</TableCell>
										<TableCell class="text-right tabular-nums">{item.quantity}</TableCell>
										<TableCell class="text-right tabular-nums">{item.unit_cost}</TableCell>
										<TableCell class="text-right font-medium tabular-nums">
											{item.subtotal}
										</TableCell>
									</TableRow>
								{/each}
							</TableBody>
						</Table>
					</div>
				{/if}
			</div>
		{/if}
	</SheetContent>
</Sheet>
