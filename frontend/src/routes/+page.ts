import { redirect } from '@sveltejs/kit';

// The root has no content of its own; login determines the destination.
export function load() {
	redirect(307, '/products');
}
