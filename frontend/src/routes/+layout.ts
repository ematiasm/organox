// SPA: sin SSR. El navegador recibe un cascarón y arma todo.
// `prerender = false` porque las rutas necesitan datos de la DB que no existen
// durante `vite build`.
export const ssr = false;
export const prerender = false;
