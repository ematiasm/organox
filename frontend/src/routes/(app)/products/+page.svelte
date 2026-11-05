<script lang="ts">
	import { goto } from '$app/navigation';
	import { Badge } from '$lib/components/ui/badge';
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

	function onInput(event: Event) {
		const value = (event.target as HTMLInputElement).value;
		clearTimeout(timer);
		timer = setTimeout(() => {
			const query = value.trim() ? `?search=${encodeURIComponent(value.trim())}` : '';
			// `reset: false` preserves input focus; `replace` avoids adding each
			// keystroke to the history.
			goto(`/products${query}`, { reset: false, replace: true });
		}, 300);
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
					<TableRow>
						<TableCell class="font-medium">{product.name}</TableCell>
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

	<p class="mt-3 text-sm text-muted-foreground">
		{data.items.length} {data.items.length === 1 ? 'product' : 'products'}
	</p>
{/if}
