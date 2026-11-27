<script lang="ts">
	import { goto } from '$app/navigation';
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

	// Page changes are real navigations, not replaces: Back must return to the
	// previous page instead of leaving the list entirely.
	function goTo(page: number) {
		goto(page > 1 ? `/operations/sales?page=${page}` : '/operations/sales');
	}

	/** "1–50 of 137": the range on screen and the real total. */
	let counter = $derived.by(() => {
		if (data.total === 0) return '0 sales';
		const from = data.offset + 1;
		const to = data.offset + data.items.length;
		const noun = data.total === 1 ? 'sale' : 'sales';
		return `${from}–${to} of ${data.total} ${noun}`;
	});

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

{#if data.total === 0}
	<div class="rounded-lg border border-border p-10 text-center text-sm text-muted-foreground">
		No charged sales yet. A sale appears here once it is charged at the point of sale.
	</div>
{:else if data.items.length === 0}
	<div class="rounded-lg border border-border p-10 text-center text-sm text-muted-foreground">
		<p>No sales on page {data.page}. There are {data.total} {data.total === 1 ? 'sale' : 'sales'} across {data.lastPage} {data.lastPage === 1 ? 'page' : 'pages'}.</p>
		<Button variant="outline" size="sm" class="mt-4" onclick={() => goTo(data.lastPage)}>
			Go to page {data.lastPage}
		</Button>
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
				{#each data.items as sale (sale.id)}
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
