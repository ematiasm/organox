/**
 * API HTTP client. Centralizes rules shared by all screens:
 *  1. Relative paths work through Vite's proxy in development and Axum in
 *     production without a hardcoded host.
 *  2. `401` redirects expired sessions to login consistently.
 *  3. `429` exposes `Retry-After` to make errors actionable.
 *  4. `application/problem+json` errors (RFC 7807) become `ApiError` objects.
 */

import type { components } from './schema';

// ---------- Types derived from the OpenAPI spec ----------
//
// These aliases propagate backend changes from generated `schema.d.ts` to the UI.

export type Product = components['schemas']['ProductResponse'];
export type ProductList = components['schemas']['ProductListResponse'];
export type ProductKind = components['schemas']['ProductKind'];
export type Customer = components['schemas']['CustomerResponse'];
export type CustomerList = components['schemas']['CustomerListResponse'];
export type CustomerId = components['schemas']['CustomerIdResponse'];
export type Supplier = components['schemas']['SupplierResponse'];
export type SupplierList = components['schemas']['SupplierListResponse'];
export type SupplierId = components['schemas']['SupplierIdResponse'];
export type Purchase = components['schemas']['PurchaseResponse'];
export type PurchaseItem = components['schemas']['PurchaseItemResponse'];
export type PurchaseList = components['schemas']['PurchaseListResponse'];
export type PurchaseStatus = components['schemas']['PurchaseStatus'];
export type PurchaseDraft = components['schemas']['PurchaseDraftResponse'];
export type CompletedPurchase = components['schemas']['CompletedPurchaseResponse'];
export type PurchasePayment = components['schemas']['PurchasePaymentRequest'];
export type PurchaseReturnsSummary = components['schemas']['PurchaseReturnsSummaryResponse'];
export type PurchaseReturn = components['schemas']['PurchaseReturnResponse'];
export type ReturnablePurchaseLine = components['schemas']['ReturnablePurchaseLineResponse'];
export type PurchaseReturnItem = components['schemas']['PurchaseReturnItemResponse'];
export type PurchaseReturnRefund = components['schemas']['PurchaseReturnRefundResponse'];
export type CreatePurchaseReturnRequest = components['schemas']['CreatePurchaseReturnRequest'];
export type SurplusMode = components['schemas']['SurplusMode'];
export type CustomerAccount = components['schemas']['CustomerAccountResponse'];
export type AccountStatementEntry = components['schemas']['AccountStatementEntryResponse'];
export type AccountEntryKind = components['schemas']['CustomerAccountEntryKind'];
export type OutstandingSale = components['schemas']['OutstandingSaleResponse'];
export type CollectReceiptRequest = components['schemas']['CollectReceiptRequest'];
export type ReceiptAllocationRequest = components['schemas']['ReceiptAllocationRequest'];
export type Receipt = components['schemas']['ReceiptResponse'];
export type ReceiptLog = components['schemas']['ReceiptLogResponse'];
export type RefundCreditRequest = components['schemas']['RefundCreditRequest'];
export type CreditRefund = components['schemas']['CreditRefundResponse'];
export type RecordAdjustmentRequest = components['schemas']['RecordAdjustmentRequest'];
export type AdjustmentDirection = components['schemas']['AdjustmentDirection'];
export type Adjustment = components['schemas']['AdjustmentResponse'];
export type AdjustmentLog = components['schemas']['AdjustmentLogResponse'];
export type Receivables = components['schemas']['ReceivablesResponse'];
export type ReceivableCustomer = components['schemas']['ReceivableCustomerResponse'];
export type AgingBuckets = components['schemas']['AgingBucketsResponse'];
export type SupplierAccount = components['schemas']['SupplierAccountResponse'];
export type SupplierAccountStatementEntry =
	components['schemas']['SupplierAccountStatementEntryResponse'];
export type SupplierAccountEntryKind = components['schemas']['SupplierAccountEntryKind'];
export type OutstandingPurchase = components['schemas']['OutstandingPurchaseResponse'];
export type RecordSupplierPaymentRequest = components['schemas']['RecordSupplierPaymentRequest'];
export type SupplierPaymentAllocationRequest =
	components['schemas']['SupplierPaymentAllocationRequest'];
