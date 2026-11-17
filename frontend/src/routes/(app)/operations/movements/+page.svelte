<script lang="ts">
	import { Badge } from '$lib/components/ui/badge';
	import {
		Table,
		TableBody,
		TableCell,
		TableHead,
		TableHeader,
		TableRow
	} from '$lib/components/ui/table';
	import { cn } from '$lib/utils';
	import type { MovementReason } from '$lib/api/client';
	import type { MovementRow } from './+page';

	let { data } = $props();

	/** Map API reason values to display labels. */
	const REASONS: Record<MovementReason, string> = {
		purchase: 'Purchase',
		sale: 'Sale',
		adjustment: 'Adjustment',
		loss: 'Loss'
	};

	/** The server calculates `signed_quantity`, avoiding duplicated sign rules. */
	function isOut(movement: MovementRow): boolean {
		return movement.signed_quantity.trim().startsWith('-');
	}

	function when(iso: string): string {
		const date = new Date(iso);
		return Number.isNaN(date.getTime())
			? iso
			: date.toLocaleString('en-US', { dateStyle: 'short', timeStyle: 'short' });
	}

	const shown = $derived(data.movements.length);
</script>

<div class="mb-4">
	<h1 class="text-xl font-semibold tracking-tight">Movements</h1>
	<p class="mt-1 text-sm text-muted-foreground">
		{#if data.productCount >= data.totalProducts}
			Recent activity across all {data.totalProducts}
			{data.totalProducts === 1 ? 'product' : 'products'}.
		{:else}
			Recent activity across the first {data.productCount}
			{data.productCount === 1 ? 'product' : 'products'} of {data.totalProducts} in the
			catalog. Products outside this sample are not shown.
		{/if}
	</p>
</div>

{#if shown === 0}
	<div class="rounded-lg border border-border p-10 text-center text-sm text-muted-foreground">
		No movements recorded yet. They are created when adding stock and charging a sale.
	</div>
{:else}
	<div class="overflow-hidden rounded-lg border border-border">
		<Table>
			<TableHeader>
				<TableRow>
					<TableHead>Date</TableHead>
					<TableHead>Product</TableHead>
					<TableHead>SKU</TableHead>
					<TableHead>Reason</TableHead>
					<TableHead class="text-right">Quantity</TableHead>
					<TableHead>Note</TableHead>
				</TableRow>
			</TableHeader>
			<TableBody>
				{#each data.movements as movement (movement.id)}
					<TableRow>
						<TableCell class="text-muted-foreground">{when(movement.created_at)}</TableCell>
						<TableCell>{movement.product_name}</TableCell>
						<TableCell class="font-mono text-xs text-muted-foreground">
							{movement.product_sku ?? '—'}
						</TableCell>
						<TableCell><Badge variant="outline">{REASONS[movement.reason]}</Badge></TableCell>
						<TableCell
							class={cn('text-right tabular-nums', isOut(movement) && 'text-destructive')}
						>
							{movement.signed_quantity}
						</TableCell>
						<TableCell class="text-muted-foreground">{movement.note ?? '—'}</TableCell>
					</TableRow>
				{/each}
			</TableBody>
		</Table>
	</div>
	<p class="mt-3 text-sm text-muted-foreground">
		{#if data.truncated}
			Showing the {shown} most recent movements. Older movements are not shown.
		{:else}
			Showing {shown}
			{shown === 1 ? 'movement' : 'movements'}.
		{/if}
		{#if data.productCount < data.totalProducts}
			The full history needs a product the sample does not cover.
		{/if}
	</p>
{/if}
