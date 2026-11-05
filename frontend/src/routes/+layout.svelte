<script lang="ts">
	import { ModeWatcher } from 'mode-watcher';
	import '@fontsource-variable/inter';
	import '../app.css';

	let { children } = $props();
</script>

<!--
  `ModeWatcher` applies `.dark` to `<html>` from `localStorage` (key
  `mode-watcher-mode`), following `prefers-color-scheme` in `system` mode.
  Without it, the `.dark` variables in app.css are unreachable.
-->
<ModeWatcher defaultMode="system" />

{@render children()}
