<script lang="ts">
	import { Button } from '$lib/components/ui/button';
	import { Input } from '$lib/components/ui/input';
	import { Label } from '$lib/components/ui/label';

	let email = $state('');
	let password = $state('');
	let error = $state<string | null>(null);
	let retryAfter = $state<number | null>(null);
	let loading = $state(false);

	async function submit(event: SubmitEvent) {
		event.preventDefault();
		error = null;
		retryAfter = null;
		loading = true;

		try {
			const response = await fetch('/api/v1/auth/login', {
				method: 'POST',
				headers: { 'content-type': 'application/json' },
				credentials: 'same-origin',
				body: JSON.stringify({ email, password })
			});

			if (response.ok) {
				window.location.href = '/products';
				return;
			}

			// 429: account locked. The header indicates the remaining wait.
			if (response.status === 429) {
				const seconds = Number(response.headers.get('retry-after'));
				retryAfter = Number.isFinite(seconds) && seconds > 0 ? seconds : null;
				error =
					retryAfter !== null
						? `Too many attempts. Try again in ${retryAfter} ${retryAfter === 1 ? 'second' : 'seconds'}.`
						: 'Too many attempts. Wait a few minutes.';
				return;
			}

			// The backend returns RFC 7807; show its `detail` if present.
			const problem = await response.json().catch(() => null);
			error = problem?.detail ?? 'Could not sign in.';
		} catch {
			error = 'Could not connect to the server.';
		} finally {
			loading = false;
		}
	}
</script>

<main class="flex min-h-screen items-center justify-center p-4">
	<div class="w-full max-w-sm rounded-lg border border-border bg-card p-8">
		<h1 class="text-lg font-semibold tracking-tight">Administration</h1>
		<p class="mb-6 text-sm text-muted-foreground">Sign in with your account.</p>

		{#if error}
			<p
				role="alert"
				class="mb-4 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-sm text-destructive"
			>
				{error}
			</p>
		{/if}

		<form onsubmit={submit} class="flex flex-col gap-4">
			<div class="flex flex-col gap-1.5">
				<Label for="email">Email</Label>
				<Input id="email" type="email" bind:value={email} autocomplete="username" required />
			</div>

			<div class="flex flex-col gap-1.5">
				<Label for="password">Password</Label>
				<Input
					id="password"
					type="password"
					bind:value={password}
					autocomplete="current-password"
					required
				/>
			</div>

			<Button type="submit" disabled={loading} class="mt-2">
				{loading ? 'Signing in…' : 'Sign in'}
			</Button>
		</form>
	</div>
</main>
