<script lang="ts">
	import { api, ApiError, type Customer } from '$lib/api/client';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
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
		customer,
		onClose,
		onSaved,
		onCreated
	}: {
		/** Driven by `?id=` in the URL: true whenever a customer is selected. */
		open: boolean;
		/** Driven by `?new` in the URL: the drawer opens empty, saving a POST. */
		creating: boolean;
		customer: Customer | null;
		/** Close the drawer: the page navigates the `?id`/`?new` params away. */
		onClose: () => void;
		/** A mutation landed: the page invalidates its load data. */
		onSaved: () => Promise<void>;
		/** Create landed: the page replaces `?new` with `?id=<uuid>`. */
		onCreated: (id: string) => void;
	} = $props();

	// --- Form state ---------------------------------------------------------
	//
	// Edit mode is transient component state, not URL state: it is not
	// shareable, and Back closes the whole drawer (the URL pops the `id`).
	// Create mode IS URL state (`?new`) for the same reason as `?id`.

	type Form = {
		name: string;
		tax_id: string;
		email: string;
		phone: string;
	};

	let editing = $state(false);
	let saving = $state(false);
	let deactivating = $state(false);
	let confirmDeactivate = $state(false);
	let formError = $state<string | null>(null);
	let form = $state<Form>(emptyForm());

	function emptyForm(): Form {
		return { name: '', tax_id: '', email: '', phone: '' };
	}

	/** Seed the form from the customer; blank strings for what is `null`. */
	function seededForm(customer: Customer): Form {
		return {
			name: customer.name,
			tax_id: customer.tax_id ?? '',
			email: customer.email ?? '',
			phone: customer.phone ?? ''
		};
	}

	// Reset the transient state whenever the drawer closes or switches target:
	// a stale "editing" or a half-confirmed deactivation must not survive a
	// reopen on another customer.
	$effect(() => {
		open && customer?.id;
		editing = false;
		confirmDeactivate = false;
		formError = null;
		form = customer && !creating ? seededForm(customer) : emptyForm();
	});

	function startEdit() {
		if (!customer) return;
		form = seededForm(customer);
		formError = null;
		editing = true;
	}

	function cancelEdit() {
		if (!customer) return;
		form = seededForm(customer);
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
		if (!customer) return;
		saving = true;
		formError = null;
		try {
			await api.updateCustomer(customer.id, {
				name: form.name,
				// The API cannot clear a document, an email or a phone (COALESCE on
				// PATCH): an empty input leaves the field unchanged instead of
				// pretending to clear, and an empty string would be rejected.
				tax_id: form.tax_id || undefined,
				email: form.email || undefined,
				phone: form.phone || undefined
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
			const { id } = await api.createCustomer({
				name: form.name,
				tax_id: form.tax_id || undefined,
				email: form.email || undefined,
				phone: form.phone || undefined
			});
			onCreated(id);
		} catch (error) {
			formError = failure(error, 'Could not reach the server. Try again.');
		} finally {
			saving = false;
		}
	}

	async function deactivate() {
		if (!customer) return;
		deactivating = true;
		formError = null;
		try {
			// Idempotent: pressing twice is a duplicate no-op, and the drawer
			// closes after the first success anyway.
			await api.deactivateCustomer(customer.id);
			await onSaved();
			onClose();
		} catch (error) {
			formError = failure(error, 'Could not reach the server. Try again.');
		} finally {
			deactivating = false;
			confirmDeactivate = false;
		}
	}

	/** `en-US` per the repo language convention; same format as products. */
	function when(iso: string): string {
		const date = new Date(iso);
		return Number.isNaN(date.getTime())
			? iso
			: date.toLocaleString('en-US', { dateStyle: 'short', timeStyle: 'short' });
	}

	/** Edit placeholders differ from create ones: PATCH keeps what is empty. */
	function optionalPlaceholder(): string {
		return creating ? 'Optional' : 'Unchanged when empty';
	}
</script>

<Sheet open={open} onOpenChange={(o) => !o && onClose()}>
	<SheetContent side="right" class="overflow-y-auto sm:max-w-md">
		{#if creating || (customer && editing)}
			<SheetHeader>
				<SheetTitle>{creating ? 'New customer' : 'Edit customer'}</SheetTitle>
				<SheetDescription>
					{creating
						? 'The server validates every field; its answer is the error message.'
						: customer?.name}
				</SheetDescription>
			</SheetHeader>

			<form
				class="grid gap-4 px-4"
				onsubmit={(e) => {
					e.preventDefault();
					creating ? saveCreate() : saveEdit();
				}}
			>
				<div class="grid gap-2">
					<Label for="name">Name</Label>
					<Input id="name" bind:value={form.name} required maxlength={120} />
				</div>

				<div class="grid gap-2">
					<Label for="tax_id">Document</Label>
					<!-- Empty keeps the current document on edit (the API cannot clear
					     it) and omits it on create. The server enforces uniqueness. -->
					<Input id="tax_id" bind:value={form.tax_id} maxlength={32} placeholder={optionalPlaceholder()} />
				</div>

				<div class="grid gap-2">
					<Label for="email">Email</Label>
					<Input id="email" bind:value={form.email} maxlength={120} placeholder={optionalPlaceholder()} />
				</div>

				<div class="grid gap-2">
					<Label for="phone">Phone</Label>
					<Input id="phone" bind:value={form.phone} maxlength={40} placeholder={optionalPlaceholder()} />
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
						{saving ? 'Saving…' : creating ? 'Create customer' : 'Save changes'}
					</Button>
				</div>
			</form>
		{:else if customer}
			<SheetHeader>
				<SheetTitle>{customer.name}</SheetTitle>
				<SheetDescription>{customer.tax_id ?? 'No document'}</SheetDescription>
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
				<dt class="text-muted-foreground">Document</dt>
				<dd>{customer.tax_id ?? '—'}</dd>
				<dt class="text-muted-foreground">Email</dt>
				<dd>{customer.email ?? '—'}</dd>
				<dt class="text-muted-foreground">Phone</dt>
				<dd>{customer.phone ?? '—'}</dd>
				<dt class="text-muted-foreground">Status</dt>
				<dd>
					<Badge variant={customer.is_active ? 'outline' : 'secondary'}>
						{customer.is_active ? 'Active' : 'Inactive'}
					</Badge>
				</dd>
				<dt class="text-muted-foreground">Created</dt>
				<dd class="text-muted-foreground">{when(customer.created_at)}</dd>
				<dt class="text-muted-foreground">Updated</dt>
				<dd class="text-muted-foreground">{when(customer.updated_at)}</dd>
			</dl>
		{:else}
			<SheetHeader>
				<SheetTitle>Customer not found</SheetTitle>
				<SheetDescription>This customer does not exist. The link may be stale.</SheetDescription>
			</SheetHeader>
		{/if}
	</SheetContent>
</Sheet>
