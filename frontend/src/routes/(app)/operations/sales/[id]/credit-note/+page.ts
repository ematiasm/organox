import { api, ApiError } from '$lib/api/client';

/**
 * The read endpoint is the screen's only source of truth: it reports the
 * charged lines, what is still returnable and every note already issued. An
 * unknown sale is a not-found state, not an empty return form.
 */
export async function load({ params }: { params: { id: string } }) {
	try {
		const credits = await api.saleCreditNotes(params.id);
		return {
			saleId: params.id,
			credits,
			notFound: credits.items.length === 0
		};
	} catch (error) {
		if (error instanceof ApiError && error.status === 404) {
			return { saleId: params.id, credits: null, notFound: true };
		}
		throw error;
	}
}
