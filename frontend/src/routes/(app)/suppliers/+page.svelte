<script lang="ts">
	import { goto, invalidateAll } from '$app/navigation';
	import SupplierDrawer from '$lib/components/supplier-drawer.svelte';
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

	let search = $derived(data.search);
	let timer: ReturnType<typeof setTimeout> | undefined;

	function suppliersUrl(params: {
		search?: string;
		page?: number;
		includeInactive?: boolean;
		id?: string | null;
		new?: boolean;
	}): string {
		const query = new URLSearchParams();
		const term = (params.search ?? search).trim();
		if (term) query.set('search', term);
		if ((params.page ?? data.page) > 1) query.set('page', String(params.page ?? data.page));
		if (params.includeInactive ?? data.includeInactive) query.set('include_inactive', '1');
		if (params.new) query.set('new', '1');
		else if (params.id) query.set('id', params.id);

		const qs = query.toString();
		return qs ? `/suppliers?${qs}` : '/suppliers';
	}

	function onInput(event: Event) {
		const value = (event.target as HTMLInputElement).value;
		clearTimeout(timer);
		timer = setTimeout(() => {
			goto(suppliersUrl({ search: value, page: 1 }), { reset: false, replace: true });
		}, 300);
	}

	function goTo(page: number) {
		goto(suppliersUrl({ page }));
	}

	function openSupplier(id: string) {
		goto(suppliersUrl({ id }));
	}

	function closeDrawer() {
		goto(suppliersUrl(data.creating ? { new: false } : { id: null }), { replace: true });
	}

	function startCreate() {
		goto(suppliersUrl({ new: true }));
	}

	function created(id: string) {
		goto(suppliersUrl({ id }), { replace: true });
	}

	let counter = $derived.by(() => {
		if (data.total === 0) return '0 suppliers';
		const from = data.offset + 1;
		const to = data.offset + data.items.length;
		const noun = data.total === 1 ? 'supplier' : 'suppliers';
		return `${from}–${to} of ${data.total} ${noun}`;
	});
</script>

<div class="mb-4 flex items-center justify-between gap-4">
	<h1 class="text-xl font-semibold tracking-tight">Suppliers</h1>
	<Button size="sm" onclick={startCreate}>New supplier</Button>
</div>

<div class="mb-4 flex items-center gap-4">
	<div class="max-w-sm grow">
		<Input
			type="search"
			placeholder="Search by name or document…"
			value={search}
			oninput={onInput}
		/>
	</div>
	<label class="flex items-center gap-2 text-sm text-muted-foreground">
		<input
			id="include-inactive"
			type="checkbox"
			class="size-4 accent-primary"
			checked={data.includeInactive}
			onchange={(e) =>
				goto(
					suppliersUrl({
						includeInactive: (e.target as HTMLInputElement).checked,
						page: 1
					}),
					{ reset: false }
				)}
		/>
		Include deactivated
	</label>
</div>

{#if data.items.length === 0}
	<div class="rounded-lg border border-border p-10 text-center text-sm text-muted-foreground">
		{#if data.search}
			No suppliers match “{data.search}”.
		{:else}
			No suppliers have been added yet.
		{/if}
	</div>
{:else}
	<div class="overflow-hidden rounded-lg border border-border">
		<Table>
			<TableHeader>
				<TableRow>
					<TableHead>Name</TableHead>
					<TableHead>Document</TableHead>
					<TableHead>Email</TableHead>
					<TableHead>Phone</TableHead>
					<TableHead class="text-right">Balance</TableHead>
					<TableHead class="text-right">Credit limit</TableHead>
					<TableHead>Status</TableHead>
				</TableRow>
			</TableHeader>
			<TableBody>
				{#each data.items as supplier (supplier.id)}
					<TableRow class="cursor-pointer" onclick={() => openSupplier(supplier.id)}>
						<TableCell class="font-medium">
							<button type="button" class="text-left">{supplier.name}</button>
						</TableCell>
						<TableCell class="text-muted-foreground">{supplier.tax_id ?? '—'}</TableCell>
						<TableCell class="text-muted-foreground">{supplier.email ?? '—'}</TableCell>
						<TableCell class="text-muted-foreground">{supplier.phone ?? '—'}</TableCell>
						<TableCell
							class={cn(
								'text-right tabular-nums',
								supplier.balance === '0.00'
									? 'text-muted-foreground'
									: supplier.balance.startsWith('-')
										? 'font-medium text-primary'
										: 'font-medium'
							)}
						>
							{supplier.balance}
						</TableCell>
						<TableCell
							class={cn(
								'text-right tabular-nums',
								supplier.credit_limit == null && 'text-muted-foreground'
							)}
						>
							{supplier.credit_limit ?? 'Unlimited'}
						</TableCell>
						<TableCell>
							<Badge variant={supplier.is_active ? 'outline' : 'secondary'}>
								{supplier.is_active ? 'Active' : 'Inactive'}
							</Badge>
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

<SupplierDrawer
	open={data.id !== '' || data.creating}
	creating={data.creating}
	supplier={data.supplier}
	onClose={closeDrawer}
	onSaved={invalidateAll}
	onCreated={created}
/>
