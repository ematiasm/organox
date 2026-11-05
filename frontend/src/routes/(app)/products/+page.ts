import { api, type Product } from '$lib/api/client';

// `ssr = false` is inherited from the private layout's +layout.ts.
export async function load({ url }: { url: URL }) {
	const search = url.searchParams.get('search') ?? '';

	const list = await api.listProducts({ search: search || undefined });

	return {
		search,
		items: list.items ?? [],
		limit: list.limit,
		offset: list.offset
	} satisfies { search: string; items: Product[]; limit: number; offset: number };
}
