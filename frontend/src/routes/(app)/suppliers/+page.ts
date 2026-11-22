import { api, ApiError, type Supplier } from '$lib/api/client';

const PAGE_SIZE = 50;

export async function load({ url }: { url: URL }) {
	const search = url.searchParams.get('search') ?? '';
	const page = Math.max(1, Number(url.searchParams.get('page') ?? '1') || 1);
	const includeInactive = url.searchParams.get('include_inactive') === '1';

	const list = await api.listSuppliers({
		search: search || undefined,
		includeInactive,
		limit: PAGE_SIZE,
		offset: (page - 1) * PAGE_SIZE
	});

	const total = list.total ?? 0;
	const id = url.searchParams.get('id') ?? '';
	const creating = url.searchParams.get('new') === '1';

	let supplier: Supplier | null = null;
	if (id) {
		try {
			supplier = await api.getSupplier(id);
		} catch (error) {
			if (error instanceof ApiError && error.status === 404) {
				supplier = null;
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
		supplier,
		items: list.items ?? [],
		limit: list.limit ?? PAGE_SIZE,
		offset: list.offset ?? 0,
		total,
		lastPage: Math.max(1, Math.ceil(total / (list.limit ?? PAGE_SIZE)))
	} satisfies {
		search: string;
		page: number;
		includeInactive: boolean;
		id: string;
		creating: boolean;
		supplier: Supplier | null;
		items: Supplier[];
		limit: number;
		offset: number;
		total: number;
		lastPage: number;
	};
}
