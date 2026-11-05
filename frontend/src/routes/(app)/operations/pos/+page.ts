import { api, type Product, type Sale } from '$lib/api/client';

/**
 * Load the sale screen. The draft is opened on the server: on reload,
 * `openDraft` returns the same cart instead of creating an empty one, so the
 * cart survives a refresh or network interruption.
 */
export async function load() {
	const [list, draft] = await Promise.all([api.listProducts({}), api.openDraft()]);
	const sale: Sale = await api.getSale(draft.id);

	const sellable: Product[] = list.items.filter(
		(p) => p.kind === 'product' && p.is_active
	);

	return { products: sellable, saleId: draft.id, sale };
}
