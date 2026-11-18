<script lang="ts">
	import { Badge } from '$lib/components/ui/badge';
	import {
		Table,
		TableBody,
		TableCell,
		TableHead,
		TableHeader,
		TableRow
	} from '$lib/components/ui/table';

	let { data } = $props();

	let counter = $derived.by(() => {
		const count = data.users.length;
		return `${count} ${count === 1 ? 'user' : 'users'}`;
	});
</script>

<h1>Users</h1>

{#if !data.canManage}
	<p class="error" role="alert">
		Only the <strong>owner</strong> role can manage users.
	</p>
{:else}
	{#if data.users.length === 0}
		<div class="rounded-lg border border-border p-10 text-center text-sm text-muted-foreground">
			No users have been added yet.
		</div>
	{:else}
		<div class="overflow-hidden rounded-lg border border-border">
			<Table>
				<TableHeader>
					<TableRow>
						<TableHead>Email</TableHead>
						<TableHead>Role</TableHead>
					</TableRow>
				</TableHeader>
				<TableBody>
					{#each data.users as user (user.id)}
						<TableRow>
							<TableCell class="font-medium">{user.email}</TableCell>
							<TableCell>
								<Badge variant="outline">
									{user.role === 'owner' ? 'Owner' : 'Manager'}
								</Badge>
							</TableCell>
						</TableRow>
					{/each}
				</TableBody>
			</Table>
		</div>
	{/if}

	<div class="mt-3 text-sm text-muted-foreground">{counter}</div>
{/if}
