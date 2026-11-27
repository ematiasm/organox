import { api, type Receivables } from '$lib/api/client';

const PAGE_SIZE = 50;

// `ssr = false` is inherited from the private layout's +layout.ts.
export async function load({ url }: { url: URL }) {
	const rawPage = Number(url.searchParams.get('page') ?? '1');
	const page = Number.isFinite(rawPage) && rawPage >= 1 ? Math.trunc(rawPage) : 1;
	const offset = (page - 1) * PAGE_SIZE;

	const report: Receivables = await api.getReceivables({ limit: PAGE_SIZE, offset });
	const total = report.items_total ?? 0;

	return {
		report,
		page,
		limit: PAGE_SIZE,
		offset,
		total,
		lastPage: Math.max(1, Math.ceil(total / PAGE_SIZE))
	};
}
