import { api } from '$lib/api/client';

/**
 * The backend already returns only completed sales, newest first, and caps the
 * list at 50. The screen shows that server order as-is instead of sorting or
 * filtering in the browser; a sale is never charged twice by this list.
 */
export async function load() {
	const sales = await api.listSales();
	return { sales };
}
