import { api, type Payables } from '$lib/api/client';

// `ssr = false` is inherited from the private layout's +layout.ts.
export async function load() {
	const report: Payables = await api.getPayables();
	return { report };
}
