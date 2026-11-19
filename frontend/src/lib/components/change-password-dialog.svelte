<script lang="ts">
	import { api, ApiError } from '$lib/api/client';
	import { Button } from '$lib/components/ui/button';
	import * as Dialog from '$lib/components/ui/dialog';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';

	let { open, onClose }: { open: boolean; onClose: () => void } = $props();

	let currentPassword = $state('');
	let newPassword = $state('');
	let saving = $state(false);
	let formError = $state<string | null>(null);
	let changed = $state(false);

	// Never carry a previous attempt (or its error/success) into the next open.
	$effect(() => {
		if (!open) return;
		currentPassword = '';
		newPassword = '';
		formError = null;
		changed = false;
	});

	/**
	 * The API client does not redirect on 401 for this route, so the status is
	 * safe to interpret here: 401 is a bad current password, 422 is the server's
	 * validation detail, and everything else (including network and 5xx) is
	 * reported generically instead of being silently ignored.
	 */
	function describeError(error: unknown): string {
		if (!(error instanceof ApiError)) {
			return 'Could not change the password. Check your connection and try again.';
		}
		if (error.status === 401) return 'The current password is incorrect.';
		if (error.status === 422) {
			return error.problem?.detail ?? 'The new password does not meet the requirements.';
		}
		return 'Could not change the password. Try again.';
	}

	async function submit() {
		saving = true;
		formError = null;
		try {
			await api.changePassword({
				current_password: currentPassword,
				new_password: newPassword
			});
			changed = true;
			currentPassword = '';
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
			<Dialog.Title>Change password</Dialog.Title>
			<Dialog.Description>
				Your current session stays signed in. Other sessions will be ended.
			</Dialog.Description>
		</Dialog.Header>

		{#if changed}
			<div role="status" class="rounded-md border border-border p-3 text-sm">
				Password changed. This session stays signed in; other sessions were ended.
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
					<Label for="current-password">Current password</Label>
					<Input
						id="current-password"
						type="password"
						autocomplete="current-password"
						bind:value={currentPassword}
						required
					/>
				</div>
				<div class="grid gap-2">
					<Label for="new-password">New password</Label>
					<Input
						id="new-password"
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
						{saving ? 'Changing…' : 'Change password'}
					</Button>
				</Dialog.Footer>
			</form>
		{/if}
	</Dialog.Content>
</Dialog.Root>
