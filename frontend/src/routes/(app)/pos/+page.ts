import { redirect } from '@sveltejs/kit';

// `/pos` was the first POS URL. Keep it as a redirect to preserve saved links
// and cashiers' history.
export function load() {
	redirect(307, '/operations/pos');
}
