<script lang="ts">
	import { invalidateAll } from '$app/navigation';
	import {
		api,
		ApiError,
		type PaymentMethod,
		type Wallet,
		type WalletMovement,
		type WalletMovementKind
	} from '$lib/api/client';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';
	import * as Select from '$lib/components/ui/select';
	import { cn } from '$lib/utils';
	import {
		Table,
		TableBody,
		TableCell,
		TableHead,
		TableHeader,
		TableRow
	} from '$lib/components/ui/table';

	let { data } = $props();

	// `sale_payment` and `sale_refund` are deliberately absent: that money only
	// moves by charging a sale or returning one. The server rejects them here,
	// and the form does not offer them.
	type ManualKind = Exclude<
		WalletMovementKind,
		| 'sale_payment'
		| 'sale_refund'
		| 'account_payment'
		| 'account_payment_refund'
		| 'purchase_payment'
		| 'supplier_account_payment'
		| 'supplier_account_refund'
	>;
	const MANUAL_KINDS: { value: ManualKind; label: string }[] = [
		{ value: 'expense', label: 'Expense' },
		{ value: 'adjustment_in', label: 'Adjustment in' },
		{ value: 'adjustment_out', label: 'Adjustment out' }
	];
	const KIND_LABELS: Record<WalletMovementKind, string> = {
		sale_payment: 'Sale payment',
		sale_refund: 'Sale refund',
		account_payment: 'Account payment',
		account_payment_refund: 'Credit refund',
		purchase_payment: 'Purchase payment',
		supplier_account_payment: 'Supplier account payment',
		supplier_account_refund: 'Supplier credit refund',
		expense: 'Expense',
		adjustment_in: 'Adjustment in',
		adjustment_out: 'Adjustment out'
	};
	// The endpoint caps at 200; 50 is the same recent-history window the product
	// drawer shows. A full page is disclosed as possibly incomplete.
	const MOVEMENT_LIMIT = 50;

	/** The server is the only validator: its `detail` is the error message. */
	function failure(error: unknown, fallback: string): string {
		if (error instanceof ApiError) {
			return error.problem?.detail ?? `The server rejected the change (HTTP ${error.status}).`;
		}
		return fallback;
	}

	function walletLabel(wallet: Wallet): string {
		return wallet.is_active ? wallet.name : `${wallet.name} (inactive)`;
	}

	function when(iso: string): string {
		const date = new Date(iso);
		return Number.isNaN(date.getTime())
			? iso
			: date.toLocaleString('en-US', { dateStyle: 'short', timeStyle: 'short' });
	}

	/** `signed_amount` comes from the server; the client only colors negatives. */
	function isNegative(amount: string): boolean {
		return amount.trim().startsWith('-');
	}

	async function run(
		action: () => Promise<void>,
		setError: (message: string | null) => void,
		fallback: string
	) {
		if (busy) return;
		busy = true;
		setError(null);
		try {
			await action();
		} catch (error) {
			setError(failure(error, fallback));
		} finally {
			busy = false;
		}
	}

	// --- Wallet state ---
	let busy = $state(false);
	let createWalletName = $state('');
	let walletError = $state<string | null>(null);
	let editWalletId = $state<string | null>(null);
	let editWalletName = $state('');
	let deactivateWalletId = $state<string | null>(null);
	let openWalletId = $state<string | null>(null);
	let movements = $state<Record<string, WalletMovement[]>>({});
	let movementsLoading = $state(false);
	let movementsError = $state<string | null>(null);
	let movementKind = $state<ManualKind>('expense');
	let movementAmount = $state('');
	let movementNote = $state('');
	let movementError = $state<string | null>(null);

	// --- Payment method state ---
	let createMethodName = $state('');
	let createMethodWalletId = $state('');
	let methodError = $state<string | null>(null);
	let editMethodId = $state<string | null>(null);
	let editMethodName = $state('');
	let editMethodWalletId = $state('');
	let deactivateMethodId = $state<string | null>(null);

	function startWalletEdit(wallet: Wallet) {
		editMethodId = null;
		deactivateWalletId = null;
		editWalletId = wallet.id;
		editWalletName = wallet.name;
		walletError = null;
	}

	async function createWallet(event: SubmitEvent) {
		event.preventDefault();
		await run(async () => {
			await api.createWallet(createWalletName);
			createWalletName = '';
			await invalidateAll();
		}, (message) => (walletError = message), 'Could not create the wallet.');
	}

	async function saveWallet(wallet: Wallet, event: SubmitEvent) {
		event.preventDefault();
		await run(async () => {
			await api.updateWallet(wallet.id, { name: editWalletName });
			editWalletId = null;
			await invalidateAll();
		}, (message) => (walletError = message), 'Could not rename the wallet.');
	}

	async function setWalletActive(wallet: Wallet, isActive: boolean) {
		await run(async () => {
			await api.updateWallet(wallet.id, { is_active: isActive });
			deactivateWalletId = null;
			await invalidateAll();
		}, (message) => (walletError = message), 'Could not change the wallet status.');
	}

	async function toggleMovements(wallet: Wallet) {
		if (openWalletId === wallet.id) {
			openWalletId = null;
			return;
		}
		openWalletId = wallet.id;
		movementsError = null;
		movementError = null;
		movementAmount = '';
		movementNote = '';
		movementKind = 'expense';
		movementsLoading = true;
		try {
			movements[wallet.id] = await api.listWalletMovements(wallet.id, MOVEMENT_LIMIT);
		} catch (error) {
			movementsError = failure(error, 'Could not load the wallet movements.');
		} finally {
			movementsLoading = false;
		}
	}

	async function recordMovement(wallet: Wallet, event: SubmitEvent) {
		event.preventDefault();
		await run(async () => {
			await api.recordWalletMovement(wallet.id, {
				kind: movementKind,
				amount: movementAmount,
				note: movementNote.trim() || undefined
			});
			movementAmount = '';
			movementNote = '';
			// Refresh the panel from the API: the movement list is the evidence
			// behind the balance the table shows after `invalidateAll()`.
			movements[wallet.id] = await api.listWalletMovements(wallet.id, MOVEMENT_LIMIT);
			await invalidateAll();
		}, (message) => (movementError = message), 'Could not record the movement.');
	}

	function startMethodEdit(method: PaymentMethod) {
		editWalletId = null;
		deactivateMethodId = null;
		editMethodId = method.id;
		editMethodName = method.name;
		editMethodWalletId = method.wallet_id;
		methodError = null;
	}

	async function createMethod(event: SubmitEvent) {
		event.preventDefault();
		await run(async () => {
			await api.createPaymentMethod({
				name: createMethodName,
				wallet_id: createMethodWalletId
			});
			createMethodName = '';
			await invalidateAll();
		}, (message) => (methodError = message), 'Could not create the payment method.');
	}

	async function saveMethod(method: PaymentMethod, event: SubmitEvent) {
		event.preventDefault();
		await run(async () => {
			await api.updatePaymentMethod(method.id, {
				name: editMethodName,
				wallet_id: editMethodWalletId
			});
			editMethodId = null;
			await invalidateAll();
		}, (message) => (methodError = message), 'Could not update the payment method.');
	}

	async function setMethodActive(method: PaymentMethod, isActive: boolean) {
		await run(async () => {
			await api.updatePaymentMethod(method.id, { is_active: isActive });
			deactivateMethodId = null;
			await invalidateAll();
		}, (message) => (methodError = message), 'Could not change the payment method status.');
	}
