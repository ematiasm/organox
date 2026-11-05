<script lang="ts">
	import { untrack } from 'svelte';
	import { api, ApiError, type Product, type Sale } from '$lib/api/client';

	let { data } = $props();

	// La venta la calcula el servidor. Este estado solo refleja su respuesta:
	// nunca sumamos ni validamos acá, porque un total que el navegador cree
	// puede no ser el que se cobra.
	//
	// Se copia `data.sale` a estado local porque las mutaciones lo reemplazan
	// sin navegar. Es una copia deliberada del valor inicial, no un `$derived`:
	// si fuera derivado, cada respuesta del servidor tendría que pasar por una
	// recarga de página.
	let sale = $state<Sale>(untrack(() => data.sale));
	let error = $state<string | null>(null);
	let busy = $state(false);
	let search = $state('');
	let scanInput: HTMLInputElement | undefined = $state();
	let completed = $state<{ total: string } | null>(null);

	// Búsqueda en cliente sobre lo ya cargado: la grilla es de productos
	// visibles, no un buscador de todo el catálogo.
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

	/** Aplica la respuesta del servidor. Es la única fuente de verdad del carrito. */
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
			error = e instanceof ApiError ? e.message : 'No se pudo actualizar la venta.';
		} finally {
			busy = false;
		}
	}

	async function add(product: Product) {
		await run(() => api.addItem(sale.id, product.id, '1'));
	}

	async function changeQuantity(productId: string, current: string, delta: number) {
		// Los decimales viajan como string; se opera con Number solo para el
		// incremento de a 1, que no pierde precisión.
		const next = Math.max(0, Number(current) + delta);
		await run(() => api.setItemQuantity(sale.id, productId, String(next)));
	}

	async function remove(productId: string) {
		await run(() => api.removeItem(sale.id, productId));
	}

	async function complete() {
		if (busy || sale.item_count === 0) return;
		busy = true;
		try {
			const done = await api.completeSale(sale.id);
			completed = { total: done.total };
		} catch (e) {
			// 409 con stock insuficiente: el mensaje del servidor dice de qué
			// producto y cuánto hay. Se muestra tal cual.
			error = e instanceof ApiError ? e.message : 'No se pudo cobrar la venta.';
		} finally {
			busy = false;
		}
	}

	async function cancel() {
		if (busy) return;
		busy = true;
		try {
			await api.cancelSale(sale.id);
			window.location.href = '/pos';
		} catch (e) {
			error = e instanceof ApiError ? e.message : 'No se pudo cancelar.';
		} finally {
			busy = false;
		}
	}

	/** El lector de código de barras escribe y manda Enter. */
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
</script>

<svelte:window onkeydown={(e) => e.key === 'F2' && (e.preventDefault(), focusScan())} />

<div class="pos">
	<section class="main">
		<header class="head">
			<h1>Nueva venta</h1>
			<span class="pill">{sale.item_count} {sale.item_count === 1 ? 'ítem' : 'ítems'}</span>
		</header>

		{#if completed}
			<div class="done" role="status">
				<strong>Venta cobrada.</strong> Total {completed.total}. El stock ya está descontado.
				<button type="button" onclick={() => (window.location.href = '/pos')}>Nueva venta</button>
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
				placeholder="Escaneá un código o buscá por nombre…"
				aria-label="Escanear o buscar"
			/>
			<span class="hint"><kbd>F2</kbd> foco · <kbd>Enter</kbd> agrega</span>
		</div>

		{#if visible.length === 0}
			<div class="empty">
				{search ? `Sin resultados para «${search}».` : 'No hay productos cargados todavía.'}
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
						title={out ? 'Sin stock' : `Agregar ${product.name}`}
					>
						{#if inCart}
							<span class="qty-badge">{inCart.quantity}</span>
						{/if}
						<span class="n">{product.name}</span>
						<span class="p">{product.price ?? '—'}</span>
						<span class="m">
							<span class="s">{out ? 'sin stock' : `${product.stock} u.`}</span>
							<span>{product.sku ?? '—'}</span>
						</span>
					</button>
				{/each}
			</div>
		{/if}
	</section>

	<!-- El carrito vive en el servidor: este panel solo dibuja su respuesta. -->
	<aside class="cart" aria-label="Carrito">
		<div class="cart-head">
			<span class="t">Carrito</span>
			<span class="n">{sale.item_count} {sale.item_count === 1 ? 'ítem' : 'ítems'}</span>
		</div>

		{#if sale.items.length === 0}
			<div class="empty-cart">
				Escaneá un producto o tocá una tarjeta para empezar la venta.
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
								aria-label="Quitar uno"
								disabled={busy}
								onclick={() => changeQuantity(item.product_id, item.quantity, -1)}>−</button
							>
							<span class="v">{item.quantity}</span>
							<button
								type="button"
								aria-label="Agregar uno"
								disabled={busy}
								onclick={() => changeQuantity(item.product_id, item.quantity, 1)}>+</button
							>
						</div>
						<div class="sub">{item.subtotal}</div>
						<button
							type="button"
							class="rm"
							aria-label={`Quitar ${item.product_name}`}
							disabled={busy}
							onclick={() => remove(item.product_id)}>×</button
						>
					</div>
				{/each}
			</div>
		{/if}

		<div class="totals">
			<div class="trow big"><span>Total</span><span>{sale.total}</span></div>
			<p class="note">Calculado por el servidor. El stock se descuenta al cobrar.</p>
			<div class="pay">
				<button type="button" onclick={cancel} disabled={busy || sale.item_count === 0}>
					Cancelar
				</button>
				<button
					type="button"
					class="primary"
					onclick={complete}
					disabled={busy || sale.item_count === 0}
				>
					{busy ? 'Procesando…' : 'Cobrar'}
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

	/* --- Grilla --- */
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

	/* --- Carrito --- */
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

	.trow.big {
		font-size: 1.3rem;
		font-weight: 700;
		margin-bottom: 0.4rem;
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
