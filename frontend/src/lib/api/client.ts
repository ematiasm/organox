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
export type Movement = components['schemas']['MovementResponse'];
export type MovementReason = components['schemas']['MovementReason'];
export type Role = components['schemas']['Role'];
export type SessionUser = components['schemas']['UserResponse'];
export type Sale = components['schemas']['SaleResponse'];
export type SaleItem = components['schemas']['SaleItemResponse'];
export type SaleStatus = components['schemas']['SaleStatus'];
export type CompletedSale = components['schemas']['CompletedSaleResponse'];
export type Draft = components['schemas']['DraftResponse'];
export type Problem = components['schemas']['Problem'];

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

	login: (email: string, password: string) =>
		request<components['schemas']['LoginResponse']>('/api/v1/auth/login', {
			method: 'POST',
			body: { email, password },
			skipAuthRedirect: true
		}),

	logout: () => request<void>('/api/v1/auth/logout', { method: 'POST', skipAuthRedirect: true }),

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

	completeSale: (saleId: string) =>
		request<CompletedSale>(`/api/v1/sales/${saleId}/complete`, { method: 'POST' }),

	cancelSale: (saleId: string) =>
		request<void>(`/api/v1/sales/${saleId}/cancel`, { method: 'POST' })
};
