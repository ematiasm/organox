import { api, type Movement, type Product } from '$lib/api/client';

/**
 * Load movements. The backend exposes movements for one product at a time
 * (`GET /api/v1/products/{id}/movements`), not a paginated global history.
 * Fetch recent products' movements in parallel and sort them by date.
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

	return { movements, productCount: products.length, totalProducts: list.items.length };
}
