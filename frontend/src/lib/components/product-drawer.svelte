<script lang="ts">
	import { Badge } from '$lib/components/ui/badge';
	import {
		Sheet,
		SheetContent,
		SheetDescription,
		SheetHeader,
		SheetTitle
	} from '$lib/components/ui/sheet';
	import type { Movement, Product } from '$lib/api/client';

	let {
		open,
		product,
		movements,
		onClose
	}: {
		/** Driven by `?id=` in the URL: true whenever a product is selected. */
		open: boolean;
		product: Product | null;
		movements: Movement[];
		onClose: () => void;
	} = $props();

	/** Map API reason values to display labels (same copy as the movements screen). */
	const REASONS: Record<Movement['reason'], string> = {
		purchase: 'Purchase',
		sale: 'Sale',
		adjustment: 'Adjustment',
		loss: 'Loss'
	};

	function kindLabel(product: Product): string {
		return product.kind === 'service' ? 'Service' : 'Product';
	}

	/** Decimals arrive as strings and are displayed unchanged (ui-admin S10). */
	function decimal(value: string | null | undefined): string {
		return value ?? '—';
	}

	/** `null` stock on a service means "not applicable", not zero. */
	function stockOf(product: Product): string {
		if (product.kind === 'service') return '—';
		return product.stock ?? '0';
	}

	/** `en-US` per the repo language convention; same format as the movements screen. */
	function when(iso: string): string {
		const date = new Date(iso);
		return Number.isNaN(date.getTime())
			? iso
			: date.toLocaleString('en-US', { dateStyle: 'short', timeStyle: 'short' });
	}

	/** The server applies the sign rule; the client only colors it. */
	function isOut(movement: Movement): boolean {
		return movement.signed_quantity.trim().startsWith('-');
	}
</script>

<!-- Read-only drawer: the Edit button, create mode and deactivation land in the
     next slice over this same component (odd/tasks/catalog-drawer.md, T2-T4). -->
<Sheet open={open} onOpenChange={(o) => !o && onClose()}>
	<SheetContent side="right" class="overflow-y-auto sm:max-w-md">
		{#if product}
			<SheetHeader>
				<SheetTitle>{product.name}</SheetTitle>
				<SheetDescription>{product.sku ?? 'No SKU'}</SheetDescription>
			</SheetHeader>

			<dl class="grid grid-cols-[7rem_1fr] gap-x-4 gap-y-2 px-4 text-sm">
				<dt class="text-muted-foreground">Type</dt>
				<dd>{kindLabel(product)}</dd>
				<dt class="text-muted-foreground">Price</dt>
				<dd class="tabular-nums">{decimal(product.price)}</dd>
				<dt class="text-muted-foreground">Min stock</dt>
				<dd class="tabular-nums">{decimal(product.min_stock)}</dd>
				<dt class="text-muted-foreground">Stock</dt>
				<dd>
					<span class="tabular-nums">{stockOf(product)}</span>
					{#if product.low_stock}
						<Badge variant="outline" class="ml-1.5 text-[10px] uppercase">low</Badge>
					{/if}
				</dd>
				<dt class="text-muted-foreground">Status</dt>
				<dd>
					<Badge variant={product.is_active ? 'outline' : 'secondary'}>
						{product.is_active ? 'Active' : 'Inactive'}
					</Badge>
				</dd>
				<dt class="text-muted-foreground">Description</dt>
				<dd class="whitespace-pre-wrap">{product.description ?? '—'}</dd>
				<dt class="text-muted-foreground">Created</dt>
				<dd class="text-muted-foreground">{when(product.created_at)}</dd>
				<dt class="text-muted-foreground">Updated</dt>
				<dd class="text-muted-foreground">{when(product.updated_at)}</dd>
			</dl>

			<div class="px-4">
				<h2 class="mb-2 text-xs font-medium uppercase tracking-wide text-muted-foreground">
					Stock movements
				</h2>
				{#if movements.length === 0}
					<p class="text-sm text-muted-foreground">No movements recorded yet.</p>
				{:else}
					<ul class="space-y-2">
						{#each movements as movement (movement.id)}
							<li class="flex items-start justify-between gap-2 border-b pb-2 last:border-b-0">
								<div class="min-w-0">
									<Badge variant="outline" class="text-[10px] uppercase">
										{REASONS[movement.reason]}
									</Badge>
									<p class="mt-1 truncate text-xs text-muted-foreground">
										{movement.note ?? '—'}
									</p>
								</div>
								<div class="text-right">
									<p
										class="tabular-nums text-sm font-medium"
										class:text-destructive={isOut(movement)}
									>
										{movement.signed_quantity}
									</p>
									<p class="text-[10px] text-muted-foreground">{when(movement.created_at)}</p>
								</div>
							</li>
						{/each}
					</ul>
					{#if movements.length >= 50}
						<!-- The movements endpoint returns up to 50 by default and reports no
						     total, so a full page may be hiding older rows. Learned from the
						     movements screen review: never imply completeness. -->
						<p class="mt-2 text-xs text-muted-foreground">
							Newest first. The endpoint returns up to 50; older movements may exist.
						</p>
					{/if}
				{/if}
			</div>
		{:else}
			<SheetHeader>
				<SheetTitle>Product not found</SheetTitle>
				<SheetDescription>This product does not exist. The link may be stale.</SheetDescription>
			</SheetHeader>
		{/if}
	</SheetContent>
</Sheet>
