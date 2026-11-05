import { redirect } from '@sveltejs/kit';

// La raíz no tiene contenido propio: el login decide el destino.
export function load() {
	redirect(307, '/products');
}
