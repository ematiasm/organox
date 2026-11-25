<script lang="ts">
	import {
		api,
		ApiError,
		type AccountStatementEntry,
		type CreditNote,
		type OutstandingPurchase,
		type OutstandingSale,
		type PaymentMethod,
		type Purchase,
		type PurchaseReturn,
		type ReceiptLog,
		type Sale,
		type SupplierAccountStatementEntry,
		type SupplierPaymentLog
	} from '$lib/api/client';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import {
		Dialog,
		DialogContent,
		DialogDescription,
		DialogFooter,
		DialogHeader,
		DialogTitle
	} from '$lib/components/ui/dialog';
	import {
		Table,
		TableBody,
		TableCell,
		TableHead,
		TableHeader,
		TableRow
	} from '$lib/components/ui/table';

	export type AccountDocumentTarget =
		| {
				party: 'customer';
				customerId: string;
				entry: AccountStatementEntry;
				openSale: OutstandingSale | null;
		  }
		| {
				party: 'supplier';
				supplierId: string;
				entry: SupplierAccountStatementEntry;
				openPurchase: OutstandingPurchase | null;
		  };

	type LoadedDocument =
		| { kind: 'sale'; sale: Sale; openSale: OutstandingSale | null }
		| { kind: 'credit_note'; note: CreditNote | null; saleId: string }
		| { kind: 'receipt'; receipt: ReceiptLog | null; methodName: string | null; walletName: string | null }
		| { kind: 'purchase'; purchase: Purchase; openPurchase: OutstandingPurchase | null }
		| { kind: 'purchase_return'; ret: PurchaseReturn | null; purchaseId: string }
		| {
				kind: 'supplier_payment';
				payment: SupplierPaymentLog | null;
				methodName: string | null;
				walletName: string | null;
		  }
		| { kind: 'entry_only' };

	let {
		target,
		onClose
	}: {
		target: AccountDocumentTarget | null;
		onClose: () => void;
	} = $props();

	let loading = $state(false);
	let loadError = $state<string | null>(null);
	let loaded = $state<LoadedDocument | null>(null);
	let loadSeq = 0;

	function failure(error: unknown, fallback: string): string {
		if (error instanceof ApiError) {
			return error.problem?.detail ?? `The server rejected the request (HTTP ${error.status}).`;
		}
		return fallback;
	}

	function when(iso: string | null | undefined): string {
		if (!iso) return '—';
		const date = new Date(iso);
		return Number.isNaN(date.getTime())
			? iso
			: date.toLocaleString('en-US', { dateStyle: 'short', timeStyle: 'short' });
	}

	function formatSigned(value: string): string {
		return value.startsWith('-') || value.startsWith('+') ? value : `+${value}`;
	}

	function resolveMethodAndWallet(
		methods: PaymentMethod[],
		paymentMethodId: string | null | undefined,
		walletId: string
	): { methodName: string | null; walletName: string | null } {
		const byMethod = paymentMethodId ? methods.find((m) => m.id === paymentMethodId) : undefined;
		if (byMethod) {
			return { methodName: byMethod.name, walletName: byMethod.wallet_name };
		}
		const byWallet = methods.find((m) => m.wallet_id === walletId);
		return { methodName: null, walletName: byWallet?.wallet_name ?? null };
	}

	function dialogHeading(current: AccountDocumentTarget): { title: string; subtitle: string } {
		if (current.party === 'customer') {
			const e = current.entry;
			switch (e.kind) {
				case 'sale_charge':
					return {
						title: `Sale #${(e.sale_id ?? e.id).slice(0, 8)}`,
						subtitle: `On-account charge · ${when(e.created_at)}`
					};
				case 'credit_note':
					return {
						title: `Credit note #${(e.credit_note_id ?? e.id).slice(0, 8)}`,
						subtitle: `Customer return · ${when(e.created_at)}`
					};
				case 'receipt':
					return {
						title: `Receipt #${e.id.slice(0, 8)}`,
						subtitle: `Customer payment · ${when(e.created_at)}`
					};
				case 'credit_refund':
					return {
						title: `Credit refund #${e.id.slice(0, 8)}`,
						subtitle: `Credit handed back to customer · ${when(e.created_at)}`
					};
				case 'adjustment_in':
					return {
						title: `Manual adjustment (+)`,
						subtitle: `Debt increase · ${when(e.created_at)}`
					};
				case 'adjustment_out':
					return {
						title: `Manual adjustment (−)`,
						subtitle: `Debt decrease · ${when(e.created_at)}`
					};
			}
		} else {
			const e = current.entry;
			switch (e.kind) {
				case 'purchase_charge':
					return {
						title: `Purchase #${(e.purchase_id ?? e.id).slice(0, 8)}`,
						subtitle: `On-account payable · ${when(e.created_at)}`
					};
				case 'purchase_return':
					return {
						title: `Purchase return #${(e.purchase_return_id ?? e.id).slice(0, 8)}`,
						subtitle: `Supplier return · ${when(e.created_at)}`
					};
				case 'purchase_return_credit':
					return {
						title: `Return credit #${(e.purchase_return_id ?? e.id).slice(0, 8)}`,
						subtitle: `Unapplied supplier credit · ${when(e.created_at)}`
					};
				case 'payment':
					return {
						title: `Payment #${e.id.slice(0, 8)}`,
						subtitle: `Payment to supplier · ${when(e.created_at)}`
					};
				case 'credit_refund':
					return {
						title: `Credit refund #${e.id.slice(0, 8)}`,
						subtitle: `Advance credit refunded by supplier · ${when(e.created_at)}`
					};
				case 'adjustment_in':
					return {
						title: `Manual adjustment (+)`,
						subtitle: `Payable increase · ${when(e.created_at)}`
					};
				case 'adjustment_out':
					return {
						title: `Manual adjustment (−)`,
						subtitle: `Payable decrease · ${when(e.created_at)}`
					};
			}
		}
	}

	async function loadTarget(current: AccountDocumentTarget) {
		const seq = ++loadSeq;
		loading = true;
		loadError = null;
		loaded = null;

		try {
			if (current.party === 'customer') {
				const e = current.entry;
				if (e.kind === 'sale_charge' && e.sale_id) {
					const sale = await api.getSale(e.sale_id);
					if (seq !== loadSeq) return;
					loaded = { kind: 'sale', sale, openSale: current.openSale };
				} else if (e.kind === 'credit_note' && e.sale_id) {
					const summary = await api.saleCreditNotes(e.sale_id);
					if (seq !== loadSeq) return;
					const note = summary.credit_notes.find((n) => n.id === e.credit_note_id) ?? null;
					loaded = { kind: 'credit_note', note, saleId: e.sale_id };
				} else if (e.kind === 'receipt') {
					const [receipts, methods] = await Promise.all([
						api.listCustomerReceipts(current.customerId, 200),
						api.listPaymentMethods(true)
					]);
					if (seq !== loadSeq) return;
					const receipt = receipts.find((r) => r.receipt_id === e.id) ?? null;
					const resolved = receipt
						? resolveMethodAndWallet(methods, receipt.payment_method_id, receipt.wallet_id)
						: { methodName: null, walletName: null };
					loaded = { kind: 'receipt', receipt, ...resolved };
				} else {
					loaded = { kind: 'entry_only' };
				}
			} else {
				const e = current.entry;
				if (e.kind === 'purchase_charge' && e.purchase_id) {
					const purchase = await api.getPurchase(e.purchase_id);
					if (seq !== loadSeq) return;
					loaded = { kind: 'purchase', purchase, openPurchase: current.openPurchase };
				} else if (e.kind === 'purchase_return' && e.purchase_id) {
					const summary = await api.purchaseReturns(e.purchase_id);
					if (seq !== loadSeq) return;
					const ret = summary.returns.find((r) => r.id === e.purchase_return_id) ?? null;
					loaded = { kind: 'purchase_return', ret, purchaseId: e.purchase_id };
				} else if (e.kind === 'payment') {
					const [payments, methods] = await Promise.all([
						api.listSupplierPayments(current.supplierId, 200),
						api.listPaymentMethods(true)
					]);
					if (seq !== loadSeq) return;
					const payment = payments.find((p) => p.payment_id === e.id) ?? null;
					const resolved = payment
						? resolveMethodAndWallet(methods, payment.payment_method_id, payment.wallet_id)
						: { methodName: null, walletName: null };
					loaded = { kind: 'supplier_payment', payment, ...resolved };
				} else {
					loaded = { kind: 'entry_only' };
				}
			}
		} catch (error) {
			if (seq !== loadSeq) return;
			loadError = failure(error, 'Could not load the document details.');
		} finally {
			if (seq === loadSeq) loading = false;
		}
	}

	$effect(() => {
		const current = target;
		if (!current) {
			++loadSeq;
			loading = false;
			loadError = null;
			loaded = null;
			return;
		}
		void loadTarget(current);
	});
