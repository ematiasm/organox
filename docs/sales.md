# Ventas

## La decisión central: el carrito vive en el servidor

El carrito **no** vive en el navegador. Es una fila en `sales` con estado
`draft`, y sus ítems están en `sale_items`.

Por qué:

1. **Sobrevive un F5.** Recargar la pantalla de venta no vacía el carrito.
   `POST /sales/draft` devuelve el borrador abierto en vez de crear uno nuevo.
2. **Sobrevive un corte de red.** El cajero cobra cuando vuelve la conexión.
3. **El total lo calcula el servidor.** Un total que el navegador cree puede no
   ser el que se cobra; el que vale es el que devuelve la API.
4. **El stock se puede validar en cada acción**, no recién al confirmar.

Cada mutación devuelve la **venta completa recalculada**. El cliente no suma, no
valida y no decide: solo dibuja.

## Un borrador no toca el libro de stock

La venta solo escribe `stock_movements` **al cobrar**. Un borrador abandonado o
cancelado no deja rastro en el libro.

Esto es lo que hace que la tabla `sales` no necesite columna de stock, y por qué
`cancelled` solo aplica a borradores: no hay nada que revertir.

## El precio se congela al agregar el ítem

`sale_items.unit_price` copia el precio del producto en el momento de agregarlo.

Si el producto sube de precio mientras el cajero cobra, el total del carrito **no
se mueve**. En una venta completada, ese número es el precio cobrado, y así queda
auditado.

## Cobrar: una transacción, con el producto bloqueado

```
BEGIN
  SELECT status FROM sales WHERE id = ? FOR UPDATE   -- la venta
  por cada ítem:
    SELECT id FROM products WHERE id = ? FOR UPDATE  -- el producto
    SUM(movimientos) → stock disponible
    si no alcanza → ROLLBACK, 409
    INSERT stock_movements (reason='sale')
  UPDATE sales SET status='completed', total=?, completed_at=now()
COMMIT
```

Misma disciplina que `record_movement`. Con dos cajas vendiendo la última unidad,
una gana y la otra recibe `409` con el disponible exacto. Cubierto por
`two_drafts_competing_for_the_last_units`.

Si falta stock **no se escribe nada**: ni movimientos, ni estado, ni carrito
modificado. Cubierto por `insufficient_stock_leaves_no_state_change`.

## Un servicio no se vende como ítem

`service_cannot_be_sold_as_item`. Un servicio no tiene stock, así que no puede
generar un movimiento de salida. Si en el futuro un servicio debe facturarse, el
modelo cambia: sería un ítem que no descuenta stock, y eso necesita su propio
razonamiento sobre el libro.

## API

| Método | Ruta | Qué hace |
|---|---|---|
| POST | `/api/v1/sales/draft` | Abre borrador o devuelve el abierto (`reused: true`) |
| GET | `/api/v1/sales/{id}` | Venta con ítems y total |
| POST | `/api/v1/sales/{id}/items` | Agrega; si ya estaba, suma la cantidad |
| PUT | `/api/v1/sales/{id}/items/{product_id}` | Fija cantidad; `0` lo quita |
| DELETE | `/api/v1/sales/{id}/items/{product_id}` | Quita el ítem |
| POST | `/api/v1/sales/{id}/complete` | Cobra |
| POST | `/api/v1/sales/{id}/cancel` | Cancela el borrador |
| GET | `/api/v1/sales` | Últimas ventas cobradas |

## Por qué una venta cobrada no se puede corregir

`completed` no se edita ni se cancela. Corregir una venta cobrada es una **nota
de crédito**: una venta nueva en sentido inverso, con su propio movimiento de
stock. Permitir un `UPDATE` sobre una venta cerrada haría que el libro de stock y
la contabilidad dejen de coincidir sin rastro.

La nota de crédito no está implementada. Es lo próximo si hace falta devolver.

## Lo que falta

- **Nota de crédito / devolución**: no existe.
- **Medios de pago**: hoy la venta no registra cómo se pagó (efectivo, tarjeta,
  cuenta corriente). Es la falta más grande para operar de verdad.
- **Turno de caja / arqueo**: no hay apertura ni cierre de caja.
- **IVA**: la venta guarda el total; el impuesto no está desagregado.
- **Cliente asociado** a la venta.
- **Borradores huérfanos**: no hay limpieza de borradores viejos.
