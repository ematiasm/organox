import { redirect } from '@sveltejs/kit';

// `/pos` fue la primera URL del punto de venta. Se mantiene como redirección
// para no romper enlaces guardados ni el historial de los cajeros.
export function load() {
	redirect(307, '/operations/pos');
}
