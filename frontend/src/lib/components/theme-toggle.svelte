<script lang="ts">
	import { toggleMode, mode } from 'mode-watcher';
	import SunIcon from '@lucide/svelte/icons/sun';
	import MoonIcon from '@lucide/svelte/icons/moon';
	import MonitorIcon from '@lucide/svelte/icons/monitor';
	import { Button } from '$lib/components/ui/button';
	import { cn } from '$lib/utils';

	/**
	 * Toggle light and dark. It starts in `system` mode and the first click sets
	 * an explicit preference. Two states suffice: clicking requests the opposite
	 * of the current appearance.
	 */
	const isDark = $derived(mode.current === 'dark');

	// The mounting location controls width; only content is set here.
	let { class: className = '' } = $props();
</script>

<Button
	variant="ghost"
	size="sm"
	onclick={toggleMode}
	title={isDark ? 'Switch to light theme' : 'Switch to dark theme'}
	aria-label={isDark ? 'Switch to light theme' : 'Switch to dark theme'}
	class={cn('justify-start gap-2 font-normal', className)}
>
	{#if isDark}
		<SunIcon class="size-4" />
		Light
	{:else}
		<MoonIcon class="size-4" />
		Dark
	{/if}
</Button>
