<script lang="ts">
	let { data } = $props();
</script>

<h1>Usuarios</h1>

{#if !data.canManage}
	<p class="error" role="alert">
		Solo el rol <strong>owner</strong> puede gestionar usuarios.
	</p>
{:else}
	<div class="table-wrap empty">
		El listado de usuarios todavía no está disponible: el backend solo expone el alta
		(<code>POST /api/v1/users</code>).
	</div>
{/if}
