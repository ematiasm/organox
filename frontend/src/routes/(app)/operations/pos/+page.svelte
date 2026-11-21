<script lang="ts">
	import { untrack } from 'svelte';
	import { api, ApiError, type Customer, type Product, type Sale } from '$lib/api/client';
	import { formatCents, minCents, parseCents } from '$lib/money';
	import * as Select from '$lib/components/ui/select';

	let { data } = $props();

	// The server calculates the sale; this state only reflects its response.
	// The browser never adds or validates totals that may differ from the charge.
	// `data.sale` is copied to local state because mutations replace it without
	// navigation. A derived value would require a page reload for each response.
	let sale = $state<Sale>(untrack(() => data.sale));
	let selectedCustomer = $state<Customer | null>(untrack(() => data.selectedCustomer));
	let onAccountInput = $state('');
	let error = $state<string | null>(null);
	let busy = $state(false);
	let search = $state('');
	let scanInput: HTMLInputElement | undefined = $state();
	let completed = $state<{
		total: string;
		paid: string | null;
		method: string | null;
		destination: string | null;
		onAccount: string | null;
		creditApplied: string | null;
	} | null>(null);
	// Nothing is preselected: a charge must name a method explicitly.
	let paymentMethodId = $state('');

	// --- Customer picker ---
	//
	// The list loads lazily so the cart, the POS's hot path, stays as it was.
	// The picker searches the active registry server-side: a deactivated
	// customer never appears, and the one the cashier already loaded is still
	// clickable, which is how a refusal gets a chance to be shown.
	let pickerOpen = $state(false);
	let customerSearch = $state('');
	let customers = $state<Customer[]>([]);
	let pickerError = $state<string | null>(null);
	let customerTimer: ReturnType<typeof setTimeout> | undefined;

	// Client-side search covers loaded products, not the entire catalog.
	const visible = $derived(
		search.trim()
			? data.products.filter(
					(p) =>
						p.name.toLowerCase().includes(search.toLowerCase()) ||
						(p.sku ?? '').toLowerCase().includes(search.toLowerCase())
				)
			: data.products
	);

	const itemOf = $derived(
		(id: string) => sale.items.find((i) => i.product_id === id)
	);

	const selectedMethod = $derived(data.methods.find((m) => m.id === paymentMethodId) ?? null);

	// --- On-account preview (exact integer cents) ---
	//
	// The server still validates every amount and ceiling on completion. These
	// derivations exist only so the cashier sees what will be charged at the
	// till, how much available credit applies automatically (S8), and what
	// remains of the customer's limit (S7) before clicking Charge.
	const totalCents = $derived(parseCents(sale.total) ?? 0n);
	const onAccountTrimmed = $derived(onAccountInput.trim());
	const onAccountCents = $derived(
		!sale.customer_id || !selectedCustomer || onAccountTrimmed === ''
			? 0n
			: parseCents(onAccountTrimmed)
	);
	const onAccountError = $derived.by(() => {
		if (!sale.customer_id || !selectedCustomer || onAccountTrimmed === '') return null;
		if (onAccountCents === null) return 'Enter an amount with up to two decimals.';
		if (onAccountCents < 0n) return 'Amount cannot be negative.';
		if (onAccountCents > totalCents) return 'Cannot exceed the sale total.';
		return null;
	});
	const activeOnAccountCents = $derived(
		onAccountError === null && onAccountCents !== null ? onAccountCents : 0n
	);
	const toChargeCents = $derived(totalCents - activeOnAccountCents);
	const toChargeDisplay = $derived(
		onAccountError === null ? formatCents(toChargeCents) : '—'
	);
	const needsPaymentMethod = $derived(toChargeCents > 0n || activeOnAccountCents === 0n);
	const availableCreditCents = $derived(
		selectedCustomer ? (parseCents(selectedCustomer.available_credit) ?? 0n) : 0n
	);
	const creditAppliedPreviewCents = $derived(
		activeOnAccountCents > 0n && availableCreditCents > 0n
			? minCents(activeOnAccountCents, availableCreditCents)
			: 0n
	);
	const debtAddedPreviewCents = $derived(activeOnAccountCents - creditAppliedPreviewCents);
	const limitPreview = $derived.by(() => {
		if (!selectedCustomer) return null;
		if (selectedCustomer.credit_limit === null || selectedCustomer.credit_limit === undefined) {
			return { unlimited: true as const };
		}
		const limit = parseCents(selectedCustomer.credit_limit) ?? 0n;
		const balance = parseCents(selectedCustomer.balance) ?? 0n;
		const remaining = limit - (balance + activeOnAccountCents);
		return {
			unlimited: false as const,
			cashOnly: limit === 0n,
			limitText: selectedCustomer.credit_limit,
			remainingCents: remaining > 0n ? remaining : 0n,
			exceedsByCents: remaining < 0n ? -remaining : 0n
		};
	});
	const canComplete = $derived(
		!busy &&
			!completed &&
			sale.item_count > 0 &&
			onAccountError === null &&
			(!needsPaymentMethod || selectedMethod !== null)
	);

	/** Apply the server response, the cart's only source of truth. */
	function apply(next: Sale) {
		sale = next;
		error = null;
	}

	async function run(action: () => Promise<Sale>) {
		if (busy) return;
		busy = true;
		try {
			apply(await action());
		} catch (e) {
			error = e instanceof ApiError ? e.message : 'Could not update the sale.';
		} finally {
			busy = false;
		}
	}

	async function add(product: Product) {
		await run(() => api.addItem(sale.id, product.id, '1'));
	}

	async function changeQuantity(productId: string, current: string, delta: number) {
		// Decimals travel as strings; Number is used only for the exact increment of 1.
		const next = Math.max(0, Number(current) + delta);
		await run(() => api.setItemQuantity(sale.id, productId, String(next)));
	}

	async function remove(productId: string) {
		await run(() => api.removeItem(sale.id, productId));
	}

	async function complete() {
		if (!canComplete) return;
		busy = true;
		try {
			const hasOnAccount = activeOnAccountCents > 0n;
			const onAccountParam = hasOnAccount ? formatCents(activeOnAccountCents) : undefined;
			const payments =
				needsPaymentMethod && selectedMethod
					? [{ payment_method_id: selectedMethod.id, amount: formatCents(toChargeCents) }]
					: [];

			const done = await api.completeSale(sale.id, payments, onAccountParam);
			const creditAppliedCents = parseCents(done.credit_applied) ?? 0n;
			const tillPayment = payments[0] ?? null;
			completed = {
				total: done.total,
				paid: tillPayment ? tillPayment.amount : null,
				method: tillPayment ? (selectedMethod?.name ?? null) : null,
				destination: tillPayment ? (selectedMethod?.wallet_name ?? null) : null,
				onAccount: onAccountParam ?? null,
				creditApplied: creditAppliedCents > 0n ? done.credit_applied : null
			};
			error = null;
		} catch (e) {
			// The server checks the sum, the method and the credit limit. A 409/422's
			// `problem.detail` is displayed verbatim, never replaced by a client-side guess.
			error =
				e instanceof ApiError
					? (e.problem?.detail ?? `The server rejected the charge (HTTP ${e.status}).`)
					: 'Could not charge the sale.';
		} finally {
			busy = false;
		}
	}

	async function cancel() {
		if (busy) return;
		busy = true;
		try {
			await api.cancelSale(sale.id);
			window.location.href = '/operations/pos';
		} catch (e) {
			error = e instanceof ApiError ? e.message : 'Could not cancel.';
		} finally {
			busy = false;
		}
	}

	/** The barcode scanner types and sends Enter. */
	function onScanKey(event: KeyboardEvent) {
		if (event.key !== 'Enter') return;
		event.preventDefault();
		const term = search.trim();
		if (!term) return;

		const exact = data.products.find((p) => p.sku?.toLowerCase() === term.toLowerCase());
		const target = exact ?? visible[0];
		if (target) {
			void add(target);
			search = '';
		}
	}

	function focusScan() {
		scanInput?.focus();
	}

	/** Active customers matching the picker's search box, name or document. */
	async function loadCustomers() {
		try {
			const list = await api.listCustomers({
				search: customerSearch.trim() || undefined,
				limit: 20
			});
			customers = list.items;
			pickerError = null;
		} catch (e) {
			pickerError = e instanceof ApiError ? e.message : 'Could not load customers.';
		}
	}

	function togglePicker() {
		pickerOpen = !pickerOpen;
		if (pickerOpen) void loadCustomers();
	}

	/** The list follows the search box, at the customers screen's own pace. */
	function onCustomerInput() {
		clearTimeout(customerTimer);
		customerTimer = setTimeout(() => void loadCustomers(), 250);
	}

	/**
	 * Attach or clear the draft's customer. The PUT answers 204, so the sale is
	 * re-read: the cart's only source of truth is the server response. On a
	 * refusal the previous customer stays exactly as it was, and the server's
	 * `problem.detail` is shown verbatim.
	 */
	async function chooseCustomer(customerId: string | null) {
		if (busy) return;
		busy = true;
		try {
			await api.setSaleCustomer(sale.id, customerId);
			const [nextSale, nextCustomer] = await Promise.all([
				api.getSale(sale.id),
				customerId ? api.getCustomer(customerId) : Promise.resolve(null)
			]);
			apply(nextSale);
			selectedCustomer = nextCustomer;
			onAccountInput = '';
			pickerOpen = false;
		} catch (e) {
			error = e instanceof ApiError ? e.message : 'Could not set the customer.';
		} finally {
			busy = false;
		}
	}
