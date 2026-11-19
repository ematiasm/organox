<script lang="ts">
	import { untrack } from 'svelte';
	import { invalidateAll } from '$app/navigation';
	import { api, ApiError, type CreditNote } from '$lib/api/client';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import {
		Table,
		TableBody,
		TableCell,
		TableHead,
		TableHeader,
		TableRow
	} from '$lib/components/ui/table';

	let { data } = $props();

	const credits = $derived(data.credits);

	// Quantities are local input state, not money. They start at zero for the
	// loaded lines and are reset after a successful note.
	let quantities = $state<Record<string, number>>(
		untrack(() =>
			Object.fromEntries((data.credits?.items ?? []).map((line) => [line.product_id, 0]))
		)
	);
	let reason = $state('');
	let error = $state<string | null>(null);
	let busy = $state(false);
	let created = $state<CreditNote | null>(null);

	/** Only units are counted here; every money amount is the server's string. */
	const selectedUnits = $derived(
		Object.values(quantities).reduce(
			(sum, quantity) => sum + (Number.isFinite(quantity) ? quantity : 0),
			0
		)
	);
	const canSubmit = $derived(!busy && reason.trim().length > 0 && selectedUnits > 0);

	function failure(error: unknown, fallback: string): string {
		if (error instanceof ApiError) {
			return error.problem?.detail ?? `The server rejected the return (HTTP ${error.status}).`;
		}
		return fallback;
	}

	function when(iso: string): string {
		const date = new Date(iso);
		return Number.isNaN(date.getTime())
			? iso
			: date.toLocaleString('en-US', { dateStyle: 'short', timeStyle: 'short' });
	}

	function unitLabel(units: number): string {
		const count = new Intl.NumberFormat('en-US', { maximumFractionDigits: 3 }).format(units);
		return `${count} ${units === 1 ? 'unit' : 'units'}`;
	}

	function setQuantity(productId: string, raw: string): void {
		const parsed = Number(raw);
		// No clamping to the returnable maximum: the server owns that check and
		// its 409 detail is what the operator sees.
		quantities[productId] = Number.isFinite(parsed) ? Math.max(0, parsed) : 0;
	}

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		if (!canSubmit || !credits) return;

		error = null;
		busy = true;
		try {
			const items = Object.entries(quantities)
				.filter(
					([productId, quantity]) =>
						credits.items.some((line) => line.product_id === productId) && quantity > 0
				)
				.map(([productId, quantity]) => ({ product_id: productId, quantity: String(quantity) }));

			const note = await api.createCreditNote(data.saleId, { reason: reason.trim(), items });
			created = note;
			reason = '';
			for (const productId of Object.keys(quantities)) quantities[productId] = 0;
			// The notes list and the returnable quantities both come from the
			// load, so one invalidation refreshes the whole screen.
			await invalidateAll();
		} catch (e) {
			created = null;
			error = failure(e, 'Could not issue the credit note.');
		} finally {
			busy = false;
		}
	}
</script>

