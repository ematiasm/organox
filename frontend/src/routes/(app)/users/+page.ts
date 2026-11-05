import type { Role } from '$lib/api/client';

/**
 * User list. The backend does not yet expose `GET /users` (only `POST /users`).
 * This screen completes the sidebar and marks the pending work without
 * inventing data or pretending a list exists.
 */
export async function load({ parent }: { parent: () => Promise<{ user: { role: Role } }> }) {
	const { user } = await parent();
	return { canManage: user.role === 'owner' };
}
