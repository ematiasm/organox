import type { Role } from '$lib/api/client';

/**
 * Listado de usuarios.
 *
 * El backend NO expone `GET /users` todavía (solo `POST /users`). Esta pantalla
 * existe para dejar el sidebar completo y marcado el trabajo pendiente; no
 * inventa datos ni finge una lista.
 */
export async function load({ parent }: { parent: () => Promise<{ user: { role: Role } }> }) {
	const { user } = await parent();
	return { canManage: user.role === 'owner' };
}