</script>

<Dialog open={target !== null} onOpenChange={(o) => !o && onClose()}>
	<DialogContent class="max-h-[85vh] overflow-y-auto sm:max-w-xl" data-slot="account-document-dialog">
		{#if target}
			{@const heading = dialogHeading(target)}
			<DialogHeader>
				<DialogTitle class="font-mono text-base">{heading.title}</DialogTitle>
				<DialogDescription>{heading.subtitle}</DialogDescription>
			</DialogHeader>

			{#if loading}
				<p class="py-6 text-center text-sm text-muted-foreground">Loading document…</p>
			{:else if loadError}
				<p class="text-sm text-destructive" role="alert">{loadError}</p>
			{:else if loaded?.kind === 'sale'}
				<dl class="grid grid-cols-2 gap-x-4 gap-y-2 rounded-lg border border-border bg-muted/30 p-3 text-xs sm:grid-cols-3">
					<div>
						<dt class="text-[11px] text-muted-foreground">Sale total</dt>
						<dd class="font-semibold tabular-nums">{loaded.sale.total}</dd>
					</div>
					<div>
						<dt class="text-[11px] text-muted-foreground">Charged on account</dt>
						<dd class="font-medium tabular-nums">{formatSigned(target.entry.amount)}</dd>
					</div>
					<div>
						<dt class="text-[11px] text-muted-foreground">Running balance</dt>
						<dd class="font-medium tabular-nums">{target.entry.running_balance}</dd>
					</div>
					{#if loaded.openSale}
						<div>
							<dt class="text-[11px] text-muted-foreground">Outstanding</dt>
							<dd class="font-semibold tabular-nums">{loaded.openSale.outstanding}</dd>
						</div>
						<div>
							<dt class="text-[11px] text-muted-foreground">Paid</dt>
							<dd class="tabular-nums">{loaded.openSale.allocated}</dd>
						</div>
						<div>
							<dt class="text-[11px] text-muted-foreground">Credited</dt>
							<dd class="tabular-nums">{loaded.openSale.cancelled}</dd>
						</div>
					{:else}
						<div>
							<dt class="text-[11px] text-muted-foreground">Account status</dt>
							<dd><Badge variant="outline" class="text-[11px]">Settled</Badge></dd>
						</div>
					{/if}
				</dl>

				{#if loaded.sale.items.length > 0}
					<div class="overflow-x-auto rounded-md border border-border">
						<Table>
							<TableHeader>
								<TableRow>
									<TableHead>Product</TableHead>
									<TableHead class="text-right">Qty</TableHead>
									<TableHead class="text-right">Price</TableHead>
									<TableHead class="text-right">Subtotal</TableHead>
								</TableRow>
							</TableHeader>
							<TableBody>
								{#each loaded.sale.items as line (line.product_id)}
									<TableRow>
										<TableCell class="text-xs">{line.product_name}</TableCell>
										<TableCell class="text-right text-xs tabular-nums">{line.quantity}</TableCell>
										<TableCell class="text-right text-xs tabular-nums">{line.unit_price}</TableCell>
										<TableCell class="text-right text-xs font-medium tabular-nums">
											{line.subtotal}
										</TableCell>
									</TableRow>
								{/each}
							</TableBody>
						</Table>
					</div>
				{/if}

				<DialogFooter>
					<Button
						variant="outline"
						size="sm"
						href={`/operations/sales/${loaded.sale.id}/credit-note`}
					>
						Issue credit note
					</Button>
					<Button variant="secondary" size="sm" onclick={onClose}>Close</Button>
				</DialogFooter>
			{:else if loaded?.kind === 'credit_note'}
				{#if loaded.note}
					<dl class="grid grid-cols-2 gap-x-4 gap-y-2 rounded-lg border border-border bg-muted/30 p-3 text-xs sm:grid-cols-3">
						<div>
							<dt class="text-[11px] text-muted-foreground">Credit note total</dt>
							<dd class="font-semibold tabular-nums">{loaded.note.total}</dd>
						</div>
						<div>
							<dt class="text-[11px] text-muted-foreground">Ledger effect</dt>
							<dd class="font-medium text-primary tabular-nums">{target.entry.amount}</dd>
						</div>
						<div>
							<dt class="text-[11px] text-muted-foreground">Running balance</dt>
							<dd class="font-medium tabular-nums">{target.entry.running_balance}</dd>
						</div>
						<div class="col-span-2 sm:col-span-3">
							<dt class="text-[11px] text-muted-foreground">Reason</dt>
							<dd>{loaded.note.reason}</dd>
						</div>
					</dl>

					{#if loaded.note.items.length > 0}
						<div class="overflow-x-auto rounded-md border border-border">
							<Table>
								<TableHeader>
									<TableRow>
										<TableHead>Returned product</TableHead>
										<TableHead class="text-right">Qty</TableHead>
										<TableHead class="text-right">Price</TableHead>
										<TableHead class="text-right">Subtotal</TableHead>
									</TableRow>
								</TableHeader>
								<TableBody>
									{#each loaded.note.items as line (line.product_id)}
										<TableRow>
											<TableCell class="text-xs">{line.product_name}</TableCell>
											<TableCell class="text-right text-xs tabular-nums">{line.quantity}</TableCell>
											<TableCell class="text-right text-xs tabular-nums">{line.unit_price}</TableCell>
											<TableCell class="text-right text-xs font-medium tabular-nums">
												{line.subtotal}
											</TableCell>
										</TableRow>
									{/each}
								</TableBody>
							</Table>
						</div>
					{/if}

					{#if loaded.note.refunds.length > 0}
						<div class="grid gap-1.5 text-xs">
							<span class="text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">
								Wallet refunds
							</span>
							<div class="grid gap-1 rounded-md border border-border p-2.5">
								{#each loaded.note.refunds as refund, idx (idx)}
									<div class="flex items-center justify-between gap-2">
										<span>
											{refund.payment_method_name}
											<span class="text-muted-foreground">← {refund.wallet_name}</span>
										</span>
										<span class="font-medium tabular-nums">{refund.amount}</span>
									</div>
								{/each}
							</div>
						</div>
					{/if}
				{:else}
					<p class="text-xs text-muted-foreground">
						Credit note detail was not found in the sale summary.
					</p>
				{/if}

				<DialogFooter>
					<Button
						variant="outline"
						size="sm"
						href={`/operations/sales/${loaded.saleId}/credit-note`}
					>
						Open sale returns
					</Button>
					<Button variant="secondary" size="sm" onclick={onClose}>Close</Button>
				</DialogFooter>
			{:else if loaded?.kind === 'receipt'}
				<dl class="grid grid-cols-2 gap-x-4 gap-y-2 rounded-lg border border-border bg-muted/30 p-3 text-xs">
					<div>
						<dt class="text-[11px] text-muted-foreground">Amount collected</dt>
						<dd class="font-semibold text-primary tabular-nums">
							{loaded.receipt?.amount ?? target.entry.amount}
						</dd>
					</div>
					<div>
						<dt class="text-[11px] text-muted-foreground">Running balance</dt>
						<dd class="font-medium tabular-nums">{target.entry.running_balance}</dd>
					</div>
					{#if loaded.methodName}
						<div>
							<dt class="text-[11px] text-muted-foreground">Payment method</dt>
							<dd>{loaded.methodName}</dd>
						</div>
					{/if}
					{#if loaded.walletName}
						<div>
							<dt class="text-[11px] text-muted-foreground">Wallet</dt>
							<dd>{loaded.walletName}</dd>
						</div>
					{/if}
					{#if loaded.receipt}
						<div>
							<dt class="text-[11px] text-muted-foreground">Allocated to sales</dt>
							<dd class="tabular-nums">{loaded.receipt.allocated_total}</dd>
						</div>
						<div>
							<dt class="text-[11px] text-muted-foreground">Wallet movement</dt>
							<dd class="font-mono text-muted-foreground" title={loaded.receipt.wallet_movement_id}>
								#{loaded.receipt.wallet_movement_id.slice(0, 8)}
							</dd>
						</div>
					{/if}
					{#if loaded.receipt?.note ?? target.entry.note}
						<div class="col-span-2">
							<dt class="text-[11px] text-muted-foreground">Note</dt>
							<dd>{loaded.receipt?.note ?? target.entry.note}</dd>
						</div>
					{/if}
				</dl>

				{#if loaded.receipt && loaded.receipt.allocations.length > 0}
					<div class="grid gap-1.5 text-xs">
						<span class="text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">
							Applied to sales
						</span>
						<div class="grid gap-1 rounded-md border border-border p-2.5">
							{#each loaded.receipt.allocations as alloc (alloc.sale_id)}
								<div class="flex items-center justify-between gap-2">
									<a
										href={`/operations/sales/${alloc.sale_id}/credit-note`}
										class="font-mono underline-offset-2 hover:underline"
										title={alloc.sale_id}
									>
										Sale #{alloc.sale_id.slice(0, 8)}
									</a>
									<span class="font-medium tabular-nums">{alloc.amount}</span>
								</div>
							{/each}
						</div>
					</div>
				{/if}

				<DialogFooter>
					<Button variant="secondary" size="sm" onclick={onClose}>Close</Button>
				</DialogFooter>
			{:else if loaded?.kind === 'purchase'}
				<dl class="grid grid-cols-2 gap-x-4 gap-y-2 rounded-lg border border-border bg-muted/30 p-3 text-xs sm:grid-cols-3">
					<div>
						<dt class="text-[11px] text-muted-foreground">Purchase total</dt>
						<dd class="font-semibold tabular-nums">{loaded.purchase.total}</dd>
					</div>
					<div>
						<dt class="text-[11px] text-muted-foreground">Charged on account</dt>
						<dd class="font-medium tabular-nums">{formatSigned(target.entry.amount)}</dd>
					</div>
					<div>
						<dt class="text-[11px] text-muted-foreground">Running balance</dt>
						<dd class="font-medium tabular-nums">{target.entry.running_balance}</dd>
					</div>
					{#if loaded.openPurchase}
						<div>
							<dt class="text-[11px] text-muted-foreground">Outstanding</dt>
							<dd class="font-semibold tabular-nums">{loaded.openPurchase.outstanding}</dd>
						</div>
						<div>
							<dt class="text-[11px] text-muted-foreground">Paid</dt>
							<dd class="tabular-nums">{loaded.openPurchase.allocated}</dd>
						</div>
						<div>
							<dt class="text-[11px] text-muted-foreground">Returned</dt>
							<dd class="tabular-nums">{loaded.openPurchase.cancelled}</dd>
						</div>
					{:else}
						<div>
							<dt class="text-[11px] text-muted-foreground">Account status</dt>
							<dd><Badge variant="outline" class="text-[11px]">Settled</Badge></dd>
						</div>
					{/if}
					{#if loaded.purchase.note}
						<div class="col-span-2 sm:col-span-3">
							<dt class="text-[11px] text-muted-foreground">Note</dt>
							<dd>{loaded.purchase.note}</dd>
						</div>
					{/if}
				</dl>

				{#if loaded.purchase.items.length > 0}
					<div class="overflow-x-auto rounded-md border border-border">
						<Table>
							<TableHeader>
								<TableRow>
									<TableHead>Product</TableHead>
									<TableHead class="text-right">Qty</TableHead>
									<TableHead class="text-right">Unit cost</TableHead>
									<TableHead class="text-right">Subtotal</TableHead>
								</TableRow>
							</TableHeader>
							<TableBody>
								{#each loaded.purchase.items as line (line.product_id)}
									<TableRow>
										<TableCell class="text-xs">{line.product_name}</TableCell>
										<TableCell class="text-right text-xs tabular-nums">{line.quantity}</TableCell>
										<TableCell class="text-right text-xs tabular-nums">{line.unit_cost}</TableCell>
										<TableCell class="text-right text-xs font-medium tabular-nums">
											{line.subtotal}
										</TableCell>
									</TableRow>
								{/each}
							</TableBody>
						</Table>
					</div>
				{/if}

				<DialogFooter>
					<Button
						variant="outline"
						size="sm"
						href={`/operations/purchases/${loaded.purchase.id}/return`}
					>
						Return goods
					</Button>
					<Button variant="secondary" size="sm" onclick={onClose}>Close</Button>
				</DialogFooter>
			{:else if loaded?.kind === 'purchase_return'}
				{#if loaded.ret}
					<dl class="grid grid-cols-2 gap-x-4 gap-y-2 rounded-lg border border-border bg-muted/30 p-3 text-xs sm:grid-cols-3">
						<div>
							<dt class="text-[11px] text-muted-foreground">Return total</dt>
							<dd class="font-semibold tabular-nums">{loaded.ret.total}</dd>
						</div>
						<div>
							<dt class="text-[11px] text-muted-foreground">Payable cancelled</dt>
							<dd class="font-medium text-primary tabular-nums">{loaded.ret.debt_cancelled}</dd>
						</div>
						<div>
							<dt class="text-[11px] text-muted-foreground">Credit granted</dt>
							<dd class="tabular-nums">{loaded.ret.credit_granted}</dd>
						</div>
						<div class="col-span-2 sm:col-span-3">
							<dt class="text-[11px] text-muted-foreground">Reason</dt>
							<dd>{loaded.ret.reason}</dd>
						</div>
					</dl>

					{#if loaded.ret.items.length > 0}
						<div class="overflow-x-auto rounded-md border border-border">
							<Table>
								<TableHeader>
									<TableRow>
										<TableHead>Returned product</TableHead>
										<TableHead class="text-right">Qty</TableHead>
										<TableHead class="text-right">Unit cost</TableHead>
										<TableHead class="text-right">Subtotal</TableHead>
									</TableRow>
								</TableHeader>
								<TableBody>
									{#each loaded.ret.items as line (line.product_id)}
										<TableRow>
											<TableCell class="text-xs">{line.product_name}</TableCell>
											<TableCell class="text-right text-xs tabular-nums">{line.quantity}</TableCell>
											<TableCell class="text-right text-xs tabular-nums">{line.unit_cost}</TableCell>
											<TableCell class="text-right text-xs font-medium tabular-nums">
												{line.subtotal}
											</TableCell>
										</TableRow>
									{/each}
								</TableBody>
							</Table>
						</div>
					{/if}

					{#if loaded.ret.refunds.length > 0}
						<div class="grid gap-1.5 text-xs">
							<span class="text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">
								Wallet refunds
							</span>
							<div class="grid gap-1 rounded-md border border-border p-2.5">
								{#each loaded.ret.refunds as refund, idx (idx)}
									<div class="flex items-center justify-between gap-2">
										<span>
											{refund.payment_method_name}
											<span class="text-muted-foreground">→ {refund.wallet_name}</span>
										</span>
										<span class="font-medium tabular-nums">{refund.amount}</span>
									</div>
								{/each}
							</div>
						</div>
					{/if}
				{:else}
					<p class="text-xs text-muted-foreground">
						Purchase return detail was not found in the purchase summary.
					</p>
				{/if}

				<DialogFooter>
					<Button
						variant="outline"
						size="sm"
						href={`/operations/purchases/${loaded.purchaseId}/return`}
					>
						Open purchase returns
					</Button>
					<Button variant="secondary" size="sm" onclick={onClose}>Close</Button>
				</DialogFooter>
			{:else if loaded?.kind === 'supplier_payment'}
				<dl class="grid grid-cols-2 gap-x-4 gap-y-2 rounded-lg border border-border bg-muted/30 p-3 text-xs">
					<div>
						<dt class="text-[11px] text-muted-foreground">Amount paid</dt>
						<dd class="font-semibold text-primary tabular-nums">
							{loaded.payment?.amount ?? target.entry.amount}
						</dd>
					</div>
					<div>
						<dt class="text-[11px] text-muted-foreground">Running balance</dt>
						<dd class="font-medium tabular-nums">{target.entry.running_balance}</dd>
					</div>
					{#if loaded.methodName}
						<div>
							<dt class="text-[11px] text-muted-foreground">Payment method</dt>
							<dd>{loaded.methodName}</dd>
						</div>
					{/if}
					{#if loaded.walletName}
						<div>
							<dt class="text-[11px] text-muted-foreground">Wallet</dt>
							<dd>{loaded.walletName}</dd>
						</div>
					{/if}
					{#if loaded.payment}
						<div>
							<dt class="text-[11px] text-muted-foreground">Allocated to purchases</dt>
							<dd class="tabular-nums">{loaded.payment.allocated_total}</dd>
						</div>
						<div>
							<dt class="text-[11px] text-muted-foreground">Wallet movement</dt>
							<dd class="font-mono text-muted-foreground" title={loaded.payment.wallet_movement_id}>
								#{loaded.payment.wallet_movement_id.slice(0, 8)}
							</dd>
						</div>
					{/if}
					{#if loaded.payment?.note ?? target.entry.note}
						<div class="col-span-2">
							<dt class="text-[11px] text-muted-foreground">Note</dt>
							<dd>{loaded.payment?.note ?? target.entry.note}</dd>
						</div>
					{/if}
				</dl>

				{#if loaded.payment && loaded.payment.allocations.length > 0}
					<div class="grid gap-1.5 text-xs">
						<span class="text-[11px] font-semibold uppercase tracking-wide text-muted-foreground">
							Applied to purchases
						</span>
						<div class="grid gap-1 rounded-md border border-border p-2.5">
							{#each loaded.payment.allocations as alloc (alloc.purchase_id)}
								<div class="flex items-center justify-between gap-2">
									<a
										href={`/operations/purchases/${alloc.purchase_id}/return`}
										class="font-mono underline-offset-2 hover:underline"
										title={alloc.purchase_id}
									>
										Purchase #{alloc.purchase_id.slice(0, 8)}
									</a>
									<span class="font-medium tabular-nums">{alloc.amount}</span>
								</div>
							{/each}
						</div>
					</div>
				{/if}

				<DialogFooter>
					<Button variant="secondary" size="sm" onclick={onClose}>Close</Button>
				</DialogFooter>
			{:else if loaded?.kind === 'entry_only'}
				<dl class="grid grid-cols-2 gap-x-4 gap-y-2 rounded-lg border border-border bg-muted/30 p-3 text-xs">
					<div>
						<dt class="text-[11px] text-muted-foreground">Amount</dt>
						<dd class="font-semibold tabular-nums">{formatSigned(target.entry.amount)}</dd>
					</div>
					<div>
						<dt class="text-[11px] text-muted-foreground">Running balance</dt>
						<dd class="font-medium tabular-nums">{target.entry.running_balance}</dd>
					</div>
					<div>
						<dt class="text-[11px] text-muted-foreground">Entry ID</dt>
						<dd class="font-mono text-muted-foreground" title={target.entry.id}>
							#{target.entry.id.slice(0, 8)}
						</dd>
					</div>
					{#if target.entry.wallet_movement_id}
						<div>
							<dt class="text-[11px] text-muted-foreground">Wallet movement</dt>
							<dd class="font-mono text-muted-foreground" title={target.entry.wallet_movement_id}>
								#{target.entry.wallet_movement_id.slice(0, 8)}
							</dd>
						</div>
					{/if}
					{#if target.entry.note}
						<div class="col-span-2">
							<dt class="text-[11px] text-muted-foreground">Note</dt>
							<dd>{target.entry.note}</dd>
						</div>
					{/if}
				</dl>

				<DialogFooter>
					<Button variant="secondary" size="sm" onclick={onClose}>Close</Button>
				</DialogFooter>
			{/if}
		{/if}
	</DialogContent>
</Dialog>
