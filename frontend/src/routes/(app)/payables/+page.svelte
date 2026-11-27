<script lang="ts">
	import { goto, invalidateAll } from '$app/navigation';
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
	import { cn } from '$lib/utils';

	let { data } = $props();

	let search = $state('');
	let refreshing = $state(false);

	function goTo(page: number) {
		search = '';
		goto(page > 1 ? `/payables?page=${page}` : '/payables');
	}

	const filtered = $derived.by(() => {
		const term = search.trim().toLowerCase();
		if (!term) return data.report.items;
		return data.report.items.filter(
			(item) =>
				item.supplier_name.toLowerCase().includes(term) ||
				(item.tax_id ?? '').toLowerCase().includes(term)
		);
	});

	const counter = $derived.by(() => {
		if (data.total === 0) return '0 accounts';
		if (search.trim()) {
			const shown = filtered.length;
			return `${shown} ${shown === 1 ? 'account' : 'accounts'} matching on this page (${data.total} total)`;
		}
		const from = data.offset + 1;
		const to = data.offset + data.report.items.length;
		const noun = data.total === 1 ? 'account' : 'accounts';
		return `${from}–${to} of ${data.total} ${noun}`;
	});

	async function refresh() {
		refreshing = true;
		try {
			await invalidateAll();
		} finally {
			refreshing = false;
		}
	}

	function bucketClass(amount: string): string {
		return cn('text-right tabular-nums', amount === '0.00' && 'text-muted-foreground');
	}
</script>

<div class="mb-4 flex items-center justify-between gap-4">
	<div>
		<h1 class="text-xl font-semibold tracking-tight">Payables</h1>
		<p class="mt-1 text-sm text-muted-foreground">
			Suppliers with an open balance or unapplied credit. Aging is measured from each purchase's
			completion date.
		</p>
	</div>
	<Button variant="outline" size="sm" disabled={refreshing} onclick={refresh}>
		{refreshing ? 'Refreshing…' : 'Refresh'}
	</Button>
</div>

<div class="mb-4 grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
	<div class="rounded-lg border border-border p-3.5">
		<div class="text-xs text-muted-foreground">Total balance</div>
		<div class="mt-1 text-xl font-semibold tabular-nums">{data.report.total_balance}</div>
	</div>
	<div class="rounded-lg border border-border p-3.5">
		<div class="text-xs text-muted-foreground">Total outstanding</div>
		<div class="mt-1 text-xl font-semibold tabular-nums">{data.report.total_outstanding}</div>
	</div>
	<div class="rounded-lg border border-border p-3.5">
		<div class="text-xs text-muted-foreground">Unapplied credit</div>
		<div class="mt-1 text-xl font-semibold tabular-nums">
			{data.report.total_unapplied_credit}
		</div>
	</div>
	<div class="rounded-lg border border-border p-3.5">
		<div class="text-xs text-muted-foreground">Manual adjustments</div>
		<div class="mt-1 text-xl font-semibold tabular-nums">{data.report.total_adjustments}</div>
	</div>
</div>

<div class="mb-4 grid grid-cols-2 gap-3 rounded-lg border border-border bg-muted/30 p-3 sm:grid-cols-4">
	<div>
		<div class="text-xs text-muted-foreground">0–30 days</div>
		<div class="mt-0.5 text-sm font-medium tabular-nums">{data.report.aging.days_0_30}</div>
	</div>
	<div>
		<div class="text-xs text-muted-foreground">31–60 days</div>
		<div class="mt-0.5 text-sm font-medium tabular-nums">{data.report.aging.days_31_60}</div>
	</div>
	<div>
		<div class="text-xs text-muted-foreground">61–90 days</div>
		<div class="mt-0.5 text-sm font-medium tabular-nums">{data.report.aging.days_61_90}</div>
	</div>
	<div>
		<div class="text-xs text-muted-foreground">90+ days</div>
		<div class="mt-0.5 text-sm font-medium tabular-nums">{data.report.aging.days_90_plus}</div>
	</div>
</div>

{#if data.total === 0}
	<div class="rounded-lg border border-border p-10 text-center text-sm text-muted-foreground">
		No suppliers with an open balance or unapplied credit.
	</div>
{:else if data.report.items.length === 0}
	<div class="rounded-lg border border-border p-10 text-center text-sm text-muted-foreground">
		<p>
			No suppliers on page {data.page}. There are {data.total}
			{data.total === 1 ? 'account' : 'accounts'} across {data.lastPage}
			{data.lastPage === 1 ? 'page' : 'pages'}.
		</p>
		<Button variant="outline" size="sm" class="mt-4" onclick={() => goTo(data.lastPage)}>
			Go to page {data.lastPage}
		</Button>
	</div>
{:else}
	<div class="mb-4 max-w-sm">
		<Input
			type="search"
			placeholder={data.lastPage > 1
				? 'Filter this page by supplier or document…'
				: 'Filter by supplier or document…'}
			bind:value={search}
		/>
	</div>

	{#if filtered.length === 0}
		<div class="rounded-lg border border-border p-10 text-center text-sm text-muted-foreground">
			No open accounts match “{search.trim()}”.
		</div>
	{:else}
		<div class="overflow-x-auto rounded-lg border border-border">
			<Table>
				<TableHeader>
					<TableRow>
						<TableHead>Supplier</TableHead>
						<TableHead>Document</TableHead>
						<TableHead class="text-right">Balance</TableHead>
						<TableHead class="text-right">Credit</TableHead>
						<TableHead class="text-right">Outstanding</TableHead>
						<TableHead class="text-right">0–30d</TableHead>
						<TableHead class="text-right">31–60d</TableHead>
						<TableHead class="text-right">61–90d</TableHead>
						<TableHead class="text-right">90+d</TableHead>
					</TableRow>
				</TableHeader>
				<TableBody>
					{#each filtered as row (row.supplier_id)}
						<TableRow>
							<TableCell class="font-medium">
								<a
									href={`/suppliers?id=${encodeURIComponent(row.supplier_id)}${row.is_active ? '' : '&include_inactive=1'}`}
									class="hover:underline"
								>
									{row.supplier_name}
								</a>
								{#if !row.is_active}
									<Badge variant="secondary" class="ml-1.5 text-[11px]">Inactive</Badge>
								{/if}
							</TableCell>
							<TableCell class="text-muted-foreground">{row.tax_id ?? '—'}</TableCell>
							<TableCell
								class={cn(
									'text-right font-medium tabular-nums',
									row.balance.startsWith('-') && 'text-primary'
								)}
							>
								{row.balance}
							</TableCell>
							<TableCell class={bucketClass(row.unapplied_credit)}>
								{row.unapplied_credit}
							</TableCell>
							<TableCell class={bucketClass(row.outstanding_total)}>
								{row.outstanding_total}
							</TableCell>
							<TableCell class={bucketClass(row.aging.days_0_30)}>
								{row.aging.days_0_30}
							</TableCell>
							<TableCell class={bucketClass(row.aging.days_31_60)}>
								{row.aging.days_31_60}
							</TableCell>
							<TableCell class={bucketClass(row.aging.days_61_90)}>
								{row.aging.days_61_90}
							</TableCell>
							<TableCell class={bucketClass(row.aging.days_90_plus)}>
								{row.aging.days_90_plus}
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
{/if}
