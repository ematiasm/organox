<script lang="ts">
	let { data } = $props();
</script>

<h1>Users</h1>

{#if !data.canManage}
	<p class="error" role="alert">
		Only the <strong>owner</strong> role can manage users.
	</p>
{:else}
	<div class="table-wrap empty">
		The user list is not available yet: the backend only exposes user creation
		(<code>POST /api/v1/users</code>).
	</div>
{/if}
