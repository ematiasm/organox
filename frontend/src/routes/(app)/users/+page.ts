import { api, type Role } from '$lib/api/client';

/**
 * User list. The endpoint is owner-only, so managers keep the gate message
 * without making a request they are guaranteed to receive a 403 for.
 */
export async function load({ parent }: { parent: () => Promise<{ user: { role: Role } }> }) {
	const { user } = await parent();
	const canManage = user.role === 'owner';

	return {
		canManage,
		users: canManage ? await api.listUsers() : []
	};
}
