import { api, type Receivables } from '$lib/api/client';

// `ssr = false` is inherited from the private layout's +layout.ts.
export async function load() {
	const report: Receivables = await api.getReceivables();
	return { report };
}
