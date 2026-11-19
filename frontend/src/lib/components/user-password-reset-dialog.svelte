<script lang="ts">
	import { api, ApiError, type User } from '$lib/api/client';
	import { Button } from '$lib/components/ui/button';
	import * as Dialog from '$lib/components/ui/dialog';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';

	let {
		open,
		user,
		onClose
	}: { open: boolean; user: User | null; onClose: () => void } = $props();

	let newPassword = $state('');
	let saving = $state(false);
	let formError = $state<string | null>(null);
	let reset = $state(false);

	// A reset dialog is reused for each row: clear the previous target's input,
	// error and success before it opens for another user.
	$effect(() => {
		if (!open) return;
		user?.id;
		newPassword = '';
		formError = null;
		reset = false;
	});

	function describeError(error: unknown): string {
		if (error instanceof ApiError && error.problem) return error.problem.detail;
		return 'Could not reset the password. Check your connection and try again.';
	}

	async function submit() {
		if (!user) return;
		saving = true;
		formError = null;
		try {
			await api.resetUserPassword(user.id, { new_password: newPassword });
			reset = true;
			newPassword = '';
		} catch (error) {
			formError = describeError(error);
		} finally {
			saving = false;
		}
	}
</script>

<Dialog.Root {open} onOpenChange={(o) => !o && onClose()}>
	<Dialog.Content class="sm:max-w-md">
		<Dialog.Header>
			<Dialog.Title>Reset password</Dialog.Title>
			<Dialog.Description>
				Set a new password for {user?.email ?? 'this user'}. The owner does not need the current
				password.
			</Dialog.Description>
		</Dialog.Header>

		{#if reset}
			<div role="status" class="rounded-md border border-border p-3 text-sm">
				Password reset for {user?.email}. All sessions of that user were ended; if it was your
				own account, this session stays signed in.
			</div>
			<Dialog.Footer>
				<Button onclick={onClose}>Done</Button>
			</Dialog.Footer>
		{:else}
			<form
				class="grid gap-4"
				onsubmit={(event) => {
					event.preventDefault();
					submit();
				}}
			>
				<div class="grid gap-2">
					<Label for="reset-password">New password</Label>
					<Input
						id="reset-password"
						type="password"
						autocomplete="new-password"
						bind:value={newPassword}
						required
					/>
				</div>
				{#if formError}
					<p role="alert" class="text-sm text-destructive">{formError}</p>
				{/if}
				<Dialog.Footer>
					<Button type="button" variant="outline" onclick={onClose}>Cancel</Button>
					<Button type="submit" disabled={saving}>
						{saving ? 'Resetting…' : 'Reset password'}
					</Button>
				</Dialog.Footer>
			</form>
		{/if}
	</Dialog.Content>
</Dialog.Root>
