<script lang="ts">
	import { api, ApiError, type Supplier } from '$lib/api/client';
	import SupplierAccountPanel from '$lib/components/supplier-account-panel.svelte';
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
		credit_limit: string;
	};

	let editing = $state(false);
	let saving = $state(false);
	let deactivating = $state(false);
	let confirmDeactivate = $state(false);
	let formError = $state<string | null>(null);
	let form = $state<Form>(emptyForm());

	function emptyForm(): Form {
		return { name: '', tax_id: '', email: '', phone: '', credit_limit: '' };
	}

	function seededForm(target: Supplier): Form {
		return {
			name: target.name,
			tax_id: target.tax_id ?? '',
			email: target.email ?? '',
			phone: target.phone ?? '',
			credit_limit: target.credit_limit ?? ''
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
			const trimmedLimit = form.credit_limit.trim();
			// Tri-state PATCH contract (S4): explicit `null` clears an existing
			// limit back to unlimited; omitting keeps it unchanged; a value sets it.
			let creditLimit: string | null | undefined;
			if (trimmedLimit !== '') {
				creditLimit = trimmedLimit;
			} else if (supplier.credit_limit !== null && supplier.credit_limit !== undefined) {
				creditLimit = null;
			}

			await api.updateSupplier(supplier.id, {
				name: form.name,
				tax_id: form.tax_id || undefined,
				email: form.email || undefined,
				phone: form.phone || undefined,
				credit_limit: creditLimit
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
			const trimmedLimit = form.credit_limit.trim();
			const { id } = await api.createSupplier({
				name: form.name,
				tax_id: form.tax_id || undefined,
				email: form.email || undefined,
				phone: form.phone || undefined,
				credit_limit: trimmedLimit || undefined
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

	function creditLimitLabel(limit: string | null | undefined): string {
		if (limit === null || limit === undefined) return 'Unlimited';
		if (limit === '0.00' || limit === '0') return '0.00 (cash only)';
		return limit;
	}
</script>

<Sheet open={open} onOpenChange={(o) => !o && onClose()}>
	<SheetContent side="right" class="overflow-y-auto sm:max-w-xl lg:max-w-2xl">
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

				<div class="grid gap-2">
					<Label for="supplier-credit-limit">Credit limit</Label>
					<Input
						id="supplier-credit-limit"
						type="text"
						inputmode="decimal"
						bind:value={form.credit_limit}
						placeholder="Unlimited (leave empty)"
						class="tabular-nums"
					/>
					<p class="text-xs text-muted-foreground">
						Leave empty for no limit; enter 0 for cash only.
					</p>
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
			<SheetHeader class="gap-3 border-b border-border pb-4">
				<div class="flex items-center gap-2 pr-8">
					<SheetTitle>{supplier.name}</SheetTitle>
					<Badge variant={supplier.is_active ? 'outline' : 'secondary'}>
						{supplier.is_active ? 'Active' : 'Inactive'}
					</Badge>
				</div>

				<div class="rounded-lg border border-border bg-muted/30 p-3">
					<div class="mb-2.5 flex flex-wrap items-center justify-between gap-2">
						<span class="text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">
							Supplier details
						</span>
						<div class="flex items-center gap-1.5">
							<Button variant="outline" size="xs" onclick={startEdit}>Edit</Button>
							{#if supplier.is_active}
								{#if confirmDeactivate}
									<Button
										variant="destructive"
										size="xs"
										disabled={deactivating}
										onclick={deactivate}
									>
										{deactivating ? 'Deactivating…' : 'Confirm deactivation'}
									</Button>
									<Button variant="ghost" size="xs" onclick={() => (confirmDeactivate = false)}>
										Cancel
									</Button>
								{:else}
									<Button
										variant="destructive"
										size="xs"
										onclick={() => (confirmDeactivate = true)}>Deactivate</Button
									>
								{/if}
							{/if}
						</div>
					</div>

					{#if formError}
						<p class="mb-2 text-xs text-destructive" role="alert">{formError}</p>
					{/if}

					<dl class="grid grid-cols-2 gap-x-4 gap-y-2 text-xs sm:grid-cols-3">
						<div class="min-w-0">
							<dt class="text-[11px] text-muted-foreground">Document</dt>
							<dd class="truncate font-medium">{supplier.tax_id ?? '—'}</dd>
						</div>
						<div class="min-w-0">
							<dt class="text-[11px] text-muted-foreground">Email</dt>
							<dd class="truncate">{supplier.email ?? '—'}</dd>
						</div>
						<div class="min-w-0">
							<dt class="text-[11px] text-muted-foreground">Phone</dt>
							<dd class="truncate">{supplier.phone ?? '—'}</dd>
						</div>
						<div class="min-w-0">
							<dt class="text-[11px] text-muted-foreground">Credit limit</dt>
							<dd class="truncate tabular-nums font-medium">
								{creditLimitLabel(supplier.credit_limit)}
							</dd>
						</div>
						<div class="min-w-0">
							<dt class="text-[11px] text-muted-foreground">Created</dt>
							<dd class="truncate text-muted-foreground">{when(supplier.created_at)}</dd>
						</div>
						<div class="min-w-0">
							<dt class="text-[11px] text-muted-foreground">Updated</dt>
							<dd class="truncate text-muted-foreground">{when(supplier.updated_at)}</dd>
						</div>
					</dl>
				</div>
			</SheetHeader>

			<SupplierAccountPanel
				supplierId={supplier.id}
				balance={supplier.balance}
				availableCredit={supplier.available_credit}
				onMutated={onSaved}
			/>
		{:else}
			<SheetHeader>
				<SheetTitle>Supplier not found</SheetTitle>
				<SheetDescription>This supplier does not exist. The link may be stale.</SheetDescription>
			</SheetHeader>
		{/if}
	</SheetContent>
</Sheet>
