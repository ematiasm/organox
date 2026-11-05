<script lang="ts">
	import type { Snippet } from 'svelte';
	import { page } from '$app/state';
	import { api, type SessionUser } from '$lib/api/client';
	import { Button } from '$lib/components/ui/button';
	import {
		DropdownMenu,
		DropdownMenuContent,
		DropdownMenuItem,
		DropdownMenuLabel,
		DropdownMenuSeparator,
		DropdownMenuTrigger
	} from '$lib/components/ui/dropdown-menu';
	import ThemeToggle from '$lib/components/theme-toggle.svelte';
	import CartIcon from '@lucide/svelte/icons/shopping-cart';
	import ListIcon from '@lucide/svelte/icons/list';
	import ArrowLeftRightIcon from '@lucide/svelte/icons/arrow-left-right';
	import UsersIcon from '@lucide/svelte/icons/users';
	import ChevronLeftIcon from '@lucide/svelte/icons/chevron-left';
	import ChevronUpIcon from '@lucide/svelte/icons/chevron-up';
	import LogOutIcon from '@lucide/svelte/icons/log-out';

	/**
	 * Cáscara de toda pantalla privada: un único sistema de navegación.
	 *
	 * Vive en el layout raíz (fuera de los grupos con guard) porque el guard de
	 * sesión es asíncrono: si el rail dependiera de sus datos, cada navegación
	 * desmontaría y remontaría la navegación, y el estado expandido se perdería.
	 * Acá los datos se piden una vez y el rail queda montado de punta a punta.
	 *
	 * El ítem activo se deriva de la URL: no hay estado que se desincronice al
	 * usar Atrás.
	 */
	let { user, children }: { user: SessionUser; children?: Snippet } = $props();

	type Item = {
		href: string;
		label: string;
		icon: typeof CartIcon;
		current?: (path: string) => boolean;
		// Solo `owner` ve Usuarios. Ocultar el link es cosmético: el 403 real lo
		// decide el backend. Ver docs/auth.md y S11 de odd/tasks/ui-admin.md.
		ownerOnly?: boolean;
	};

	type Group = { label: string; items: Item[] };

	// Iniciar venta no es un destino más: es la acción primaria del local. Se
	// pidió explícitamente que se abra desde el rail y no desde la página.
	const groups: Group[] = [
		{
			label: 'Operación',
			items: [{ href: '/operations/pos', label: 'Venta', icon: CartIcon }]
		},
		{
			label: 'Catálogo',
			items: [
				// `current` propio: el prefijo `/products` también matchearía un
				// futuro `/products/...` de detalle, que sí debe marcar Productos.
				{
					href: '/products',
					label: 'Productos',
					icon: ListIcon,
					current: (path) => path.startsWith('/products')
				},
				{
					href: '/operations/movements',
					label: 'Movimientos',
					icon: ArrowLeftRightIcon,
					current: (path) => path.startsWith('/operations/movements')
				}
			]
		},
		{
			label: 'Sistema',
			items: [
				{ href: '/users', label: 'Usuarios', icon: UsersIcon, ownerOnly: true }
			]
		}
	];

	const path = $derived(page.url.pathname);

	const visibleGroups = $derived(
		groups
			.map((group) => ({
				label: group.label,
				items: group.items.filter((item) => !item.ownerOnly || user.role === 'owner')
			}))
			.filter((group) => group.items.length > 0)
	);

	const isCurrent = (item: Item) => (item.current ? item.current(path) : path === item.href);

	const initials = $derived((user.email.split('@')[0] ?? user.email).slice(0, 2).toUpperCase());

	// Preferencia de la barra, no dato del servidor: se lee al montar y se
	// escribe en cada cambio. Sin `localStorage` (SSR) el rail nace compacto.
	const EXPANDED_KEY = 'organox:rail-expanded';
	let expanded = $state(false);

	$effect(() => {
		try {
			expanded = localStorage.getItem(EXPANDED_KEY) === '1';
		} catch {
			expanded = false;
		}
	});

	function toggleRail() {
		expanded = !expanded;
		try {
			localStorage.setItem(EXPANDED_KEY, expanded ? '1' : '0');
		} catch {
			// Modo privado o storage lleno: la preferencia es cosmética, no se
			// interrumpe la sesión por no poder recordarla.
		}
	}

	async function logout() {
		await api.logout().catch(() => {});
		window.location.href = '/login';
	}
</script>

