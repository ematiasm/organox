<script lang="ts">
	import type { User } from '$lib/api/client';
	import UserPasswordResetDialog from '$lib/components/user-password-reset-dialog.svelte';
	import { Badge } from '$lib/components/ui/badge';
	import { Button } from '$lib/components/ui/button';
	import KeyRoundIcon from '@lucide/svelte/icons/key-round';
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

	let resetTarget = $state<User | null>(null);
	let resetOpen = $state(false);

	function openReset(user: User) {
		resetTarget = user;
		resetOpen = true;
	}
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
						<TableHead>Actions</TableHead>
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
							<TableCell class="text-right">
								<Button
									variant="outline"
									size="sm"
									aria-label={`Reset password for ${user.email}`}
									onclick={() => openReset(user)}
								>
									<KeyRoundIcon />
									Reset password
								</Button>
							</TableCell>
						</TableRow>
					{/each}
				</TableBody>
			</Table>
		</div>
	{/if}

	<div class="mt-3 text-sm text-muted-foreground">{counter}</div>

	<UserPasswordResetDialog open={resetOpen} user={resetTarget} onClose={() => (resetOpen = false)} />
{/if}
