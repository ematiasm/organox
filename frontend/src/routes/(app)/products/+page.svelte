<script lang="ts">
	import { goto } from '$app/navigation';
	import { Badge } from '$lib/components/ui/badge';
	import { Input } from '$lib/components/ui/input';
	import {
		Table,
		TableBody,
		TableCell,
		TableHead,
		TableHeader,
		TableRow
	} from '$lib/components/ui/table';

	let { data } = $props();

	// `$derived` en vez de `$state` con semilla: el input refleja la URL, y la
	// URL es la única fuente de verdad de la búsqueda. Evita el input y la URL
	// desincronizados tras el botón Atrás.
	let search = $derived(data.search);
	let timer: ReturnType<typeof setTimeout> | undefined;

	function onInput(event: Event) {
		const value = (event.target as HTMLInputElement).value;
		clearTimeout(timer);
		timer = setTimeout(() => {
			const query = value.trim() ? `?search=${encodeURIComponent(value.trim())}` : '';
			// `reset: false` preserva el foco en el input mientras se escribe;
			// `replace` evita llenar el historial con cada letra.
			goto(`/products${query}`, { reset: false, replace: true });
		}, 300);
	}

	/** Los decimales llegan como string ("1500.50") y así se muestran. */
	function money(value: string | null | undefined): string {
		return value ?? '—';
	}

	function stock(product: { stock?: string | null; kind: string }): string {
		// `null` en un servicio no es "cero": es "no aplica".
		if (product.kind === 'service') return '—';
		return product.stock ?? '0';
	}
</script>

<!-- Iniciar venta vive en el rail: la página no repite el acceso. -->
<h1 class="mb-4 text-xl font-semibold tracking-tight">Productos</h1>

<div class="mb-4 max-w-sm">
	<Input type="search" placeholder="Buscar por nombre…" value={search} oninput={onInput} />
</div>

{#if data.items.length === 0}
	<div class="rounded-lg border border-border p-10 text-center text-sm text-muted-foreground">
		{#if data.search}
			No hay productos que coincidan con «{data.search}».
		{:else}
			Todavía no hay productos cargados.
		{/if}
	</div>
{:else}
	<div class="overflow-hidden rounded-lg border border-border">
		<Table>
			<TableHeader>
				<TableRow>
					<TableHead>Nombre</TableHead>
					<TableHead>SKU</TableHead>
					<TableHead>Tipo</TableHead>
					<TableHead class="text-right">Precio</TableHead>
					<TableHead class="text-right">Stock</TableHead>
				</TableRow>
			</TableHeader>
			<TableBody>
				{#each data.items as product (product.id)}
					<TableRow>
						<TableCell class="font-medium">{product.name}</TableCell>
						<TableCell class="text-muted-foreground">{product.sku ?? '—'}</TableCell>
						<TableCell>
							{product.kind === 'service' ? 'Servicio' : 'Producto'}
						</TableCell>
						<TableCell class="text-right tabular-nums">{money(product.price)}</TableCell>
						<TableCell class="text-right tabular-nums">
							{stock(product)}
							{#if product.low_stock}
								<Badge variant="outline" class="ml-1.5 text-[10px] uppercase">bajo</Badge>
							{/if}
						</TableCell>
					</TableRow>
				{/each}
			</TableBody>
		</Table>
	</div>

	<p class="mt-3 text-sm text-muted-foreground">
		{data.items.length} {data.items.length === 1 ? 'producto' : 'productos'}
	</p>
{/if}
