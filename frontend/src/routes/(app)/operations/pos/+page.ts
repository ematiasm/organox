import { api, type Product, type Sale } from '$lib/api/client';

/**
 * Carga de la pantalla de venta.
 *
 * El borrador se abre en el servidor: si el cajero recarga, `openDraft`
 * devuelve el mismo carrito en vez de crear uno vacío. Por eso el carrito
 * sobrevive un F5 y un corte de red.
 */
export async function load() {
	const [list, draft] = await Promise.all([api.listProducts({}), api.openDraft()]);
	const sale: Sale = await api.getSale(draft.id);

	const sellable: Product[] = list.items.filter(
		(p) => p.kind === 'product' && p.is_active
	);

	return { products: sellable, saleId: draft.id, sale };
}
