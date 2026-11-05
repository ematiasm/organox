import adapter from '@sveltejs/adapter-static';
import tailwindcss from '@tailwindcss/vite';
import { sveltekit } from '@sveltejs/kit/vite';
import path from 'node:path';
import { defineConfig } from 'vite';

// SvelteKit 3 dropped `svelte.config.js`: configuration goes in the plugin.
// (In Kit 2 this lived in svelte.config.js; if you find old tutorials, that is
// the reason for the difference.)
export default defineConfig({
	plugins: [
		tailwindcss(),
		sveltekit({
			// Pure SPA: all routing happens in the browser and Axum serves
			// `index.html` as the fallback. `strict: false` is required because
			// there are dynamic routes (/products/[id]) that are not prerendered.
			adapter: adapter({ fallback: 'index.html', strict: false })
		})
	],
	resolve: {
		// Standard Vite alias (not the `sveltekit({ alias })` option, which is
		// deprecated in Kit 3).
		alias: {
			$lib: path.resolve('./src/lib')
		}
	},
	server: {
		port: 5173,
		// The proxy is what makes the session cookie work in dev. Without it the
		// browser sees two origins (:5173 and :3000), SameSite=Strict does not
		// send the cookie, and everything returns 401.
		proxy: {
			'/api': {
				target: 'http://localhost:3000',
				changeOrigin: false
			}
		}
	}
});
