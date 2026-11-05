import { redirect } from '@sveltejs/kit';
import { api, ApiError, type SessionUser } from '$lib/api/client';

/**
 * Guard de sesión de TODA pantalla privada.
 *
 * Vive en el grupo `(app)` y no en la raíz porque el rail necesita el usuario:
 * el rail se dibuja en `+layout.svelte` de este mismo grupo, así que los datos
 * ya están resueltos cuando el rail se monta, y solo se piden una vez por carga
 * (no en cada navegación del cliente). Ese es el motivo por el que el layout
 * raíz no hace `load` propio.
 */
export const ssr = false;

export async function load(): Promise<{ user: SessionUser }> {
	try {
		const user = await api.me();
		return { user };
	} catch (error) {
		// `redirect` de SvelteKit lanza una excepción de control; por eso se
		// compara con `instanceof ApiError` y no se atrapa todo el rango.
		if (error instanceof ApiError && error.isUnauthorized) {
			redirect(307, '/login');
		}
		if (error instanceof Error) throw error;
		throw new Error(String(error));
	}
}
