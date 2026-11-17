<script lang="ts">
	import { goto } from '$app/navigation';
	import ProductDrawer from '$lib/components/product-drawer.svelte';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import {
		Table,
		TableBody,
		TableCell,
		TableHead,
		TableHeader,
		TableRow
	} from '$lib/components/ui/table';

	let { data } = $props();

	// Use `$derived` instead of seeded `$state`: the URL is the source of truth
	// for search, keeping the input in sync after using Back.
	let search = $derived(data.search);
	let timer: ReturnType<typeof setTimeout> | undefined;

	/**
	 * Build a products URL from the state that must survive navigation.
	 *
	 * Contract of `page`: omitting it KEEPS the current page; every caller that
	 * must reset passes `1` explicitly. The search call site resets that way on
	 * purpose — staying on page 4 of an old result set after a new search would
	 * show an empty screen that reads as "no matches" — and keeping the page on
	 * omission is what lets the drawer open and close without moving the list
	 * underneath it.
	 *
	 * Search and page changes drop the drawer: it is a peek at one row of the
	 * current result set, and a new result set closes the peek.
	 */
	function productsUrl(params: { search?: string; page?: number; id?: string | null }): string {
		const query = new URLSearchParams();
		const term = (params.search ?? search).trim();
		if (term) query.set('search', term);
		if ((params.page ?? data.page) > 1) query.set('page', String(params.page ?? data.page));
		if (params.id) query.set('id', params.id);

		const qs = query.toString();
		return qs ? `/products?${qs}` : '/products';
	}

	function onInput(event: Event) {
		const value = (event.target as HTMLInputElement).value;
		clearTimeout(timer);
		timer = setTimeout(() => {
			// `reset: false` preserves input focus; `replace` avoids adding each
			// keystroke to the history.
			goto(productsUrl({ search: value, page: 1 }), { reset: false, replace: true });
		}, 300);
	}

	// Page changes are real navigations, not replaces: Back must return to the
	// previous page instead of leaving the list entirely.
	function goTo(page: number) {
		goto(productsUrl({ page }));
	}

	// Opening the drawer is a real navigation too, so Back closes it. Closing
	// replaces the entry instead: after a close, Back leaves the list rather
	// than reopening what the user just dismissed.
	function openProduct(id: string) {
		goto(productsUrl({ id }));
	}

	function closeDrawer() {
		goto(productsUrl({ id: null }), { replace: true });
	}

	/** Decimals arrive as strings ("1500.50") and are displayed unchanged. */
	function money(value: string | null | undefined): string {
		return value ?? '—';
	}

	function stock(product: { stock?: string | null; kind: string }): string {
		// `null` for a service means "not applicable", not "zero".
		if (product.kind === 'service') return '—';
		return product.stock ?? '0';
	}

	/** "1–50 of 137": the range on screen and the real total. */
	let counter = $derived.by(() => {
		if (data.total === 0) return '0 products';
		const from = data.offset + 1;
		const to = data.offset + data.items.length;
		const noun = data.total === 1 ? 'product' : 'products';
		return `${from}–${to} of ${data.total} ${noun}`;
	});
</script>

<!-- Starting a sale lives in the rail; the page does not repeat the link. -->
<h1 class="mb-4 text-xl font-semibold tracking-tight">Products</h1>

<div class="mb-4 max-w-sm">
	<Input type="search" placeholder="Search by name…" value={search} oninput={onInput} />
</div>

{#if data.items.length === 0}
	<div class="rounded-lg border border-border p-10 text-center text-sm text-muted-foreground">
		{#if data.search}
			No products match “{data.search}”.
		{:else}
			No products have been added yet.
		{/if}
	</div>
{:else}
	<div class="overflow-hidden rounded-lg border border-border">
		<Table>
			<TableHeader>
				<TableRow>
					<TableHead>Name</TableHead>
					<TableHead>SKU</TableHead>
					<TableHead>Type</TableHead>
					<TableHead class="text-right">Price</TableHead>
					<TableHead class="text-right">Stock</TableHead>
				</TableRow>
			</TableHeader>
			<TableBody>
				{#each data.items as product (product.id)}
					<TableRow class="cursor-pointer" onclick={() => openProduct(product.id)}>
						<TableCell class="font-medium">
							<!-- A real button so the row is reachable by keyboard; the click bubbles
							     to the row handler. -->
							<button type="button" class="text-left">{product.name}</button>
						</TableCell>
						<TableCell class="text-muted-foreground">{product.sku ?? '—'}</TableCell>
						<TableCell>
							{product.kind === 'service' ? 'Service' : 'Product'}
						</TableCell>
						<TableCell class="text-right tabular-nums">{money(product.price)}</TableCell>
						<TableCell class="text-right tabular-nums">
							{stock(product)}
							{#if product.low_stock}
								<Badge variant="outline" class="ml-1.5 text-[10px] uppercase">low</Badge>
							{/if}
						</TableCell>
					</TableRow>
				{/each}
			</TableBody>
		</Table>
	</div>

	<div class="mt-3 flex items-center justify-between gap-4 text-sm text-muted-foreground">
		<span>{counter}</span>

		{#if data.lastPage > 1}
			<div class="flex items-center gap-2">
				<Button
					variant="outline"
					size="sm"
					disabled={data.page <= 1}
					onclick={() => goTo(data.page - 1)}>Previous</Button
				>
				<span class="tabular-nums">Page {data.page} of {data.lastPage}</span>
				<Button
					variant="outline"
					size="sm"
					disabled={data.page >= data.lastPage}
					onclick={() => goTo(data.page + 1)}>Next</Button
				>
			</div>
		{/if}
	</div>
{/if}

<ProductDrawer
	open={data.id !== ''}
	product={data.product}
	movements={data.movements}
	onClose={closeDrawer}
/>
