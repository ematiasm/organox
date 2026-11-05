import { api, type Movement, type Product } from '$lib/api/client';

/**
 * Carga de Movimientos.
 *
 * El backend solo expone los movimientos de UN producto
 * (`GET /api/v1/products/{id}/movements`), no un historial global con
 * paginación. Para que la pantalla muestre datos reales y no una lista vacía,
 * se toman los últimos productos y se piden sus movimientos en paralelo,
 * ordenando después por fecha.
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