export type SupplierPayment = components['schemas']['SupplierPaymentResponse'];
export type SupplierPaymentLog = components['schemas']['SupplierPaymentLogResponse'];
export type RefundSupplierCreditRequest = components['schemas']['RefundSupplierCreditRequest'];
export type SupplierCreditRefund = components['schemas']['SupplierCreditRefundResponse'];
export type RecordSupplierAdjustmentRequest =
	components['schemas']['RecordSupplierAdjustmentRequest'];
export type SupplierAdjustmentDirection = components['schemas']['SupplierAdjustmentDirection'];
export type SupplierAdjustment = components['schemas']['SupplierAdjustmentResponse'];
export type SupplierAdjustmentLog = components['schemas']['SupplierAdjustmentLogResponse'];
export type Payables = components['schemas']['PayablesResponse'];
export type PayableSupplier = components['schemas']['PayableSupplierResponse'];
export type PayablesAgingBuckets = components['schemas']['PayablesAgingBucketsResponse'];
export type Movement = components['schemas']['MovementResponse'];
export type MovementReason = components['schemas']['MovementReason'];
export type Role = components['schemas']['Role'];
export type SessionUser = components['schemas']['UserResponse'];
export type User = components['schemas']['UserResponse'];
export type Sale = components['schemas']['SaleResponse'];
export type SaleItem = components['schemas']['SaleItemResponse'];
export type SaleStatus = components['schemas']['SaleStatus'];
export type SaleCreditNotes = components['schemas']['SaleCreditNotesResponse'];
export type CreditNote = components['schemas']['CreditNoteResponse'];
export type CreditableLine = components['schemas']['CreditableLineResponse'];
export type CreditNoteItem = components['schemas']['CreditNoteItemResponse'];
export type CreditNoteRefund = components['schemas']['CreditNoteRefundResponse'];
export type CreateCreditNoteRequest = components['schemas']['CreateCreditNoteRequest'];
export type CompletedSale = components['schemas']['CompletedSaleResponse'];
export type Draft = components['schemas']['DraftResponse'];
export type Problem = components['schemas']['Problem'];
export type IdResponse = components['schemas']['IdResponse'];
export type Wallet = components['schemas']['WalletResponse'];
export type PaymentMethod = components['schemas']['PaymentMethodResponse'];
export type WalletMovement = components['schemas']['WalletMovementResponse'];
export type WalletMovementKind = components['schemas']['WalletMovementKind'];
export type SalePayment = components['schemas']['SalePaymentRequest'];
export type ChangePasswordRequest = components['schemas']['ChangePasswordRequest'];
export type ResetPasswordRequest = components['schemas']['ResetPasswordRequest'];

/** Normalized API error. */
export class ApiError extends Error {
	readonly status: number;
	/** Seconds until retry, only present on a `429`. */
	readonly retryAfter: number | null;
	readonly problem: Problem | null;

	constructor(status: number, retryAfter: number | null, problem: Problem | null) {
		super(problem?.detail ?? `Error ${status}`);
		this.name = 'ApiError';
		this.status = status;
		this.retryAfter = retryAfter;
		this.problem = problem;
	}

	get isUnauthorized() {
		return this.status === 401;
	}

	get isRateLimited() {
		return this.status === 429;
	}
}

type RequestOptions = {
	method?: 'GET' | 'POST' | 'PUT' | 'PATCH' | 'DELETE';
	body?: unknown;
	/** Query parameters; `undefined` values are omitted. */
	query?: Record<string, string | number | boolean | undefined | null>;
	/** Skip automatic redirect to /login (used by the layout guard). */
	skipAuthRedirect?: boolean;
};

function buildUrl(path: string, query?: RequestOptions['query']): string {
	const url = new URL(path, window.location.origin);
	if (query) {
		for (const [key, value] of Object.entries(query)) {
			if (value === undefined || value === null || value === '') continue;
			url.searchParams.set(key, String(value));
		}
	}
	return url.pathname + url.search;
}

