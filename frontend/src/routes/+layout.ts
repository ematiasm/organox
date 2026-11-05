// SPA without SSR: the browser receives a shell and renders the application.
// `prerender = false` because routes need database data unavailable at build time.
export const ssr = false;
export const prerender = false;
