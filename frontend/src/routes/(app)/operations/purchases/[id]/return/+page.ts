import { api, ApiError } from '$lib/api/client';

export async function load({ params }: { params: { id: string } }) {
	try {
		const summary = await api.purchaseReturns(params.id);
		return {
			purchaseId: params.id,
			summary,
			notFound: false
		};
	} catch (error) {
		if (error instanceof ApiError && error.status === 404) {
			return { purchaseId: params.id, summary: null, notFound: true };
		}
		throw error;
	}
}
