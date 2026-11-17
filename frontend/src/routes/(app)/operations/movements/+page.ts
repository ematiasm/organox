import { api, type Movement, type Product } from '$lib/api/client';

/**
 * Recent activity, not a complete ledger. The backend exposes movements for one
 * product at a time (`GET /api/v1/products/{id}/movements`), not a paginated
 * global history, so this screen fans out over the first products of the
 * catalog. It says so: the header names how many products it covers and how
 * many exist, there is no page number because there is no second page, and the
 * footer says when the movement cap discarded older rows.
 *
 * A global movements endpoint is tracked as a separate follow-up; until it
 * exists, this screen deliberately shows a sample rather than pretending to be
 * a history.
 */
const MAX_PRODUCTS = 12;

/** The screen never shows more movements than this. The footer says so when it bites. */
const MAX_MOVEMENTS = 50;

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

	const sorted: MovementRow[] = batches
		.flat()
		.sort((a, b) => b.created_at.localeCompare(a.created_at));
	const movements: MovementRow[] = sorted.slice(0, MAX_MOVEMENTS);

	// The per-product endpoint also caps at 50 per request and reports no total,
	// so a product with a longer history is indistinguishable from one with
	// exactly 50. Disclosing that needs a `total` on the movements endpoint —
	// part of the global-history follow-up, not something the client can detect.
	const truncated = sorted.length > MAX_MOVEMENTS;

	// `total` is required by the OpenAPI contract, so no `?? 0` fallback: it
	// would conflate a contract violation with "the catalog is empty" and
	// silence the sample warning in the header.
	return { movements, productCount: products.length, totalProducts: list.total, truncated };
}
