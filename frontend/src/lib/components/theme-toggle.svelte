<script lang="ts">
	import { toggleMode, mode } from 'mode-watcher';
	import SunIcon from '@lucide/svelte/icons/sun';
	import MoonIcon from '@lucide/svelte/icons/moon';
	import MonitorIcon from '@lucide/svelte/icons/monitor';
	import { Button } from '$lib/components/ui/button';
	import { cn } from '$lib/utils';

	/**
	 * Un solo botón que alterna claro ↔ oscuro.
	 *
	 * Arranca en `system` (sigue a `prefers-color-scheme`), y el primer clic fija
	 * una preferencia explícita. No se ofrece un ciclo de tres estados porque en
	 * un admin de tres personas el modo "sistema" es el default razonable y dos
	 * estados alcanzan: el usuario que toca el botón quiere lo contrario de lo
	 * que ve.
	 */
	const isDark = $derived(mode.current === 'dark');

	// El lugar donde se monta decide el ancho; acá solo se fija el contenido.
	let { class: className = '' } = $props();
</script>

<Button
	variant="ghost"
	size="sm"
	onclick={toggleMode}
	title={isDark ? 'Cambiar a tema claro' : 'Cambiar a tema oscuro'}
	aria-label={isDark ? 'Cambiar a tema claro' : 'Cambiar a tema oscuro'}
	class={cn('justify-start gap-2 font-normal', className)}
>
	{#if isDark}
		<SunIcon class="size-4" />
		Claro
	{:else}
		<MoonIcon class="size-4" />
		Oscuro
	{/if}
</Button>
