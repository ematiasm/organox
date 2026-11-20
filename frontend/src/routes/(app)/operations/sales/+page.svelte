<script lang="ts">
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import {
		Table,
		TableBody,
		TableCell,
		TableHead,
		TableHeader,
		TableRow
	} from '$lib/components/ui/table';

	let { data } = $props();

	function when(iso: string | null | undefined): string {
		if (!iso) return '—';
		const date = new Date(iso);
		return Number.isNaN(date.getTime())
			? iso
			: date.toLocaleString('en-US', { dateStyle: 'short', timeStyle: 'short' });
	}
</script>

<div class="mb-4">
	<h1 class="text-xl font-semibold tracking-tight">Sales</h1>
	<p class="mt-1 text-sm text-muted-foreground">
		Charged sales, newest first. Open a sale to issue a credit note for goods that come back.
	</p>
</div>

{#if data.sales.length === 0}
	<div class="rounded-lg border border-border p-10 text-center text-sm text-muted-foreground">
		No charged sales yet. A sale appears here once it is charged at the point of sale.
	</div>
{:else}
	<div class="overflow-hidden rounded-lg border border-border">
		<Table>
			<TableHeader>
				<TableRow>
					<TableHead>When</TableHead>
					<TableHead>Sale</TableHead>
					<TableHead>Customer</TableHead>
					<TableHead>Items</TableHead>
					<TableHead class="text-right">Total</TableHead>
					<TableHead class="text-right">Action</TableHead>
				</TableRow>
			</TableHeader>
			<TableBody>
				{#each data.sales as sale (sale.id)}
					<TableRow>
						<TableCell class="text-muted-foreground">{when(sale.completed_at ?? sale.created_at)}</TableCell>
						<TableCell class="font-mono text-xs" title={sale.id}>{sale.id.slice(0, 8)}</TableCell>
						<!-- The name the server carries: for a charged sale it is the one
						     frozen at checkout, never the registry's live name. -->
						<TableCell>{sale.customer_name ?? '—'}</TableCell>
						<TableCell>
							<Badge variant="secondary">
								{sale.item_count}
								{sale.item_count === 1 ? 'item' : 'items'}
							</Badge>
						</TableCell>
						<TableCell class="text-right font-medium tabular-nums">{sale.total}</TableCell>
						<TableCell class="text-right">
							<Button size="sm" variant="outline" href={`/operations/sales/${sale.id}/credit-note`}>
								Credit note
							</Button>
						</TableCell>
					</TableRow>
				{/each}
			</TableBody>
		</Table>
	</div>
	<p class="mt-3 text-sm text-muted-foreground">
		{data.sales.length}
		{data.sales.length === 1 ? 'sale' : 'sales'} shown, newest first.
	</p>
{/if}
