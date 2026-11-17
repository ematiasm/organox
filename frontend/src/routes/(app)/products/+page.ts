import { api, ApiError, type Movement, type Product } from '$lib/api/client';

/** Products per page. The API defaults to 50 and caps at 200.
 *
 * Deliberately not exported: SvelteKit treats every export of a `+page.ts` as
 * routing metadata and rejects names it does not know (`validate_page_exports`,
 * used in dev), so an exported constant breaks the route. Nothing else imports
 * it, and a value that must match the screen lives next to the screen.
 */
const PAGE_SIZE = 50;

// `ssr = false` is inherited from the private layout's +layout.ts.
export async function load({ url }: { url: URL }) {
	const search = url.searchParams.get('search') ?? '';

	// `page` is 1-based in the URL because it is what a human shares, and
	// `offset` is 0-based in the API because it is what SQL takes. Converting
	// here keeps both honest: the URL never shows `page=0` and the request never
	// sends `offset=-50`.
	const page = Math.max(1, Number(url.searchParams.get('page') ?? '1') || 1);

	const list = await api.listProducts({
		search: search || undefined,
		limit: PAGE_SIZE,
		offset: (page - 1) * PAGE_SIZE
	});

	const total = list.total ?? 0;

	// `id` selects the drawer's product. The URL is the source of truth for the
	// same reason as `search` and `page`: Back closes the drawer and a link to a
	// product is shareable.
	const id = url.searchParams.get('id') ?? '';

	// A stale `?id=` (removed elsewhere, hand-typed URL) must not take the whole
	// list down: the drawer says "not found" and the list stays usable. Any
	// other error is the server's to report.
	let product: Product | null = null;
	let movements: Movement[] = [];
	if (id) {
		try {
			product = await api.getProduct(id);
		} catch (error) {
			if (error instanceof ApiError && error.status === 404) {
				product = null;
			} else {
				throw error;
			}
		}
		// Same tolerance as the movements screen: a failed movements fetch must
		// not cost the drawer its product data.
		movements = product ? await api.listMovements(id).catch(() => [] as Movement[]) : [];
	}

	return {
		search,
		page,
		id,
		product,
		movements,
		items: list.items ?? [],
		limit: list.limit ?? PAGE_SIZE,
		// The offset the server actually used, not the one requested: the screen
		// shows the range it received, so it must read the server's answer.
		offset: list.offset ?? 0,
		total,
		// Last page, never below 1: an empty catalog is still "page 1 of 1", which
		// is what the screen shows.
		lastPage: Math.max(1, Math.ceil(total / (list.limit ?? PAGE_SIZE)))
	} satisfies {
		search: string;
		page: number;
		id: string;
		product: Product | null;
		movements: Movement[];
		items: Product[];
		limit: number;
		offset: number;
		total: number;
		lastPage: number;
	};
}