</script>

<div class="mb-4">
	<h1 class="text-xl font-semibold tracking-tight">Payments</h1>
	<p class="mt-1 text-sm text-muted-foreground">
		Wallets hold the money; a payment method says which wallet a charge pays into. Rows are
		deactivated, never deleted, so the ledger stays readable.
	</p>
</div>

<section class="mb-8">
	<div class="mb-3 flex items-center justify-between gap-4">
		<h2 class="text-lg font-semibold">Wallets</h2>
		<span class="text-sm text-muted-foreground">
			{data.wallets.length}
			{data.wallets.length === 1 ? 'wallet' : 'wallets'}
		</span>
	</div>

	<form class="mb-4 flex flex-wrap items-end gap-3" onsubmit={createWallet}>
		<div class="grid gap-1">
			<Label for="new-wallet">New wallet</Label>
			<Input id="new-wallet" bind:value={createWalletName} placeholder="e.g. Cash box" maxlength={120} />
		</div>
		<Button type="submit" size="sm" disabled={busy}>Create wallet</Button>
	</form>

	{#if walletError}
		<p class="mb-3 text-sm text-destructive" role="alert">{walletError}</p>
	{/if}

	{#if data.wallets.length === 0}
		<div class="rounded-lg border border-border p-10 text-center text-sm text-muted-foreground">
			No wallets yet. Create one above, then add a payment method that pays into it.
		</div>
	{:else}
		<div class="overflow-hidden rounded-lg border border-border">
			<Table>
				<TableHeader>
					<TableRow>
						<TableHead>Name</TableHead>
						<TableHead class="text-right">Balance</TableHead>
						<TableHead>Status</TableHead>
						<TableHead class="text-right">Actions</TableHead>
					</TableRow>
				</TableHeader>
				<TableBody>
					{#each data.wallets as wallet (wallet.id)}
						<TableRow>
							<TableCell class="font-medium">{wallet.name}</TableCell>
							<TableCell
								class={cn(
									'text-right tabular-nums',
									isNegative(wallet.balance) && 'text-destructive'
								)}
							>
								{wallet.balance}
							</TableCell>
							<TableCell>
								<Badge variant={wallet.is_active ? 'outline' : 'secondary'}>
									{wallet.is_active ? 'Active' : 'Inactive'}
								</Badge>
							</TableCell>
							<TableCell class="text-right">
								<div class="flex flex-wrap justify-end gap-1">
									{#if editWalletId === wallet.id}
										<Button
											type="button"
											size="sm"
											variant="ghost"
											onclick={() => (editWalletId = null)}>Cancel</Button
										>
									{:else}
										<Button size="sm" variant="outline" onclick={() => startWalletEdit(wallet)}>
											Rename
										</Button>
									{/if}
									<Button
										size="sm"
										variant="outline"
										disabled={busy || movementsLoading}
										onclick={() => toggleMovements(wallet)}
									>
										{openWalletId === wallet.id ? 'Hide movements' : 'Movements'}
									</Button>
									{#if wallet.is_active}
										{#if deactivateWalletId === wallet.id}
											<Button
												size="sm"
												variant="destructive"
												disabled={busy}
												onclick={() => setWalletActive(wallet, false)}
											>
												Confirm deactivation
											</Button>
											<Button
												size="sm"
												variant="ghost"
												onclick={() => (deactivateWalletId = null)}>Cancel</Button
											>
										{:else}
											<Button
												size="sm"
												variant="destructive"
												onclick={() => (deactivateWalletId = wallet.id)}>Deactivate</Button
											>
										{/if}
									{:else}
										<Button
											size="sm"
											variant="outline"
											disabled={busy}
											onclick={() => setWalletActive(wallet, true)}>Activate</Button
										>
									{/if}
								</div>
							</TableCell>
						</TableRow>

						{#if editWalletId === wallet.id}
							<TableRow>
								<TableCell colspan={4} class="whitespace-normal">
									<form
										class="flex flex-wrap items-end gap-3 py-2"
										onsubmit={(event) => saveWallet(wallet, event)}
									>
										<div class="grid gap-1">
											<Label for={`wallet-name-${wallet.id}`}>Wallet name</Label>
											<Input
												id={`wallet-name-${wallet.id}`}
												bind:value={editWalletName}
												maxlength={120}
											/>
										</div>
										<Button type="submit" size="sm" disabled={busy}>Save name</Button>
									</form>
								</TableCell>
							</TableRow>
						{/if}

						{#if openWalletId === wallet.id}
							<TableRow>
								<TableCell colspan={4} class="whitespace-normal">
									<div class="space-y-3 py-3">
										<div class="flex flex-wrap items-center justify-between gap-2">
											<h3 class="text-sm font-medium">Recent movements</h3>
											<span class="text-xs text-muted-foreground">
												Newest first · up to {MOVEMENT_LIMIT}
											</span>
										</div>

										{#if movementsLoading}
											<p class="text-sm text-muted-foreground">Loading movements…</p>
										{:else if movementsError}
											<p class="text-sm text-destructive" role="alert">{movementsError}</p>
										{:else if (movements[wallet.id]?.length ?? 0) === 0}
											<p class="text-sm text-muted-foreground">No movements recorded yet.</p>
										{:else}
											<ul class="space-y-2">
												{#each movements[wallet.id] ?? [] as movement (movement.id)}
													<li
														class="flex flex-wrap items-start justify-between gap-2 border-b pb-2 last:border-b-0"
													>
														<div class="min-w-0">
															<Badge variant="outline" class="text-[10px] uppercase">
																{KIND_LABELS[movement.kind]}
															</Badge>
															<p class="mt-1 text-xs text-muted-foreground">
																{movement.note ?? '—'}
															</p>
															<p class="text-[10px] text-muted-foreground">
																{when(movement.created_at)}
															</p>
														</div>
														<div class="text-right">
															<p
																class="tabular-nums text-sm font-medium"
																class:text-destructive={isNegative(movement.signed_amount)}
															>
																{movement.signed_amount}
															</p>
															<p class="text-[10px] text-muted-foreground tabular-nums">
																{movement.amount}
															</p>
														</div>
													</li>
												{/each}
											</ul>
											{#if (movements[wallet.id]?.length ?? 0) >= MOVEMENT_LIMIT}
												<p class="text-xs text-muted-foreground">
													Newest first. The endpoint returns up to {MOVEMENT_LIMIT}; older
													movements may exist.
												</p>
											{/if}
										{/if}

										<form
											class="flex flex-wrap items-end gap-3 border-t pt-3"
											onsubmit={(event) => recordMovement(wallet, event)}
										>
											<div class="grid gap-1">
												<Label for={`movement-kind-${wallet.id}`}>Kind</Label>
												<Select.Root type="single" bind:value={movementKind}>
													<Select.Trigger id={`movement-kind-${wallet.id}`} class="w-44">
														{MANUAL_KINDS.find((kind) => kind.value === movementKind)?.label}
													</Select.Trigger>
													<Select.Content>
														{#each MANUAL_KINDS as kind (kind.value)}
															<Select.Item value={kind.value} label={kind.label} />
														{/each}
													</Select.Content>
												</Select.Root>
											</div>
											<div class="grid gap-1">
												<Label for={`movement-amount-${wallet.id}`}>Amount</Label>
												<Input
													id={`movement-amount-${wallet.id}`}
													bind:value={movementAmount}
													inputmode="decimal"
													placeholder="0.00"
													class="w-32"
												/>
											</div>
											<div class="grid gap-1">
												<Label for={`movement-note-${wallet.id}`}>Note (optional)</Label>
												<Input
													id={`movement-note-${wallet.id}`}
													bind:value={movementNote}
													maxlength={200}
													placeholder="Optional"
												/>
											</div>
											<Button type="submit" size="sm" disabled={busy}>
												Record movement
											</Button>
											{#if movementError}
												<p class="w-full text-sm text-destructive" role="alert">
													{movementError}
												</p>
											{/if}
										</form>
										<p class="text-xs text-muted-foreground">
											Sale payments enter a wallet by charging a sale, not by hand.
										</p>
									</div>
								</TableCell>
							</TableRow>
						{/if}
					{/each}
				</TableBody>
			</Table>
		</div>
	{/if}
</section>

<section>
	<div class="mb-3 flex items-center justify-between gap-4">
		<h2 class="text-lg font-semibold">Payment methods</h2>
		<span class="text-sm text-muted-foreground">
			{data.methods.length}
			{data.methods.length === 1 ? 'method' : 'methods'}
		</span>
	</div>

	<form class="mb-4 flex flex-wrap items-end gap-3" onsubmit={createMethod}>
		<div class="grid gap-1">
			<Label for="new-method">New method</Label>
			<Input id="new-method" bind:value={createMethodName} placeholder="e.g. Card" maxlength={120} />
		</div>
		<div class="grid gap-1">
			<Label for="new-method-wallet">Pays into</Label>
			<Select.Root type="single" bind:value={createMethodWalletId}>
				<Select.Trigger id="new-method-wallet" class="w-56">
					{data.wallets.find((wallet) => wallet.id === createMethodWalletId)?.name ??
						'Select a wallet'}
				</Select.Trigger>
				<Select.Content>
					{#each data.wallets as wallet (wallet.id)}
						<Select.Item value={wallet.id} label={walletLabel(wallet)} />
					{/each}
				</Select.Content>
			</Select.Root>
		</div>
		<Button type="submit" size="sm" disabled={busy}>Create method</Button>
	</form>

	{#if methodError}
		<p class="mb-3 text-sm text-destructive" role="alert">{methodError}</p>
	{/if}

	{#if data.methods.length === 0}
		<div class="rounded-lg border border-border p-10 text-center text-sm text-muted-foreground">
			No payment methods yet. Create one above; it needs a wallet to pay into.
		</div>
	{:else}
		<div class="overflow-hidden rounded-lg border border-border">
			<Table>
				<TableHeader>
					<TableRow>
						<TableHead>Name</TableHead>
						<TableHead>Pays into</TableHead>
						<TableHead>Status</TableHead>
						<TableHead class="text-right">Actions</TableHead>
					</TableRow>
				</TableHeader>
				<TableBody>
					{#each data.methods as method (method.id)}
						<TableRow>
							<TableCell class="font-medium">{method.name}</TableCell>
							<TableCell class="text-muted-foreground">{method.wallet_name}</TableCell>
							<TableCell>
								<Badge variant={method.is_active ? 'outline' : 'secondary'}>
									{method.is_active ? 'Active' : 'Inactive'}
								</Badge>
							</TableCell>
							<TableCell class="text-right">
								<div class="flex flex-wrap justify-end gap-1">
									{#if editMethodId === method.id}
										<Button
											type="button"
											size="sm"
											variant="ghost"
											onclick={() => (editMethodId = null)}>Cancel</Button
										>
									{:else}
										<Button size="sm" variant="outline" onclick={() => startMethodEdit(method)}>
											Edit
										</Button>
									{/if}
									{#if method.is_active}
										{#if deactivateMethodId === method.id}
											<Button
												size="sm"
												variant="destructive"
												disabled={busy}
												onclick={() => setMethodActive(method, false)}
											>
												Confirm deactivation
											</Button>
											<Button
												size="sm"
												variant="ghost"
												onclick={() => (deactivateMethodId = null)}>Cancel</Button
											>
										{:else}
											<Button
												size="sm"
												variant="destructive"
												onclick={() => (deactivateMethodId = method.id)}>Deactivate</Button
											>
										{/if}
									{:else}
										<Button
											size="sm"
											variant="outline"
											disabled={busy}
											onclick={() => setMethodActive(method, true)}>Activate</Button
										>
									{/if}
								</div>
							</TableCell>
						</TableRow>

						{#if editMethodId === method.id}
							<TableRow>
								<TableCell colspan={4} class="whitespace-normal">
									<form
										class="flex flex-wrap items-end gap-3 py-2"
										onsubmit={(event) => saveMethod(method, event)}
									>
										<div class="grid gap-1">
											<Label for={`method-name-${method.id}`}>Method name</Label>
											<Input
												id={`method-name-${method.id}`}
												bind:value={editMethodName}
												maxlength={120}
											/>
										</div>
										<div class="grid gap-1">
											<Label for={`method-wallet-${method.id}`}>Pays into</Label>
											<Select.Root type="single" bind:value={editMethodWalletId}>
												<Select.Trigger id={`method-wallet-${method.id}`} class="w-56">
													{data.wallets.find((wallet) => wallet.id === editMethodWalletId)
														?.name ?? 'Select a wallet'}
												</Select.Trigger>
												<Select.Content>
													{#each data.wallets as wallet (wallet.id)}
														<Select.Item value={wallet.id} label={walletLabel(wallet)} />
													{/each}
												</Select.Content>
											</Select.Root>
										</div>
										<Button type="submit" size="sm" disabled={busy}>Save changes</Button>
									</form>
								</TableCell>
							</TableRow>
						{/if}
					{/each}
				</TableBody>
			</Table>
		</div>
	{/if}

	<p class="mt-3 text-sm text-muted-foreground">
		Methods are deactivated rather than deleted; deactivating only stops new charges. Past
		movements keep the wallet they reached.
	</p>
</section>
