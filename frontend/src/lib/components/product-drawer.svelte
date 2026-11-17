<script lang="ts">
	import { api, ApiError, type Movement, type Product, type ProductKind } from '$lib/api/client';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import * as Select from '$lib/components/ui/select';
	import {
		Sheet,
		SheetContent,
		SheetDescription,
		SheetHeader,
		SheetTitle
	} from '$lib/components/ui/sheet';

	let {
		open,
		creating,
		product,
		movements,
		onClose,
		onSaved,
		onCreated
	}: {
		/** Driven by `?id=` in the URL: true whenever a product is selected. */
		open: boolean;
		/** Driven by `?new` in the URL: the drawer opens empty, saving a POST. */
		creating: boolean;
		product: Product | null;
		movements: Movement[];
		/** Close the drawer: the page navigates the `?id`/`?new` params away. */
		onClose: () => void;
		/** A mutation landed: the page invalidates its load data. */
		onSaved: () => Promise<void>;
		/** Create landed: the page replaces `?new` with `?id=<uuid>`. */
		onCreated: (id: string) => void;
	} = $props();

	/** Map API reason values to display labels (same copy as the movements screen). */
	const REASONS: Record<Movement['reason'], string> = {
		purchase: 'Purchase',
		sale: 'Sale',
		adjustment: 'Adjustment',
		loss: 'Loss'
	};

	function kindLabel(kind: ProductKind): string {
		return kind === 'service' ? 'Service' : 'Product';
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

	// --- Form state ---------------------------------------------------------
	//
	// Edit mode is transient component state, not URL state: it is not
	// shareable, and Back closes the whole drawer (the URL pops the `id`).
	// Create mode IS URL state (`?new`) for the same reason as `?id`: Back
	// closes it and the page link stays clean.

	type Form = {
		kind: ProductKind;
		name: string;
		sku: string;
		description: string;
		price: string;
		min_stock: string;
	};

	let editing = $state(false);
	let saving = $state(false);
	let deactivating = $state(false);
	let confirmDeactivate = $state(false);
	let formError = $state<string | null>(null);
	let form = $state<Form>(emptyForm());

	function emptyForm(): Form {
		return { kind: 'product', name: '', sku: '', description: '', price: '', min_stock: '' };
	}

	/** Seed the form from the product; blank strings for what is `null`. */
	function seededForm(product: Product): Form {
		return {
			kind: product.kind,
			name: product.name,
			sku: product.sku ?? '',
			description: product.description ?? '',
			price: product.price ?? '',
			min_stock: product.min_stock ?? ''
		};
	}

	// Reset the transient state whenever the drawer closes or switches target:
	// a stale "editing" or a half-confirmed deactivation must not survive a
	// reopen on another product.
	$effect(() => {
		open && product?.id;
		editing = false;
		confirmDeactivate = false;
		formError = null;
		form = product && !creating ? seededForm(product) : emptyForm();
	});

	function startEdit() {
		if (!product) return;
		form = seededForm(product);
		formError = null;
		editing = true;
	}

	function cancelEdit() {
		if (!product) return;
		form = seededForm(product);
		formError = null;
		editing = false;
	}

	/** The server is the only validator; its `problem.detail` is the message. */
	function failure(error: unknown, fallback: string): string {
		if (error instanceof ApiError) {
			return error.problem?.detail ?? `The server rejected the change (HTTP ${error.status}).`;
		}
		return fallback;
	}

	async function saveEdit() {
		if (!product) return;
		saving = true;
		formError = null;
		try {
			await api.updateProduct(product.id, {
				name: form.name,
				sku: form.sku || undefined,
				description: form.description,
				// The API cannot clear a price or a min stock (COALESCE on PATCH):
				// an empty input leaves them unchanged instead of pretending to clear.
				price: form.price || undefined,
				min_stock: product.kind === 'service' ? undefined : form.min_stock || undefined
			});
			editing = false;
			await onSaved();
		} catch (error) {
			formError = failure(error, 'Could not reach the server. Try again.');
		} finally {
			saving = false;
		}
	}

	async function saveCreate() {
		saving = true;
		formError = null;
		try {
			const { id } = await api.createProduct({
				kind: form.kind,
				name: form.name,
				sku: form.sku || undefined,
				description: form.description || undefined,
				price: form.price || undefined,
				min_stock: form.kind === 'service' ? undefined : form.min_stock || undefined
			});
			onCreated(id);
		} catch (error) {
			formError = failure(error, 'Could not reach the server. Try again.');
		} finally {
			saving = false;
		}
	}

	async function deactivate() {
		if (!product) return;
		deactivating = true;
		formError = null;
		try {
			// Idempotent: pressing twice is a duplicate no-op, and the drawer
			// closes after the first success anyway.
			await api.deactivateProduct(product.id);
			await onSaved();
			onClose();
		} catch (error) {
			formError = failure(error, 'Could not reach the server. Try again.');
		} finally {
			deactivating = false;
			confirmDeactivate = false;
		}
	}

	/** A service tracks no stock, so the server rejects a min stock for it. */
	let tracksStock = $derived(creating ? form.kind !== 'service' : product?.kind !== 'service');
</script>

<Sheet open={open} onOpenChange={(o) => !o && onClose()}>
	<SheetContent side="right" class="overflow-y-auto sm:max-w-md">
		{#if creating || (product && editing)}
			<SheetHeader>
				<SheetTitle>{creating ? 'New product' : 'Edit product'}</SheetTitle>
				<SheetDescription>
					{creating
						? 'The server validates every field; its answer is the error message.'
						: product?.name}
				</SheetDescription>
			</SheetHeader>

			<form
				class="grid gap-4 px-4"
				onsubmit={(e) => {
					e.preventDefault();
					creating ? saveCreate() : saveEdit();
				}}
			>
				{#if creating}
					<div class="grid gap-2">
						<Label for="kind">Type</Label>
						<Select.Root type="single" bind:value={form.kind}>
							<Select.Trigger id="kind" class="w-full">
								{kindLabel(form.kind)}
							</Select.Trigger>
							<Select.Content>
								<Select.Item value="product" label="Product" />
								<Select.Item value="service" label="Service" />
							</Select.Content>
						</Select.Root>
					</div>
				{/if}

				<div class="grid gap-2">
					<Label for="name">Name</Label>
					<Input id="name" bind:value={form.name} required maxlength={120} />
				</div>

				<div class="grid gap-2">
					<Label for="sku">SKU</Label>
					<!-- Empty keeps the current SKU on edit (the API cannot clear it) and
					     omits it on create. The server trims and enforces uniqueness. -->
					<Input id="sku" bind:value={form.sku} maxlength={40} placeholder="Optional" />
				</div>

				<div class="grid gap-2">
					<Label for="price">Price</Label>
					<Input
						id="price"
						bind:value={form.price}
						inputmode="decimal"
						placeholder={creating ? 'Optional' : 'Unchanged when empty'}
					/>
				</div>

				{#if tracksStock}
					<div class="grid gap-2">
						<Label for="min_stock">Min stock</Label>
						<Input
							id="min_stock"
							bind:value={form.min_stock}
							inputmode="decimal"
							placeholder={creating ? 'Optional low-stock threshold' : 'Unchanged when empty'}
						/>
					</div>
				{/if}

				<div class="grid gap-2">
					<Label for="description">Description</Label>
					<textarea
						id="description"
						bind:value={form.description}
						rows="3"
						class="flex min-h-20 w-full rounded-md border border-input bg-transparent px-3 py-2
							text-sm shadow-xs placeholder:text-muted-foreground focus-visible:outline-none
							focus-visible:ring-2 focus-visible:ring-ring/50"
					></textarea>
				</div>

				{#if formError}
					<p class="text-sm text-destructive">{formError}</p>
				{/if}

				<div class="flex justify-end gap-2">
					<Button
						type="button"
						variant="outline"
						onclick={() => (creating ? onClose() : cancelEdit())}>Cancel</Button
					>
					<Button type="submit" disabled={saving}>
						{saving ? 'Saving…' : creating ? 'Create product' : 'Save changes'}
					</Button>
				</div>
			</form>
		{:else if product}
			<SheetHeader>
				<SheetTitle>{product.name}</SheetTitle>
				<SheetDescription>{product.sku ?? 'No SKU'}</SheetDescription>
			</SheetHeader>

			<div class="flex items-center gap-2 px-4">
				<Button variant="outline" size="sm" onclick={startEdit}>Edit</Button>
				{#if confirmDeactivate}
					<Button variant="destructive" size="sm" disabled={deactivating} onclick={deactivate}>
						{deactivating ? 'Deactivating…' : 'Confirm deactivation'}
					</Button>
					<Button variant="ghost" size="sm" onclick={() => (confirmDeactivate = false)}>
						Cancel
					</Button>
				{:else}
					<!-- Two steps instead of a dialog: one extra click, no new component,
					     and the destructive button stays visible as the price of the action. -->
					<Button
						variant="destructive"
						size="sm"
						onclick={() => (confirmDeactivate = true)}>Deactivate</Button
					>
				{/if}
				{#if formError}
					<p class="text-sm text-destructive">{formError}</p>
				{/if}
			</div>

			<dl class="grid grid-cols-[7rem_1fr] gap-x-4 gap-y-2 px-4 text-sm">
				<dt class="text-muted-foreground">Type</dt>
				<dd>{kindLabel(product.kind)}</dd>
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
