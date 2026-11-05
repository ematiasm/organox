<script lang="ts">
	import { ModeWatcher } from 'mode-watcher';
	import '@fontsource-variable/inter';
	import '../app.css';

	let { children } = $props();
</script>

<!--
  `ModeWatcher` aplica la clase `.dark` al `<html>` leyendo la preferencia de
  `localStorage` (clave `mode-watcher-mode`) y, en modo `system`, sigue a
  `prefers-color-scheme`. Sin este componente la clase nunca se aplica y las
  variables del bloque `.dark` de app.css quedan inalcanzables.
-->
<ModeWatcher defaultMode="system" />

{@render children()}
