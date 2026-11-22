<script lang="ts">
	import { goto, invalidateAll } from '$app/navigation';
	import CustomerDrawer from '$lib/components/customer-drawer.svelte';
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

	// Use `$derived` instead of seeded `$state`: the URL is the source of truth
	// for search, keeping the input in sync after using Back.
	let search = $derived(data.search);
	let timer: ReturnType<typeof setTimeout> | undefined;

	/**
	 * Build a customers URL from the state that must survive navigation.
	 *
	 * Contract of `page`: omitting it KEEPS the current page; every caller that
	 * must reset passes `1` explicitly. The search call site resets that way on
	 * purpose — staying on page 4 of an old result set after a new search would
	 * show an empty screen that reads as "no matches" — and keeping the page on
	 * omission is what lets the drawer open and close without moving the list
	 * underneath it. Toggling deactivated customers also keeps the page: it only
	 * adds rows, so the current page cannot become empty.
	 *
	 * `new` and `id` are mutually exclusive drawer states; search changes drop
	 * both, because a new result set closes the peek.
	 */
	function customersUrl(params: {
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
		return qs ? `/customers?${qs}` : '/customers';
	}

	function onInput(event: Event) {
		const value = (event.target as HTMLInputElement).value;
		clearTimeout(timer);
		timer = setTimeout(() => {
			// `reset: false` preserves input focus; `replace` avoids adding each
			// keystroke to the history.
			goto(customersUrl({ search: value, page: 1 }), { reset: false, replace: true });
		}, 300);
	}

	// Page changes are real navigations, not replaces: Back must return to the
	// previous page instead of leaving the list entirely.
	function goTo(page: number) {
		goto(customersUrl({ page }));
	}

	// Opening the drawer is a real navigation too, so Back closes it. Closing
	// replaces the entry instead: after a close, Back leaves the list rather
	// than reopening what the user just dismissed.
	function openCustomer(id: string) {
		goto(customersUrl({ id }));
	}

	function closeDrawer() {
		goto(customersUrl(data.creating ? { new: false } : { id: null }), { replace: true });
	}

	function startCreate() {
		goto(customersUrl({ new: true }));
	}

	// A create replaces the `?new` entry with the new customer's `?id`, so Back
	// after saving leaves the list instead of reopening an empty form.
	function created(id: string) {
		goto(customersUrl({ id }), { replace: true });
	}

	/** "1–50 of 137": the range on screen and the real total. */
	let counter = $derived.by(() => {
		if (data.total === 0) return '0 customers';
		const from = data.offset + 1;
		const to = data.offset + data.items.length;
		const noun = data.total === 1 ? 'customer' : 'customers';
		return `${from}–${to} of ${data.total} ${noun}`;
	});
</script>

<div class="mb-4 flex items-center justify-between gap-4">
	<h1 class="text-xl font-semibold tracking-tight">Customers</h1>
	<Button size="sm" onclick={startCreate}>New customer</Button>
</div>

<div class="mb-4 flex items-center gap-4">
	<div class="max-w-sm grow">
		<!-- One box: the API matches the name or the document. -->
		<Input type="search" placeholder="Search by name or document…" value={search} oninput={onInput} />
	</div>
	<label class="flex items-center gap-2 text-sm text-muted-foreground">
		<input
			id="include-inactive"
			type="checkbox"
			class="size-4 accent-primary"
			checked={data.includeInactive}
			onchange={(e) =>
				goto(
					customersUrl({ includeInactive: (e.target as HTMLInputElement).checked }),
					{ reset: false }
				)}
		/>
		Include deactivated
	</label>
</div>

{#if data.items.length === 0}
	<div class="rounded-lg border border-border p-10 text-center text-sm text-muted-foreground">
		{#if data.search}
			No customers match “{data.search}”.
		{:else}
			No customers have been added yet.
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
					<TableHead>Status</TableHead>
				</TableRow>
			</TableHeader>
			<TableBody>
				{#each data.items as customer (customer.id)}
					<TableRow class="cursor-pointer" onclick={() => openCustomer(customer.id)}>
						<TableCell class="font-medium">
							<!-- A real button so the row is reachable by keyboard; the click bubbles
							     to the row handler. -->
							<button type="button" class="text-left">{customer.name}</button>
						</TableCell>
						<TableCell class="text-muted-foreground">{customer.tax_id ?? '—'}</TableCell>
						<TableCell class="text-muted-foreground">{customer.email ?? '—'}</TableCell>
						<TableCell class="text-muted-foreground">{customer.phone ?? '—'}</TableCell>
						<TableCell
							class={cn(
								'text-right tabular-nums',
								customer.balance === '0.00'
									? 'text-muted-foreground'
									: customer.balance.startsWith('-')
										? 'font-medium text-primary'
										: 'font-medium'
							)}
						>
							{customer.balance}
						</TableCell>
						<TableCell>
							<Badge variant={customer.is_active ? 'outline' : 'secondary'}>
								{customer.is_active ? 'Active' : 'Inactive'}
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

<CustomerDrawer
	open={data.id !== '' || data.creating}
	creating={data.creating}
	customer={data.customer}
	onClose={closeDrawer}
	onSaved={invalidateAll}
	onCreated={created}
/>
