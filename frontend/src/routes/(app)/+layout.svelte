<script lang="ts">
	import AppShell from '$lib/components/app-shell.svelte';

	/**
	 * Layout de las pantallas privadas: el rail (navegación única) más el `main`
	 * donde se dibuja la página. El rail no es de cada pantalla porque el punto
	 * de venta tenía el suyo y se veían dos navegaciones a la vez.
	 */
	let { data, children } = $props();
</script>

<AppShell user={data.user}>
	{@render children()}
</AppShell>
