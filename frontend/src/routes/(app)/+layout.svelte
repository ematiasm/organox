<script lang="ts">
	import AppShell from '$lib/components/app-shell.svelte';

	/** Private layout: a single rail and the `main` page content. Keeping the
	 * rail outside individual screens avoids duplicate navigation on the POS. */
	let { data, children } = $props();
</script>

<AppShell user={data.user}>
	{@render children()}
</AppShell>
