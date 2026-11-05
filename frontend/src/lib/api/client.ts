/**
 * Cliente HTTP de la API.
 *
 * Reglas que este archivo centraliza para que ninguna pantalla las repita:
 *
 *  1. **Rutas relativas** (`/api/v1/...`). Nunca una URL con host: en dev las
 *     resuelve el proxy de Vite y en producción el mismo Axum. Hardcodear el
 *     host es lo que hace que una app funcione en dev y falle al desplegarse.
 *  2. **`401` → login.** Una sesión vencida no debe producir un error raro
 *     distinto en cada pantalla.
 *  3. **`429` → tiempo de espera.** El backend manda `Retry-After`; mostrarlo
 *     es lo que hace el mensaje accionable.
 *  4. **Errores en `application/problem+json`** (RFC 7807). Se parsean a
 *     `ApiError`, no a un string suelto.
 */

import type { components } from './schema';

// ---------- Tipos derivados del spec OpenAPI ----------
//
// No se escriben a mano: si el backend cambia un campo, `schema.d.ts` cambia y
// estos alias arrastran el cambio hasta la pantalla.

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

/** Error normalizado de la API. */
export class ApiError extends Error {
	readonly status: number;
	/** Segundos hasta reintentar, solo presente en un `429`. */
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
	/** Query params. Los `undefined` se omiten. */
	query?: Record<string, string | number | boolean | undefined | null>;
	/** Evita el redirect automático a /login (lo usa el guard del layout). */
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
		// Mismo origen: la cookie HttpOnly viaja sola. `same-origin` es explícito
		// para que nadie lo cambie por `include` sin pensarlo.
		credentials: 'same-origin',
		headers: body === undefined ? undefined : { 'content-type': 'application/json' },
		body: body === undefined ? undefined : JSON.stringify(body)
	});

	if (response.ok) {
		// 204 No Content: no hay body que parsear.
		if (response.status === 204) return undefined as T;
		return (await response.json()) as T;
	}

	const retryAfterHeader = response.headers.get('retry-after');
	const retryAfter = retryAfterHeader ? Number(retryAfterHeader) : null;

	const problem = (await response.json().catch(() => null)) as Problem | null;

	if (response.status === 401 && !skipAuthRedirect) {
		// Sesión vencida o inexistente: una sola pantalla, un solo destino.
		// `replace` para no dejar la página protegida en el historial.
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

	listProducts: (params: { search?: string; kind?: ProductKind; includeInactive?: boolean }) =>
		request<ProductList>('/api/v1/products', {
			query: {
				search: params.search,
				kind: params.kind,
				include_inactive: params.includeInactive
			}
		}),

	getProduct: (id: string) => request<Product>(`/api/v1/products/${id}`),

	listMovements: (id: string) => request<Movement[]>(`/api/v1/products/${id}/movements`),

	// --- Ventas ---
	//
	// Cada mutación devuelve la venta COMPLETA recalculada. El cliente no suma
	// ni valida: solo muestra lo que el servidor dice.

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
