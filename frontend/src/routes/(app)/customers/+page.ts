import { api, ApiError, type Customer } from '$lib/api/client';

/** Customers per page. The API defaults to 50 and caps at 200.
 *
 * Deliberately not exported, same as the products screen: SvelteKit treats
 * every export of a `+page.ts` as routing metadata and rejects unknown names.
 */
const PAGE_SIZE = 50;

// `ssr = false` is inherited from the private layout's +layout.ts.
export async function load({ url }: { url: URL }) {
	const search = url.searchParams.get('search') ?? '';

	// `page` is 1-based in the URL (what a human shares), `offset` is 0-based
	// in the API (what SQL takes); converting here keeps both honest.
	const page = Math.max(1, Number(url.searchParams.get('page') ?? '1') || 1);

	// Deactivated customers are hidden until the operator asks for them, so
	// the default list is the one a sale can point at. It is URL state for the
	// same reason as search: a filtered list stays shareable and Back returns
	// to the previous view.
	const includeInactive = url.searchParams.get('include_inactive') === '1';

	const list = await api.listCustomers({
		search: search || undefined,
		includeInactive,
		limit: PAGE_SIZE,
		offset: (page - 1) * PAGE_SIZE
	});

	const total = list.total ?? 0;

	// `id` selects the drawer's customer; `?new` opens it empty for creation.
	// The URL is the source of truth: Back closes the drawer and a link to a
	// customer is shareable.
	const id = url.searchParams.get('id') ?? '';
	const creating = url.searchParams.get('new') === '1';

	// A stale `?id=` must not take the whole list down: the drawer says "not
	// found" and the list stays usable. Any other error is the server's.
	let customer: Customer | null = null;
	if (id) {
		try {
			customer = await api.getCustomer(id);
		} catch (error) {
			if (error instanceof ApiError && error.status === 404) {
				customer = null;
			} else {
				throw error;
			}
		}
	}

	return {
		search,
		page,
		includeInactive,
		id,
		creating,
		customer,
		items: list.items ?? [],
		limit: list.limit ?? PAGE_SIZE,
		offset: list.offset ?? 0,
		total,
		// Last page, never below 1: an empty registry is still "page 1 of 1".
		lastPage: Math.max(1, Math.ceil(total / (list.limit ?? PAGE_SIZE)))
	} satisfies {
		search: string;
		page: number;
		includeInactive: boolean;
		id: string;
		creating: boolean;
		customer: Customer | null;
		items: Customer[];
		limit: number;
		offset: number;
		total: number;
		lastPage: number;
	};
}
