import { redirect } from '@sveltejs/kit';
import { api, ApiError, type SessionUser } from '$lib/api/client';

/**
 * Session guard for every private screen. It lives in `(app)` because the rail
 * needs the user before mounting. Data is requested once per load, not on each
 * client navigation; the root layout therefore has no load of its own.
 */
export const ssr = false;

export async function load(): Promise<{ user: SessionUser }> {
	try {
		const user = await api.me();
		return { user };
	} catch (error) {
		// SvelteKit `redirect` throws a control-flow exception, so check for
		// `ApiError` rather than catching the entire range.
		if (error instanceof ApiError && error.isUnauthorized) {
			redirect(307, '/login');
		}
		if (error instanceof Error) throw error;
		throw new Error(String(error));
	}
}
