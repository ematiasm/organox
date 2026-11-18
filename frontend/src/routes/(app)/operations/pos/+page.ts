import { api, type Product, type Sale } from '$lib/api/client';

/**
 * Load the sale screen. The draft is opened on the server: on reload,
 * `openDraft` returns the same cart instead of creating an empty one, so the
 * cart survives a refresh or network interruption.
 */
export async function load() {
	// The methods list request omits `include_inactive`, so the server returns
	// only methods that can accept a charge. The extra filter keeps the picker
	// honest even if that default ever changes.
	const [list, draft, methods] = await Promise.all([
		api.listProducts({}),
		api.openDraft(),
		api.listPaymentMethods()
	]);
	const sale: Sale = await api.getSale(draft.id);

	const sellable: Product[] = list.items.filter(
		(p) => p.kind === 'product' && p.is_active
	);

	return {
		products: sellable,
		saleId: draft.id,
		sale,
		methods: methods.filter((method) => method.is_active)
	};
}