{#if data.notFound || !credits}
	<div class="mb-4">
		<h1 class="text-xl font-semibold tracking-tight">Credit note</h1>
	</div>
	<div class="rounded-lg border border-border p-10 text-center text-sm text-muted-foreground" role="status">
		Sale not found, or it has no charged items to return. Only charged sales can be credited.
	</div>
	<div class="mt-4">
		<Button variant="outline" href="/operations/sales">Back to sales</Button>
	</div>
{:else}
	<div class="mb-4">
		<h1 class="text-xl font-semibold tracking-tight">Credit note</h1>
		<p class="mt-1 text-sm text-muted-foreground">
			Sale <span class="font-mono text-xs" title={data.saleId}>{data.saleId.slice(0, 8)}</span> —
			return charged goods and the server calculates the amount and the refunds.
		</p>
	</div>

	{#if error}
		<p class="mb-4 text-sm text-destructive" role="alert">{error}</p>
	{/if}

	{#if created}
		<div class="issued mb-6 rounded-lg border border-border p-4 text-sm" role="status">
			<strong>Credit note issued.</strong> Total {created.total}.
			<ul class="mt-2 space-y-0.5 text-muted-foreground">
				{#each created.refunds as refund (refund.wallet_id + refund.payment_method_id)}
					<li>{refund.wallet_name} ({refund.payment_method_name}): {refund.amount}</li>
				{/each}
			</ul>
		</div>
	{/if}

	<section class="mb-6 grid gap-3 sm:grid-cols-2">
		<div class="rounded-lg border border-border p-4">
			<div class="text-xs text-muted-foreground">Sale total</div>
			<div class="mt-1 text-lg font-semibold tabular-nums">{credits.sale_total}</div>
		</div>
		<div class="rounded-lg border border-border p-4">
			<div class="text-xs text-muted-foreground">Already refunded</div>
			<div class="mt-1 text-lg font-semibold tabular-nums">{credits.refunded_total}</div>
		</div>
	</section>

	<form onsubmit={submit} novalidate>
		<div class="mb-4 overflow-hidden rounded-lg border border-border">
			<Table>
				<TableHeader>
					<TableRow>
						<TableHead>Product</TableHead>
						<TableHead class="text-right">Unit price</TableHead>
						<TableHead class="text-right">Sold</TableHead>
						<TableHead class="text-right">Already credited</TableHead>
						<TableHead>Return now</TableHead>
					</TableRow>
				</TableHeader>
				<TableBody>
					{#each credits.items as line (line.product_id)}
						{@const fullyReturned = line.returnable_quantity === '0'}
						<TableRow
							id={`return-line-${line.product_id}`}
							class={fullyReturned ? 'opacity-60' : ''}
						>
							<TableCell class="font-medium">{line.product_name}</TableCell>
							<TableCell class="text-right tabular-nums">{line.unit_price}</TableCell>
							<TableCell class="text-right tabular-nums">{line.quantity}</TableCell>
							<TableCell class="text-right tabular-nums">{line.credited_quantity}</TableCell>
							<TableCell>
								<div class="flex flex-wrap items-center gap-2">
									<Input
										id={`return-qty-${line.product_id}`}
										type="number"
										min="0"
										max={line.returnable_quantity}
										step="1"
										value={quantities[line.product_id] ?? 0}
										disabled={busy || fullyReturned}
										class="w-24"
										aria-label={`Units to return of ${line.product_name}`}
										oninput={(event) => setQuantity(line.product_id, event.currentTarget.value)}
									/>
									{#if fullyReturned}
										<Badge variant="secondary">Fully returned</Badge>
									{:else}
										<span class="text-xs text-muted-foreground tabular-nums">
											{line.returnable_quantity} left
										</span>
									{/if}
								</div>
							</TableCell>
						</TableRow>
					{/each}
				</TableBody>
			</Table>
		</div>

		<div class="mb-4 grid max-w-xl gap-1">
			<Label for="return-reason">Reason</Label>
			<textarea
				id="return-reason"
				rows="3"
				maxlength="200"
				bind:value={reason}
				placeholder="Why are these goods coming back?"
				class="flex min-h-20 w-full rounded-md border border-input bg-transparent px-2.5 py-2 text-sm shadow-xs outline-none placeholder:text-muted-foreground focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50"
			></textarea>
			<span id="reason-counter" class="text-xs text-muted-foreground tabular-nums">
				{reason.length}/200 characters
			</span>
		</div>

		<p id="return-summary" class="mb-4 rounded-md border border-border bg-muted/30 p-3 text-sm">
			You are returning {unitLabel(selectedUnits)} — the server will calculate the amount.
		</p>

		<Button type="submit" disabled={!canSubmit}>
			{busy ? 'Issuing…' : 'Issue credit note'}
		</Button>
	</form>

	<section class="mt-8">
		<div class="mb-3 flex items-center justify-between gap-4">
			<h2 class="text-lg font-semibold">Credit notes</h2>
			<span class="text-sm text-muted-foreground">
				{credits.credit_notes.length}
				{credits.credit_notes.length === 1 ? 'note' : 'notes'}
			</span>
		</div>

		{#if credits.credit_notes.length === 0}
			<div class="rounded-lg border border-border p-10 text-center text-sm text-muted-foreground">
				No credit notes have been issued for this sale yet.
			</div>
		{:else}
			<div class="overflow-hidden rounded-lg border border-border">
				<Table id="credit-notes-table">
					<TableHeader>
						<TableRow>
							<TableHead>When</TableHead>
							<TableHead>Reason</TableHead>
							<TableHead class="text-right">Total</TableHead>
							<TableHead>Lines</TableHead>
							<TableHead>Refunds</TableHead>
						</TableRow>
					</TableHeader>
					<TableBody>
						{#each credits.credit_notes as note (note.id)}
							<TableRow>
								<TableCell class="text-muted-foreground">{when(note.created_at)}</TableCell>
								<TableCell class="whitespace-normal">{note.reason}</TableCell>
								<TableCell class="text-right font-medium tabular-nums">{note.total}</TableCell>
								<TableCell class="whitespace-normal">
									<ul class="space-y-0.5 text-xs">
										{#each note.items as item (item.product_id)}
											<li>{item.product_name}: {item.quantity} × {item.unit_price} = {item.subtotal}</li>
										{/each}
									</ul>
								</TableCell>
								<TableCell class="whitespace-normal">
									<ul class="space-y-0.5 text-xs">
										{#each note.refunds as refund (refund.wallet_id + refund.payment_method_id)}
											<li>{refund.wallet_name}: {refund.amount}</li>
										{/each}
									</ul>
								</TableCell>
							</TableRow>
						{/each}
					</TableBody>
				</Table>
			</div>
		{/if}
	</section>
{/if}
