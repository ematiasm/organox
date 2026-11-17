import { api, type Movement, type Product } from '$lib/api/client';

/**
 * Recent activity, not a complete ledger. The backend exposes movements for one
 * product at a time (`GET /api/v1/products/{id}/movements`), not a paginated
 * global history, so this screen fans out over the first products of the
 * catalog. It says so: the header names how many products it covers and how many
 * exist, and there is no page number because there is no second page.
 *
 * A global movements endpoint is tracked as a separate follow-up; until it
 * exists, this screen deliberately shows a sample rather than pretending to be
 * a history.
 */
const MAX_PRODUCTS = 12;

export type MovementRow = Movement & {
	product_id: string;
	product_name: string;
	product_sku: string | null;
};

export async function load() {
	const list = await api.listProducts({ includeInactive: true });
	const products: Product[] = list.items.slice(0, MAX_PRODUCTS);

	const batches = await Promise.all(
		products.map(async (product) => {
			const movements = await api.listMovements(product.id).catch(() => [] as Movement[]);
			return movements.map((movement) => ({
				...movement,
				product_id: product.id,
				product_name: product.name,
				product_sku: product.sku ?? null
			}));
		})
	);

	const movements: MovementRow[] = batches
		.flat()
		.sort((a, b) => b.created_at.localeCompare(a.created_at))
		.slice(0, 50);

	// `total` counts every product the filters match; `items.length` is only the
	// page the API returned. Before #26 the screen printed `items.length` as the
	// catalog size, which understated it whenever the API page was shorter.
	return { movements, productCount: products.length, totalProducts: list.total ?? 0 };
}