export async function request<T>(path: string, options: RequestOptions = {}): Promise<T> {
	const { method = 'GET', body, query, skipAuthRedirect = false } = options;

	const response = await fetch(buildUrl(path, query), {
		method,
		// Same origin sends the HttpOnly cookie. Keep `same-origin` explicit so
		// it is not casually changed to `include`.
		credentials: 'same-origin',
		headers: body === undefined ? undefined : { 'content-type': 'application/json' },
		body: body === undefined ? undefined : JSON.stringify(body)
	});

	if (response.ok) {
		// 204 No Content: there is no body to parse.
		if (response.status === 204) return undefined as T;
		return (await response.json()) as T;
	}

	const retryAfterHeader = response.headers.get('retry-after');
	const retryAfter = retryAfterHeader ? Number(retryAfterHeader) : null;

	const problem = (await response.json().catch(() => null)) as Problem | null;

	if (response.status === 401 && !skipAuthRedirect) {
		// Expired or missing session: use one destination. `replace` keeps the
		// protected page out of history.
		window.location.replace('/login');
	}

	throw new ApiError(response.status, Number.isFinite(retryAfter) ? retryAfter : null, problem);
}

// ---------- Endpoints ----------

export const api = {
	me: () => request<SessionUser>('/api/v1/auth/me', { skipAuthRedirect: true }),

	listUsers: () => request<User[]>('/api/v1/users'),

	login: (email: string, password: string) =>
		request<components['schemas']['LoginResponse']>('/api/v1/auth/login', {
			method: 'POST',
			body: { email, password },
			skipAuthRedirect: true
		}),

	logout: () => request<void>('/api/v1/auth/logout', { method: 'POST', skipAuthRedirect: true }),

	/**
	 * A 401 means the current password was wrong, not that the session expired:
	 * `skipAuthRedirect` keeps a typo from sending the operator to /login.
	 */
	changePassword: (body: ChangePasswordRequest) =>
		request<void>('/api/v1/auth/password', {
			method: 'POST',
			body,
			skipAuthRedirect: true
		}),

	/** The owner resets any user; the target's sessions are revoked by the API. */
	resetUserPassword: (id: string, body: ResetPasswordRequest) =>
		request<void>(`/api/v1/users/${encodeURIComponent(id)}/password`, {
			method: 'PUT',
			body
		}),

	listProducts: (params: {
		search?: string;
		kind?: ProductKind;
		includeInactive?: boolean;
		limit?: number;
		offset?: number;
	}) =>
		request<ProductList>('/api/v1/products', {
			query: {
				search: params.search,
				kind: params.kind,
				include_inactive: params.includeInactive,
				limit: params.limit,
				offset: params.offset
			}
		}),

	getProduct: (id: string) => request<Product>(`/api/v1/products/${id}`),

	listMovements: (id: string) => request<Movement[]>(`/api/v1/products/${id}/movements`),

	// --- Product mutations ---
	//
	// The server is the only validator: the wrappers send raw strings (decimals
	// stay strings per the API contract) and surface `problem.detail` on failure.
	// An empty input must arrive as `undefined`, never `''`: for `price`,
	// `min_stock` and `sku` an omitted field means "unchanged" on PATCH, and an
	// empty string would either be a no-op (sku) or a deserialization error.

	createProduct: (body: {
		kind: ProductKind;
		name: string;
		sku?: string;
		description?: string;
		price?: string;
		min_stock?: string;
	}) => request<IdResponse>('/api/v1/products', { method: 'POST', body }),

	updateProduct: (
		id: string,
		body: {
			name?: string;
			sku?: string;
			description?: string;
			price?: string;
			min_stock?: string;
		}
	) => request<void>(`/api/v1/products/${id}`, { method: 'PATCH', body }),

	deactivateProduct: (id: string) =>
		request<void>(`/api/v1/products/${id}/deactivate`, { method: 'POST' }),

	// --- Customers ---
	//
	// The registry a sale may point at. `tax_id`, `email` and `phone` are
	// optional free text: an empty input is `undefined`, never `''` — on POST an
	// omitted field means "absent", and on PATCH the server COALESCEs, so a null
	// means "unchanged" (an optional field can be replaced, not cleared). The
	// server is the only validator; its `problem.detail` is the error message.

	listCustomers: (params: {
		search?: string;
		includeInactive?: boolean;
		limit?: number;
		offset?: number;
	}) =>
		request<CustomerList>('/api/v1/customers', {
			query: {
				search: params.search,
				include_inactive: params.includeInactive,
				limit: params.limit,
				offset: params.offset
			}
		}),

	getCustomer: (id: string) => request<Customer>(`/api/v1/customers/${id}`),

	createCustomer: (body: {
		name: string;
		tax_id?: string;
		email?: string;
		phone?: string;
		credit_limit?: string | null;
	}) => request<CustomerId>('/api/v1/customers', { method: 'POST', body }),

	updateCustomer: (
		id: string,
		body: {
			name?: string;
			tax_id?: string;
			email?: string;
			phone?: string;
			credit_limit?: string | null;
			is_active?: boolean;
		}
	) => request<void>(`/api/v1/customers/${id}`, { method: 'PATCH', body }),

	deactivateCustomer: (id: string) =>
		request<void>(`/api/v1/customers/${id}/deactivate`, { method: 'POST' }),

	// --- Suppliers ---
	//
	// Registry mirrored from customers: `tax_id`, `email` and `phone` are
	// optional, blank inputs travel as `undefined`, `credit_limit` is tri-state
	// on PATCH (`undefined` = unchanged, `null` = unlimited), and the server is
	// the only validator.

	listSuppliers: (params: {
		search?: string;
		includeInactive?: boolean;
		limit?: number;
		offset?: number;
	}) =>
		request<SupplierList>('/api/v1/suppliers', {
			query: {
				search: params.search,
				include_inactive: params.includeInactive,
				limit: params.limit,
				offset: params.offset
			}
		}),

	getSupplier: (id: string) => request<Supplier>(`/api/v1/suppliers/${encodeURIComponent(id)}`),

	createSupplier: (body: {
		name: string;
		tax_id?: string;
		email?: string;
		phone?: string;
		credit_limit?: string | null;
	}) => request<SupplierId>('/api/v1/suppliers', { method: 'POST', body }),

	updateSupplier: (
		id: string,
		body: {
			name?: string;
			tax_id?: string;
			email?: string;
			phone?: string;
			credit_limit?: string | null;
			is_active?: boolean;
		}
	) => request<void>(`/api/v1/suppliers/${encodeURIComponent(id)}`, { method: 'PATCH', body }),

	deactivateSupplier: (id: string) =>
		request<void>(`/api/v1/suppliers/${encodeURIComponent(id)}/deactivate`, { method: 'POST' }),

	// --- Purchases ---
	//
	// Persisted draft-to-confirm purchases mirroring sales. Item mutations return
	// the recalculated full purchase; completion records stock inflows, optional
	// wallet outflows, and optional on-account payables atomically on the server.

	openPurchaseDraft: () => request<PurchaseDraft>('/api/v1/purchases/draft', { method: 'POST' }),

	listPurchases: (params: {
		status?: PurchaseStatus;
		supplierId?: string;
		limit?: number;
		offset?: number;
	}) =>
		request<PurchaseList>('/api/v1/purchases', {
			query: {
				status: params.status,
				supplier_id: params.supplierId,
				limit: params.limit,
				offset: params.offset
			}
		}),

	getPurchase: (id: string) => request<Purchase>(`/api/v1/purchases/${encodeURIComponent(id)}`),

	setPurchaseSupplier: (purchaseId: string, supplierId: string | null) =>
		request<void>(`/api/v1/purchases/${encodeURIComponent(purchaseId)}/supplier`, {
			method: 'PUT',
			body: { supplier_id: supplierId }
		}),

	addPurchaseItem: (purchaseId: string, productId: string, quantity: string, unitCost: string) =>
		request<Purchase>(`/api/v1/purchases/${encodeURIComponent(purchaseId)}/items`, {
			method: 'POST',
			body: { product_id: productId, quantity, unit_cost: unitCost }
		}),

	setPurchaseItem: (purchaseId: string, productId: string, quantity: string, unitCost: string) =>
		request<Purchase>(
			`/api/v1/purchases/${encodeURIComponent(purchaseId)}/items/${encodeURIComponent(productId)}`,
			{
				method: 'PUT',
				body: { quantity, unit_cost: unitCost }
			}
		),

	removePurchaseItem: (purchaseId: string, productId: string) =>
		request<Purchase>(
			`/api/v1/purchases/${encodeURIComponent(purchaseId)}/items/${encodeURIComponent(productId)}`,
			{ method: 'DELETE' }
		),

	completePurchase: (
		purchaseId: string,
		payments: PurchasePayment[],
		note?: string,
		onAccount?: string
	) => {
		const body: { payments: PurchasePayment[]; note?: string; on_account?: string } = { payments };
		if (note) body.note = note;
		if (onAccount !== undefined) body.on_account = onAccount;
		return request<CompletedPurchase>(
			`/api/v1/purchases/${encodeURIComponent(purchaseId)}/complete`,
			{
				method: 'POST',
				body
			}
		);
	},

	cancelPurchase: (purchaseId: string) =>
		request<void>(`/api/v1/purchases/${encodeURIComponent(purchaseId)}/cancel`, {
			method: 'POST'
		}),

	// --- Customer accounts ---
	//
	// Debt ledger, receipts, credit refunds, manual adjustments and receivables.
	// Amounts travel as decimal strings; the server owns every invariant.

	getCustomerAccount: (customerId: string) =>
		request<CustomerAccount>(`/api/v1/customers/${encodeURIComponent(customerId)}/account`),

	listCustomerReceipts: (customerId: string, limit?: number) =>
		request<ReceiptLog[]>(`/api/v1/customers/${encodeURIComponent(customerId)}/receipts`, {
			query: { limit }
		}),

	collectCustomerReceipt: (customerId: string, body: CollectReceiptRequest) =>
		request<Receipt>(`/api/v1/customers/${encodeURIComponent(customerId)}/receipts`, {
			method: 'POST',
			body
		}),

	refundCustomerCredit: (customerId: string, body: RefundCreditRequest) =>
		request<CreditRefund>(`/api/v1/customers/${encodeURIComponent(customerId)}/credit-refunds`, {
			method: 'POST',
			body
		}),

	listCustomerAdjustments: (customerId: string, limit?: number) =>
		request<AdjustmentLog[]>(`/api/v1/customers/${encodeURIComponent(customerId)}/adjustments`, {
			query: { limit }
		}),

	recordCustomerAdjustment: (customerId: string, body: RecordAdjustmentRequest) =>
		request<Adjustment>(`/api/v1/customers/${encodeURIComponent(customerId)}/adjustments`, {
			method: 'POST',
			body
		}),

	getReceivables: () => request<Receivables>('/api/v1/accounts/receivables'),

	// --- Supplier accounts ---
	//
	// Payables ledger, supplier payments, credit refunds, manual adjustments and
	// aging report. Amounts travel as decimal strings; the server owns every
	// invariant.

	getSupplierAccount: (supplierId: string) =>
		request<SupplierAccount>(`/api/v1/suppliers/${encodeURIComponent(supplierId)}/account`),

	listSupplierPayments: (supplierId: string, limit?: number) =>
		request<SupplierPaymentLog[]>(`/api/v1/suppliers/${encodeURIComponent(supplierId)}/payments`, {
			query: { limit }
		}),

	recordSupplierPayment: (supplierId: string, body: RecordSupplierPaymentRequest) =>
		request<SupplierPayment>(`/api/v1/suppliers/${encodeURIComponent(supplierId)}/payments`, {
			method: 'POST',
			body
		}),

	refundSupplierCredit: (supplierId: string, body: RefundSupplierCreditRequest) =>
		request<SupplierCreditRefund>(
			`/api/v1/suppliers/${encodeURIComponent(supplierId)}/credit-refunds`,
			{
				method: 'POST',
				body
			}
		),

	listSupplierAdjustments: (supplierId: string, limit?: number) =>
		request<SupplierAdjustmentLog[]>(
			`/api/v1/suppliers/${encodeURIComponent(supplierId)}/adjustments`,
			{
				query: { limit }
			}
		),

	recordSupplierAdjustment: (supplierId: string, body: RecordSupplierAdjustmentRequest) =>
		request<SupplierAdjustment>(
			`/api/v1/suppliers/${encodeURIComponent(supplierId)}/adjustments`,
			{
				method: 'POST',
				body
			}
		),

	getPayables: () => request<Payables>('/api/v1/accounts/payables'),

	// --- Wallets and payment methods ---
	//
	// Money stays a string all the way to the API and back. Inactive rows are
	// requested only by settings, which must be able to bring them back; the
	// POS never asks for them. The server validates amounts and totals.

	listWallets: (includeInactive = false) =>
		request<Wallet[]>('/api/v1/wallets', {
			query: { include_inactive: includeInactive || undefined }
		}),

	createWallet: (name: string) =>
		request<IdResponse>('/api/v1/wallets', { method: 'POST', body: { name } }),

	updateWallet: (
		id: string,
		body: { name?: string; is_active?: boolean }
	) => request<void>(`/api/v1/wallets/${id}`, { method: 'PATCH', body }),

	listWalletMovements: (id: string, limit = 50) =>
		request<WalletMovement[]>(`/api/v1/wallets/${id}/movements`, { query: { limit } }),

	recordWalletMovement: (
		id: string,
		body: { kind: WalletMovementKind; amount: string; note?: string }
	) =>
		request<IdResponse>(`/api/v1/wallets/${id}/movements`, {
			method: 'POST',
			body
		}),

	listPaymentMethods: (includeInactive = false) =>
		request<PaymentMethod[]>('/api/v1/payment-methods', {
			query: { include_inactive: includeInactive || undefined }
		}),

	createPaymentMethod: (body: { name: string; wallet_id: string }) =>
		request<IdResponse>('/api/v1/payment-methods', { method: 'POST', body }),

	updatePaymentMethod: (
		id: string,
		body: { name?: string; wallet_id?: string; is_active?: boolean }
	) => request<void>(`/api/v1/payment-methods/${id}`, { method: 'PATCH', body }),

	// --- Sales ---
	//
	// Every mutation returns the complete recalculated sale. The client only
	// displays the server response; it does not add or validate totals.

	openDraft: () => request<Draft>('/api/v1/sales/draft', { method: 'POST' }),

	getSale: (id: string) => request<Sale>(`/api/v1/sales/${id}`),

	addItem: (saleId: string, productId: string, quantity: string) =>
		request<Sale>(`/api/v1/sales/${saleId}/items`, {
			method: 'POST',
			body: { product_id: productId, quantity }
		}),

	setItemQuantity: (saleId: string, productId: string, quantity: string) =>
		request<Sale>(`/api/v1/sales/${saleId}/items/${productId}`, {
			method: 'PUT',
			body: { quantity }
		}),

	removeItem: (saleId: string, productId: string) =>
		request<Sale>(`/api/v1/sales/${saleId}/items/${productId}`, {
			method: 'DELETE'
		}),

	completeSale: (saleId: string, payments: SalePayment[], onAccount?: string) =>
		request<CompletedSale>(`/api/v1/sales/${saleId}/complete`, {
			method: 'POST',
			body: onAccount !== undefined ? { payments, on_account: onAccount } : { payments }
		}),

	// The one sale mutation that answers 204 instead of the recalculated sale:
	// the screen re-reads the sale afterwards. `customer_id` is always present
	// in the body — an omitted field is a 422 by design, and `null` is the only
	// way to clear the customer.
	setSaleCustomer: (saleId: string, customerId: string | null) =>
		request<void>(`/api/v1/sales/${encodeURIComponent(saleId)}/customer`, {
			method: 'PUT',
			body: { customer_id: customerId }
		}),

	cancelSale: (saleId: string) =>
		request<void>(`/api/v1/sales/${saleId}/cancel`, { method: 'POST' }),

	// --- Credit notes ---
	//
	// A return sends a reason, quantities and an optional idempotency key: a
	// repeated create with the same key and sale returns the first note. The
	// note's prices, total and refunds come back from the server; the client
	// never calculates them.

	listSales: () => request<Sale[]>('/api/v1/sales'),

	saleCreditNotes: (saleId: string) =>
		request<SaleCreditNotes>(`/api/v1/sales/${encodeURIComponent(saleId)}/credit-notes`),

	createCreditNote: (saleId: string, body: CreateCreditNoteRequest) =>
		request<CreditNote>(`/api/v1/sales/${encodeURIComponent(saleId)}/credit-notes`, {
			method: 'POST',
			body
		}),

	// --- Purchase returns ---

	purchaseReturns: (purchaseId: string) =>
		request<PurchaseReturnsSummary>(`/api/v1/purchases/${encodeURIComponent(purchaseId)}/returns`),

	createPurchaseReturn: (purchaseId: string, body: CreatePurchaseReturnRequest) =>
		request<PurchaseReturn>(`/api/v1/purchases/${encodeURIComponent(purchaseId)}/returns`, {
			method: 'POST',
			body
		})
};
