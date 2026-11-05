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
	import { cn } from '$lib/utils';
	import type { MovementReason } from '$lib/api/client';
	import type { MovementRow } from './+page';

	let { data } = $props();

	/** La API manda el motivo como enum en inglés; la pantalla habla español. */
	const REASONS: Record<MovementReason, string> = {
		purchase: 'Compra',
		sale: 'Venta',
		adjustment: 'Ajuste',
		loss: 'Pérdida'
	};

	/**
	 * El signo lo calcula el servidor (`signed_quantity`): replicar acá la regla
	 * de signos por motivo es justo lo que ese campo evita.
	 */
	function isOut(movement: MovementRow): boolean {
		return movement.signed_quantity.trim().startsWith('-');
	}

	function when(iso: string): string {
		const date = new Date(iso);
		return Number.isNaN(date.getTime())
			? iso
			: date.toLocaleString('es-AR', { dateStyle: 'short', timeStyle: 'short' });
	}

	const shown = $derived(data.movements.length);
</script>

<div class="mb-4">
	<h1 class="text-xl font-semibold tracking-tight">Movimientos</h1>
	<p class="mt-1 text-sm text-muted-foreground">
		Los {data.productCount} productos con actividad más reciente, de {data.totalProducts} en el
		catálogo.
	</p>
</div>

{#if shown === 0}
	<div class="rounded-lg border border-border p-10 text-center text-sm text-muted-foreground">
		Todavía no hay movimientos registrados. Se generan al cargar stock y al cobrar una venta.
	</div>
{:else}
	<div class="overflow-hidden rounded-lg border border-border">
		<Table>
			<TableHeader>
				<TableRow>
					<TableHead>Fecha</TableHead>
					<TableHead>Producto</TableHead>
					<TableHead>SKU</TableHead>
					<TableHead>Motivo</TableHead>
					<TableHead class="text-right">Cantidad</TableHead>
					<TableHead>Nota</TableHead>
				</TableRow>
			</TableHeader>
			<TableBody>
				{#each data.movements as movement (movement.id)}
					<TableRow>
						<TableCell class="text-muted-foreground">{when(movement.created_at)}</TableCell>
						<TableCell>{movement.product_name}</TableCell>
						<TableCell class="font-mono text-xs text-muted-foreground">
							{movement.product_sku ?? '—'}
						</TableCell>
						<TableCell><Badge variant="outline">{REASONS[movement.reason]}</Badge></TableCell>
						<TableCell
							class={cn('text-right tabular-nums', isOut(movement) && 'text-destructive')}
						>
							{movement.signed_quantity}
						</TableCell>
						<TableCell class="text-muted-foreground">{movement.note ?? '—'}</TableCell>
					</TableRow>
				{/each}
			</TableBody>
		</Table>
	</div>
	<p class="mt-3 text-sm text-muted-foreground">
		Mostrando {shown}
		{shown === 1 ? 'movimiento' : 'movimientos'} · página 1
	</p>
{/if}
