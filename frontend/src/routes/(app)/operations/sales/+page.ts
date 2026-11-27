import { api } from '$lib/api/client';

// SvelteKit treats every export of a +page.ts as routing metadata and rejects
// unknown names (validate_page_exports), so the page size stays private.
const PAGE_SIZE = 50;

/**
 * The backend already returns only completed sales, newest first, paginated.
 * The screen shows that server order as-is instead of sorting or filtering in
 * the browser; a sale is never charged twice by this list.
 */
export async function load({ url }: { url: URL }) {
	// `page` is 1-based in the URL; `offset` is 0-based for SQL. Truncate to an
	// integer so fractional query values like `?page=1.5` never produce skewed
	// offsets.
	const rawPage = Number(url.searchParams.get('page') ?? '1');
	const page = Number.isFinite(rawPage) && rawPage >= 1 ? Math.trunc(rawPage) : 1;

	const list = await api.listSales({
		limit: PAGE_SIZE,
		offset: (page - 1) * PAGE_SIZE
	});

	const limit = list.limit ?? PAGE_SIZE;
	const total = list.total ?? 0;

	return {
		page,
		items: list.items ?? [],
		limit,
		// The offset the server actually used, not the one requested.
		offset: list.offset ?? 0,
		total,
		lastPage: Math.max(1, Math.ceil(total / limit))
	};
}