<div class="shell" class:rail-expanded={expanded}>
	<aside class="rail" class:expanded aria-label="Navegación principal">
		<div class="rail-head">
			<div class="logo" aria-hidden="true">A</div>
			<div class="brand-text">
				<span class="brand">Almacén</span>
				<small>Administración</small>
			</div>
		</div>

		<button
			class="rail-toggle"
			type="button"
			aria-expanded={expanded}
			aria-controls="sidenav"
			aria-label={expanded ? 'Contraer menú' : 'Expandir menú'}
			onclick={toggleRail}
		>
			<ChevronLeftIcon class="chev" />
			<span class="label t-closed">Expandir</span>
			<span class="label t-open">Contraer</span>
		</button>

		<nav id="sidenav">
			{#each visibleGroups as group (group.label)}
				<div class="group"><span class="glabel">{group.label}</span></div>
				{#each group.items as item (item.href)}
					{@const Icon = item.icon}
					<a
						href={item.href}
						title={item.label}
						class:on={isCurrent(item)}
						aria-current={isCurrent(item) ? 'page' : undefined}
					>
						<Icon />
						<span class="label">{item.label}</span>
					</a>
				{/each}
			{/each}
		</nav>

		<div class="rail-foot">
			<DropdownMenu>
				<DropdownMenuTrigger>
					{#snippet child({ props })}
						<button {...props} class="user" title="Cuenta">
							<span class="avatar" aria-hidden="true">{initials}</span>
							<span class="user-text">
								<span class="u-email">{user.email}</span>
								<span class="u-role">{user.role}</span>
							</span>
							<ChevronUpIcon class="caret" />
						</button>
					{/snippet}
				</DropdownMenuTrigger>
				<DropdownMenuContent side="top" align="start" class="w-60">
					<DropdownMenuLabel class="flex flex-col gap-0.5 py-2">
						<span class="truncate text-xs font-normal" title={user.email}>{user.email}</span>
						<span class="text-[11px] font-normal uppercase tracking-wider text-muted-foreground">
							{user.role}
						</span>
					</DropdownMenuLabel>
					<DropdownMenuSeparator />
					<div class="py-1"><ThemeToggle class="justify-start" /></div>
					<DropdownMenuSeparator />
					<DropdownMenuItem variant="destructive" onSelect={logout}>
						<LogOutIcon />
						Salir
					</DropdownMenuItem>
				</DropdownMenuContent>
			</DropdownMenu>
		</div>
	</aside>

	<main class="main">
		{@render children?.()}
	</main>
</div>


<style>
	.shell {
		display: grid;
		grid-template-columns: 64px 1fr;
		min-height: 100vh;
	}

	.shell.rail-expanded {
		grid-template-columns: 236px 1fr;
	}

	/* ---------- rail ---------- */
	.rail {
		background: var(--sidebar);
		border-right: 1px solid var(--sidebar-border);
		color: var(--sidebar-foreground);
		display: flex;
		flex-direction: column;
		gap: 0.3rem;
		padding: 0.85rem 0.6rem;
		transition: width 160ms ease;
		min-width: 0;
	}

	.rail-head {
		display: flex;
		align-items: center;
		gap: 0.65rem;
		padding: 0 0.35rem 0.15rem;
	}

	.logo {
		flex: none;
		width: 34px;
		height: 34px;
		border-radius: 9px;
		background: var(--primary);
		color: var(--primary-foreground);
		display: grid;
		place-items: center;
		font-weight: 700;
	}

	.brand-text {
		display: flex;
		flex-direction: column;
		line-height: 1.25;
		overflow: hidden;
	}

	.brand-text .brand {
		font-size: 0.92rem;
		font-weight: 600;
		white-space: nowrap;
	}

	.brand-text small {
		font-size: 0.68rem;
		text-transform: uppercase;
		letter-spacing: 0.07em;
		color: var(--muted-foreground);
		white-space: nowrap;
	}

	.rail-toggle {
		display: flex;
		align-items: center;
		gap: 0.65rem;
		width: 100%;
		height: 32px;
		padding: 0 0.7rem;
		margin-bottom: 0.25rem;
		border: none;
		border-radius: 7px;
		background: transparent;
		color: var(--muted-foreground);
		font: inherit;
		font-size: 0.8rem;
		cursor: pointer;
	}

	.rail-toggle:hover {
		background: var(--sidebar-accent);
		color: var(--sidebar-accent-foreground);
	}

	.chev {
		width: 16px;
		height: 16px;
		flex: none;
		transition: transform 160ms ease;
	}

	.rail.expanded .chev {
		transform: rotate(180deg);
	}

	.rail-toggle .t-open {
		display: none;
	}

	.rail.expanded .rail-toggle .t-open {
		display: inline;
	}

	.rail.expanded .rail-toggle .t-closed {
		display: none;
	}

	nav {
		display: flex;
		flex-direction: column;
		gap: 0.15rem;
	}

	.group {
		padding: 0.75rem 0.6rem 0.3rem;
	}

	.glabel {
		font-size: 0.7rem;
		text-transform: uppercase;
		letter-spacing: 0.08em;
		color: var(--muted-foreground);
		white-space: nowrap;
	}

	nav a {
		display: flex;
		align-items: center;
		gap: 0.7rem;
		height: 40px;
		padding: 0 0.7rem;
		border-radius: 9px;
		color: var(--muted-foreground);
		text-decoration: none;
		font-size: 0.92rem;
	}

	nav a:hover {
		background: var(--sidebar-accent);
		color: var(--sidebar-accent-foreground);
	}

	nav a.on {
		background: color-mix(in srgb, var(--primary) 18%, transparent);
		color: var(--primary);
		font-weight: 600;
	}

	nav a :global(svg) {
		width: 19px;
		height: 19px;
		flex: none;
	}

	.rail-toggle:focus-visible,
	nav a:focus-visible,
	.user:focus-visible {
		outline: 2px solid var(--primary);
		outline-offset: 2px;
	}

	/* ---------- rail compacto: sólo iconos ---------- */
	.rail:not(.expanded) .rail-toggle,
	.rail:not(.expanded) nav a {
		justify-content: center;
		padding: 0;
	}

	.rail:not(.expanded) .group {
		display: block;
		height: 1px;
		margin: 0.5rem 0.7rem;
		padding: 0;
		background: var(--sidebar-border);
	}

	.rail:not(.expanded) .brand-text,
	.rail:not(.expanded) .label,
	.rail:not(.expanded) .glabel,
	.rail:not(.expanded) .user-text,
	.rail:not(.expanded) .caret {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip-path: inset(50%);
		white-space: nowrap;
	}

	/* ---------- pie: usuario ---------- */
	.rail-foot {
		margin-top: auto;
		padding-top: 0.6rem;
	}

	.user {
		display: flex;
		align-items: center;
		gap: 0.65rem;
		width: 100%;
		padding: 0.35rem;
		border: 1px solid transparent;
		border-radius: 9px;
		background: transparent;
		color: var(--sidebar-foreground);
		font: inherit;
		cursor: pointer;
		text-align: left;
	}

	.user:hover,
	.user[aria-expanded='true'] {
		background: var(--sidebar-accent);
		border-color: var(--sidebar-border);
	}

	.rail:not(.expanded) .user {
		justify-content: center;
		padding: 0.35rem 0;
	}

	.avatar {
		flex: none;
		width: 34px;
		height: 34px;
		border-radius: 50%;
		background: var(--sidebar-accent);
		color: var(--sidebar-accent-foreground);
		display: grid;
		place-items: center;
		font-size: 0.75rem;
		font-weight: 600;
		border: 1px solid var(--sidebar-border);
	}

	.user-text {
		display: flex;
		flex-direction: column;
		line-height: 1.2;
		overflow: hidden;
		min-width: 0;
	}

	.u-email {
		font-size: 0.78rem;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.u-role {
		font-size: 0.66rem;
		text-transform: uppercase;
		letter-spacing: 0.06em;
		color: var(--primary);
	}

	.caret {
		width: 15px;
		height: 15px;
		margin-left: auto;
		color: var(--muted-foreground);
		transition: transform 160ms ease;
	}

	:global(.user[aria-expanded='true'] .caret) {
		transform: rotate(180deg);
	}

	/* ---------- contenido ---------- */
	.main {
		min-width: 0;
		padding: 1.15rem 1.5rem 1.5rem;
	}

	/* Pantallas de pantalla completa (venta) manejan su propio alto. */
	:global(.main:has(> .pos)) {
		padding: 0;
		min-height: 100vh;
	}

	@media (max-width: 980px) {
		.shell {
			grid-template-columns: 56px 1fr;
		}
	}

	@media (prefers-reduced-motion: reduce) {
		.rail,
		.chev,
		.caret {
			transition: none;
		}
	}
</style>
