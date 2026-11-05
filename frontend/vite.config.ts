import adapter from '@sveltejs/adapter-static';
import tailwindcss from '@tailwindcss/vite';
import { sveltekit } from '@sveltejs/kit/vite';
import path from 'node:path';
import { defineConfig } from 'vite';

// SvelteKit 3 eliminó `svelte.config.js`: la configuración va en el plugin.
// (En Kit 2 esto vivía en svelte.config.js; si encontrás tutoriales viejos,
// ese es el motivo de la diferencia.)
export default defineConfig({
	plugins: [
		tailwindcss(),
		sveltekit({
			// SPA pura: todo el ruteo ocurre en el navegador y Axum entrega
			// `index.html` como fallback. `strict: false` es necesario porque
			// hay rutas dinámicas (/products/[id]) que no se prerenderizan.
			adapter: adapter({ fallback: 'index.html', strict: false })
		})
	],
	resolve: {
		// Alias estándar de Vite (no la opción `sveltekit({ alias })`, que está
		// deprecada en Kit 3).
		alias: {
			$lib: path.resolve('./src/lib')
		}
	},
	server: {
		port: 5173,
		// El proxy es lo que permite que la cookie de sesión funcione en dev.
		// Sin esto, el navegador ve dos orígenes (:5173 y :3000), SameSite=Strict
		// no manda la cookie, y todo da 401.
		proxy: {
			'/api': {
				target: 'http://localhost:3000',
				changeOrigin: false
			}
		}
	}
});
