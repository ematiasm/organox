<script lang="ts">
	import { api, ApiError, type Supplier } from '$lib/api/client';
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
		supplier,
		onClose,
		onSaved,
		onCreated
	}: {
		/** Driven by `?id=` in the URL: true whenever a supplier is selected. */
		open: boolean;
		/** Driven by `?new=1` in the URL: opens empty for creation. */
		creating: boolean;
		supplier: Supplier | null;
		onClose: () => void;
		onSaved: () => Promise<void>;
		onCreated: (id: string) => void;
	} = $props();

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

	function seededForm(target: Supplier): Form {
		return {
			name: target.name,
			tax_id: target.tax_id ?? '',
			email: target.email ?? '',
			phone: target.phone ?? ''
		};
	}

	$effect(() => {
		open && supplier?.id;
		editing = false;
		confirmDeactivate = false;
		formError = null;
		form = supplier && !creating ? seededForm(supplier) : emptyForm();
	});

	function startEdit() {
		if (!supplier) return;
		form = seededForm(supplier);
		formError = null;
		editing = true;
	}

	function cancelEdit() {
		if (!supplier) return;
		form = seededForm(supplier);
		formError = null;
		editing = false;
	}

	function failure(error: unknown, fallback: string): string {
		if (error instanceof ApiError) {
			return error.problem?.detail ?? `The server rejected the change (HTTP ${error.status}).`;
		}
		return fallback;
	}

	async function saveEdit() {
		if (!supplier) return;
		saving = true;
		formError = null;
		try {
			await api.updateSupplier(supplier.id, {
				name: form.name,
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
			const { id } = await api.createSupplier({
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
		if (!supplier) return;
		deactivating = true;
		formError = null;
		try {
			await api.deactivateSupplier(supplier.id);
			await onSaved();
			onClose();
		} catch (error) {
			formError = failure(error, 'Could not reach the server. Try again.');
		} finally {
			deactivating = false;
			confirmDeactivate = false;
		}
	}

	function when(iso: string): string {
		const date = new Date(iso);
		return Number.isNaN(date.getTime())
			? iso
			: date.toLocaleString('en-US', { dateStyle: 'short', timeStyle: 'short' });
	}

	function optionalPlaceholder(): string {
		return creating ? 'Optional' : 'Unchanged when empty';
	}
</script>

<Sheet open={open} onOpenChange={(o) => !o && onClose()}>
	<SheetContent side="right" class="overflow-y-auto sm:max-w-xl">
		{#if creating || (supplier && editing)}
			<SheetHeader>
				<SheetTitle>{creating ? 'New supplier' : 'Edit supplier'}</SheetTitle>
				<SheetDescription>
					{creating
						? 'The server validates every field; its answer is the error message.'
						: supplier?.name}
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
					<Label for="supplier-name">Name</Label>
					<Input id="supplier-name" bind:value={form.name} required maxlength={200} />
				</div>

				<div class="grid gap-2">
					<Label for="supplier-tax-id">Document</Label>
					<Input
						id="supplier-tax-id"
						bind:value={form.tax_id}
						maxlength={32}
						placeholder={optionalPlaceholder()}
					/>
				</div>

				<div class="grid gap-2">
					<Label for="supplier-email">Email</Label>
					<Input
						id="supplier-email"
						bind:value={form.email}
						maxlength={254}
						placeholder={optionalPlaceholder()}
					/>
				</div>

				<div class="grid gap-2">
					<Label for="supplier-phone">Phone</Label>
					<Input
						id="supplier-phone"
						bind:value={form.phone}
						maxlength={40}
						placeholder={optionalPlaceholder()}
					/>
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
						{saving ? 'Saving…' : creating ? 'Create supplier' : 'Save changes'}
					</Button>
				</div>
			</form>
		{:else if supplier}
			<SheetHeader>
				<SheetTitle>{supplier.name}</SheetTitle>
				<SheetDescription>{supplier.tax_id ?? 'No document'}</SheetDescription>
			</SheetHeader>

			<div class="flex items-center gap-2 px-4">
				<Button variant="outline" size="sm" onclick={startEdit}>Edit</Button>
				{#if supplier.is_active}
					{#if confirmDeactivate}
						<Button
							variant="destructive"
							size="sm"
							disabled={deactivating}
							onclick={deactivate}
						>
							{deactivating ? 'Deactivating…' : 'Confirm deactivation'}
						</Button>
						<Button variant="ghost" size="sm" onclick={() => (confirmDeactivate = false)}>
							Cancel
						</Button>
					{:else}
						<Button
							variant="destructive"
							size="sm"
							onclick={() => (confirmDeactivate = true)}>Deactivate</Button
						>
					{/if}
				{/if}
				{#if formError}
					<p class="text-sm text-destructive">{formError}</p>
				{/if}
			</div>

			<dl class="grid grid-cols-[7rem_1fr] gap-x-4 gap-y-2 px-4 text-sm">
				<dt class="text-muted-foreground">Document</dt>
				<dd>{supplier.tax_id ?? '—'}</dd>
				<dt class="text-muted-foreground">Email</dt>
				<dd>{supplier.email ?? '—'}</dd>
				<dt class="text-muted-foreground">Phone</dt>
				<dd>{supplier.phone ?? '—'}</dd>
				<dt class="text-muted-foreground">Status</dt>
				<dd>
					<Badge variant={supplier.is_active ? 'outline' : 'secondary'}>
						{supplier.is_active ? 'Active' : 'Inactive'}
					</Badge>
				</dd>
				<dt class="text-muted-foreground">Created</dt>
				<dd class="text-muted-foreground">{when(supplier.created_at)}</dd>
				<dt class="text-muted-foreground">Updated</dt>
				<dd class="text-muted-foreground">{when(supplier.updated_at)}</dd>
			</dl>
		{:else}
			<SheetHeader>
				<SheetTitle>Supplier not found</SheetTitle>
				<SheetDescription>This supplier does not exist. The link may be stale.</SheetDescription>
			</SheetHeader>
		{/if}
	</SheetContent>
</Sheet>