</script>

<svelte:window onkeydown={(e) => e.key === 'F2' && (e.preventDefault(), focusScan())} />

<div class="pos">
	<section class="main">
		<header class="head">
			<h1>New sale</h1>
			<span class="pill">{sale.item_count} {sale.item_count === 1 ? 'item' : 'items'}</span>
		</header>

		{#if completed}
			<div class="done" role="status">
				<strong>Sale charged.</strong> Total {completed.total}.
				{#if completed.paid && completed.method && completed.destination}
					Paid{completed.onAccount ? ` ${completed.paid}` : ''} with {completed.method} into
					{completed.destination}.
				{/if}
				{#if completed.onAccount}
					On account: {completed.onAccount}.
				{/if}
				{#if completed.creditApplied}
					Credit applied: {completed.creditApplied}.
				{/if}
				Stock has been deducted.
				<button type="button" onclick={() => (window.location.href = '/operations/pos')}>New sale</button>
			</div>
		{/if}

		{#if error}
			<p class="error" role="alert">{error}</p>
		{/if}

		<div class="scan">
			<input
				bind:this={scanInput}
				bind:value={search}
				onkeydown={onScanKey}
				type="text"
				placeholder="Scan a code or search by name…"
				aria-label="Scan or search"
			/>
			<span class="hint"><kbd>F2</kbd> focus · <kbd>Enter</kbd> add</span>
		</div>

		{#if visible.length === 0}
			<div class="empty">
				{search ? `No results for “${search}”.` : 'No products have been added yet.'}
			</div>
		{:else}
			<div class="grid">
				{#each visible as product (product.id)}
					{@const inCart = itemOf(product.id)}
					{@const out = product.stock === '0'}
					<button
						type="button"
						class="tile"
						class:low={product.low_stock}
						class:out
						onclick={() => add(product)}
						disabled={busy || out}
						title={out ? 'Out of stock' : `Add ${product.name}`}
					>
						{#if inCart}
							<span class="qty-badge">{inCart.quantity}</span>
						{/if}
						<span class="n">{product.name}</span>
						<span class="p">{product.price ?? '—'}</span>
						<span class="m">
							<span class="s">{out ? 'out of stock' : `${product.stock} ${Number(product.stock) === 1 ? 'unit' : 'units'}`}</span>
							<span>{product.sku ?? '—'}</span>
						</span>
					</button>
				{/each}
			</div>
		{/if}
	</section>

	<!-- The cart lives on the server; this panel only displays its response. -->
	<aside class="cart" aria-label="Cart">
		<div class="cart-head">
			<span class="t">Cart</span>
			<span class="n">{sale.item_count} {sale.item_count === 1 ? 'item' : 'items'}</span>
		</div>

		{#if sale.items.length === 0}
			<div class="empty-cart">
				Scan a product or tap a card to start the sale.
			</div>
		{:else}
			<div class="lines">
				{#each sale.items as item (item.product_id)}
					<div class="line">
						<div>
							<div class="n">{item.product_name}</div>
							<div class="u">{item.unit_price} ×</div>
						</div>
						<div class="qty">
							<button
								type="button"
								aria-label="Remove one"
								disabled={busy}
								onclick={() => changeQuantity(item.product_id, item.quantity, -1)}>−</button
							>
							<span class="v">{item.quantity}</span>
							<button
								type="button"
								aria-label="Add one"
								disabled={busy}
								onclick={() => changeQuantity(item.product_id, item.quantity, 1)}>+</button
							>
						</div>
						<div class="sub">{item.subtotal}</div>
						<button
							type="button"
							class="rm"
							aria-label={`Remove ${item.product_name}`}
							disabled={busy}
							onclick={() => remove(item.product_id)}>×</button
						>
					</div>
				{/each}
			</div>
		{/if}

		<div class="totals">
			<div class="customer">
				<label class="pm-label" for="customer-trigger">Customer</label>
				<button
					type="button"
					id="customer-trigger"
					class="cust-trigger"
					onclick={togglePicker}
					aria-expanded={pickerOpen}
					aria-haspopup="dialog"
					disabled={busy}
				>
					{sale.customer_name ?? 'No customer'}
				</button>

				{#if pickerOpen}
					<div class="cust-panel" role="dialog" aria-label="Choose a customer">
						<input
							type="search"
							bind:value={customerSearch}
							oninput={onCustomerInput}
							placeholder="Search by name or document…"
							aria-label="Search customers"
						/>
						<button
							type="button"
							class="cust-none"
							disabled={busy}
							onclick={() => chooseCustomer(null)}
						>
							No customer
						</button>
						{#each customers as customer (customer.id)}
							<button
								type="button"
								class="cust-option"
								disabled={busy}
								onclick={() => chooseCustomer(customer.id)}
							>
								<span>{customer.name}</span>
								{#if customer.tax_id}<span class="doc">{customer.tax_id}</span>{/if}
							</button>
						{/each}
						{#if pickerError}
							<p class="cust-error" role="alert">{pickerError}</p>
						{:else if customers.length === 0}
							<p class="cust-empty">
								{customerSearch.trim()
									? `No customers match “${customerSearch}”.`
									: 'No customers have been added yet.'}
							</p>
						{/if}
					</div>
				{/if}
			</div>

			<div class="trow big"><span>Total</span><span>{sale.total}</span></div>

			{#if sale.customer_id && selectedCustomer}
				<div class="acct-box">
					<div class="acct-head">
						<label class="pm-label" for="on-account">On account</label>
						<div class="acct-quick">
							{#if availableCreditCents > 0n && totalCents > 0n}
								<button
									type="button"
									class="acct-chip"
									disabled={busy}
									onclick={() =>
										(onAccountInput = formatCents(minCents(totalCents, availableCreditCents)))}
								>
									Use credit
								</button>
							{/if}
							{#if totalCents > 0n}
								<button
									type="button"
									class="acct-chip"
									disabled={busy}
									onclick={() => (onAccountInput = sale.total)}
								>
									Full
								</button>
							{/if}
							{#if onAccountTrimmed !== ''}
								<button
									type="button"
									class="acct-chip"
									disabled={busy}
									onclick={() => (onAccountInput = '')}
								>
									Clear
								</button>
							{/if}
						</div>
					</div>
					<input
						id="on-account"
						class="acct-input"
						type="text"
						inputmode="decimal"
						placeholder="0.00"
						bind:value={onAccountInput}
						disabled={busy || sale.item_count === 0}
					/>
					{#if onAccountError}
						<p class="acct-warn" role="alert">{onAccountError}</p>
					{/if}

					<div class="acct-meta">
						{#if activeOnAccountCents > 0n}
							<div class="trow acct-charge">
								<span>To charge now</span>
								<span>{toChargeDisplay}</span>
							</div>
						{/if}
						{#if availableCreditCents > 0n}
							<div class="trow acct-credit">
								<span>
									{activeOnAccountCents > 0n ? 'Apply available credit' : 'Available credit'}
								</span>
								<span>
									{activeOnAccountCents > 0n
										? `−${formatCents(creditAppliedPreviewCents)}`
										: selectedCustomer.available_credit}
								</span>
							</div>
							{#if activeOnAccountCents > 0n}
								<p class="pm-hint">
									{#if debtAddedPreviewCents > 0n}
										Adds {formatCents(debtAddedPreviewCents)} to customer debt.
									{:else}
										Fully covered by the customer's available credit.
									{/if}
								</p>
							{/if}
						{/if}
						{#if limitPreview}
							<div class="trow" class:acct-warn={!limitPreview.unlimited && limitPreview.exceedsByCents > 0n}>
								<span>Remaining limit</span>
								<span>
									{#if limitPreview.unlimited}
										Unlimited
									{:else if limitPreview.cashOnly && activeOnAccountCents === 0n}
										0.00 (cash only)
									{:else if limitPreview.exceedsByCents > 0n}
										0.00 (exceeds by {formatCents(limitPreview.exceedsByCents)})
									{:else}
										{formatCents(limitPreview.remainingCents)}
									{/if}
								</span>
							</div>
						{/if}
					</div>
				</div>
			{/if}

			<div class="pay-method">
				<label class="pm-label" for="payment-method">Payment method</label>
				{#if !needsPaymentMethod}
					<p class="pm-hint">Covered by customer account — no till payment required.</p>
				{:else if data.methods.length === 0}
					<p class="pm-empty">
						No active payment methods. <a href="/settings/payments">Add one in payment settings</a>
						before charging a sale.
					</p>
				{:else}
					<Select.Root type="single" bind:value={paymentMethodId}>
						<Select.Trigger id="payment-method" class="w-full" disabled={busy}>
							{selectedMethod?.name ?? 'Choose a payment method'}
						</Select.Trigger>
						<Select.Content>
							{#each data.methods as method (method.id)}
								<Select.Item value={method.id} label={method.name}>
									{method.name}
									<span class="pm-wallet">→ {method.wallet_name}</span>
								</Select.Item>
							{/each}
						</Select.Content>
					</Select.Root>
					{#if selectedMethod}
						<p class="pm-hint">
							Pays {activeOnAccountCents > 0n ? `${toChargeDisplay} ` : ''}into
							{selectedMethod.wallet_name}
						</p>
					{/if}
				{/if}
			</div>

			<p class="note">Calculated by the server. Stock is deducted when the sale is charged.</p>
			<div class="pay">
				<button type="button" onclick={cancel} disabled={busy || sale.item_count === 0}>
					Cancel
				</button>
				<button
					type="button"
					class="primary"
					onclick={complete}
					disabled={!canComplete}
					title={needsPaymentMethod && !selectedMethod ? 'Choose a payment method first' : undefined}
				>
					{busy ? 'Processing…' : 'Charge'}
				</button>
			</div>
		</div>
	</aside>
</div>

<style>
	.pos {
		display: grid;
		grid-template-columns: 1fr 360px;
		min-height: 100vh;
	}

	/* --- Grid --- */
	.main {
		padding: 1.15rem 1.5rem 1.5rem;
		min-width: 0;
	}

	.head {
		display: flex;
		align-items: center;
		gap: 0.75rem;
		margin-bottom: 1rem;
	}

	.head h1 {
		margin: 0;
		font-size: 1.2rem;
	}

	.pill {
		font-size: 0.72rem;
		padding: 0.15rem 0.55rem;
		border-radius: 999px;
		border: 1px solid var(--color-border);
		color: var(--color-muted-foreground);
	}

	.scan {
		display: flex;
		gap: 0.6rem;
		align-items: center;
		margin-bottom: 1rem;
	}

	.scan input {
		flex: 1;
		font-size: 1.02rem;
		padding: 0.65rem 0.8rem;
		border-color: color-mix(in srgb, var(--color-primary) 55%, var(--color-border));
	}

	.hint {
		font-size: 0.78rem;
		color: var(--color-muted-foreground);
		white-space: nowrap;
	}

	kbd {
		font-family: ui-monospace, monospace;
		font-size: 0.7rem;
		padding: 0.05rem 0.3rem;
		border-radius: 4px;
		border: 1px solid var(--color-border);
		color: var(--color-muted-foreground);
	}

	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(168px, 1fr));
		gap: 0.6rem;
	}

	.tile {
		position: relative;
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
		text-align: left;
		padding: 0.7rem 0.75rem;
		background: var(--color-card);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		color: var(--color-foreground);
		font: inherit;
	}

	.tile:hover:not(:disabled) {
		border-color: color-mix(in srgb, var(--color-primary) 45%, var(--color-border));
	}

	.tile:disabled {
		opacity: 0.45;
		cursor: not-allowed;
	}

	.tile.low .s {
		color: var(--color-warn);
	}

	.tile .n {
		font-size: 0.93rem;
		font-weight: 500;
		line-height: 1.3;
	}

	.tile .p {
		font-size: 0.95rem;
		font-weight: 600;
		font-variant-numeric: tabular-nums;
	}

	.tile .m {
		display: flex;
		justify-content: space-between;
		gap: 0.5rem;
		font-size: 0.77rem;
		color: var(--color-muted-foreground);
		font-variant-numeric: tabular-nums;
	}

	.qty-badge {
		position: absolute;
		top: 0.4rem;
		right: 0.4rem;
		min-width: 1.35rem;
		padding: 0 0.3rem;
		border-radius: 999px;
		background: var(--color-primary);
		color: var(--color-primary-foreground);
		font-size: 0.72rem;
		font-weight: 700;
		text-align: center;
		font-variant-numeric: tabular-nums;
	}

	/* --- Cart --- */
	.cart {
		background: var(--color-card);
		border-left: 1px solid var(--color-border);
		display: flex;
		flex-direction: column;
	}

	.cart-head {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
		padding: 1.1rem 1.25rem 0.9rem;
		border-bottom: 1px solid var(--color-border);
	}

	.cart-head .t {
		font-weight: 600;
	}

	.cart-head .n {
		font-size: 0.8rem;
		color: var(--color-muted-foreground);
	}

	.lines {
		flex: 1;
		overflow-y: auto;
		padding: 0.25rem 1.25rem;
	}

	.line {
		display: grid;
		grid-template-columns: 1fr auto auto auto;
		gap: 0.6rem;
		align-items: center;
		padding: 0.6rem 0;
		border-bottom: 1px solid var(--color-border);
	}

	.line:last-child {
		border-bottom: none;
	}

	.line .n {
		font-size: 0.9rem;
		line-height: 1.3;
	}

	.line .u {
		font-size: 0.75rem;
		color: var(--color-muted-foreground);
		font-variant-numeric: tabular-nums;
	}

	.qty {
		display: flex;
		align-items: center;
		gap: 0.15rem;
	}

	.qty button {
		width: 24px;
		height: 24px;
		padding: 0;
		line-height: 1;
		border-radius: 6px;
		font-size: 0.9rem;
	}

	.qty .v {
		min-width: 2rem;
		text-align: center;
		font-size: 0.87rem;
		font-variant-numeric: tabular-nums;
	}

	.sub {
		min-width: 4.5rem;
		text-align: right;
		font-size: 0.9rem;
		font-variant-numeric: tabular-nums;
	}

	.rm {
		padding: 0 0.35rem;
		border: none;
		background: transparent;
		color: var(--color-muted-foreground);
		font-size: 1rem;
	}

	.rm:hover {
		color: var(--color-destructive);
	}

	.empty-cart,
	.empty {
		padding: 2.5rem 1.25rem;
		text-align: center;
		color: var(--color-muted-foreground);
		font-size: 0.9rem;
	}

	.totals {
		padding: 1rem 1.25rem;
		border-top: 1px solid var(--color-border);
		background: color-mix(in srgb, var(--color-primary) 3%, var(--color-card));
	}

	.trow {
		display: flex;
		justify-content: space-between;
		font-variant-numeric: tabular-nums;
	}

	/* --- Customer picker --- */

	.customer {
		display: grid;
		gap: 0.35rem;
		margin-bottom: 0.6rem;
	}

	.cust-trigger {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 0.5rem;
		width: 100%;
		padding: 0.45rem 0.6rem;
		font-size: 0.87rem;
		text-align: left;
		overflow-wrap: anywhere;
	}

	.cust-trigger::after {
		content: '▾';
		font-size: 0.7rem;
		color: var(--color-muted-foreground);
	}

	.cust-panel {
		display: grid;
		gap: 0.25rem;
		max-height: 240px;
		overflow-y: auto;
		padding: 0.5rem;
		background: var(--color-card);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
	}

	.cust-panel input {
		font-size: 0.87rem;
		padding: 0.4rem 0.55rem;
	}

	.cust-none {
		justify-content: flex-start;
		padding: 0.35rem 0.5rem;
		font-size: 0.85rem;
		font-style: italic;
		color: var(--color-muted-foreground);
	}

	.cust-option {
		display: flex;
		justify-content: space-between;
		gap: 0.5rem;
		padding: 0.35rem 0.5rem;
		font-size: 0.85rem;
		text-align: left;
	}

	.cust-option .doc {
		color: var(--color-muted-foreground);
		font-size: 0.75rem;
	}

	.cust-empty,
	.cust-error {
		margin: 0;
		padding: 0.3rem 0.2rem;
		font-size: 0.78rem;
		text-align: center;
		color: var(--color-muted-foreground);
	}

	.cust-error {
		color: var(--color-destructive);
	}

	.trow.big {
		font-size: 1.3rem;
		font-weight: 700;
		margin-bottom: 0.5rem;
	}

	.acct-box {
		display: grid;
		gap: 0.4rem;
		padding: 0.6rem 0.7rem;
		margin-bottom: 0.75rem;
		background: var(--color-card);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
	}

	.acct-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 0.5rem;
	}

	.acct-quick {
		display: flex;
		gap: 0.25rem;
	}

	.acct-chip {
		padding: 0.15rem 0.45rem;
		font-size: 0.72rem;
		line-height: 1.3;
	}

	.acct-input {
		width: 100%;
		padding: 0.4rem 0.55rem;
		font-size: 0.9rem;
		font-variant-numeric: tabular-nums;
	}

	.acct-meta {
		display: grid;
		gap: 0.25rem;
		font-size: 0.8rem;
		color: var(--color-muted-foreground);
	}

	.acct-charge {
		font-weight: 600;
		color: var(--color-foreground);
	}

	.acct-credit {
		color: var(--color-primary);
		font-weight: 500;
	}

	.acct-warn {
		margin: 0;
		font-size: 0.78rem;
		color: var(--color-destructive);
	}

	.pay-method {
		display: grid;
		gap: 0.35rem;
		margin-bottom: 0.85rem;
	}

	.pm-label {
		font-size: 0.78rem;
		font-weight: 500;
		color: var(--color-muted-foreground);
	}

	.pm-empty {
		margin: 0;
		font-size: 0.78rem;
		line-height: 1.4;
		color: var(--color-destructive);
	}

	.pm-empty a {
		color: var(--color-primary);
		text-decoration: underline;
	}

	.pm-hint {
		margin: 0;
		font-size: 0.75rem;
		color: var(--color-muted-foreground);
	}

	.pm-wallet {
		font-size: 0.8rem;
		color: var(--color-muted-foreground);
	}

	.note {
		font-size: 0.75rem;
		color: var(--color-muted-foreground);
		margin: 0 0 0.85rem;
	}

	.pay {
		display: flex;
		gap: 0.5rem;
	}

	.pay button {
		flex: 1;
		padding: 0.7rem;
		font-weight: 600;
	}

	.pay .primary {
		background: var(--color-primary);
		border-color: var(--color-primary);
		color: var(--color-primary-foreground);
	}

	.done {
		border: 1px solid color-mix(in srgb, var(--color-primary) 50%, var(--color-border));
		background: color-mix(in srgb, var(--color-primary) 10%, transparent);
		padding: 0.7rem 0.85rem;
		border-radius: var(--radius);
		margin-bottom: 1rem;
		font-size: 0.9rem;
		display: flex;
		align-items: center;
		gap: 0.6rem;
		flex-wrap: wrap;
	}

	.done button {
		font-size: 0.85rem;
		padding: 0.3rem 0.7rem;
	}

	@media (max-width: 980px) {
		.pos {
			grid-template-columns: 1fr;
		}

		.cart {
			border-left: none;
			border-top: 1px solid var(--color-border);
		}
	}
</style>
