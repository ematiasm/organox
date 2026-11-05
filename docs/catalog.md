# Catálogo y stock

## La decisión central: el stock no se guarda, se calcula

`products` **no tiene** columna de stock. El stock es la suma firmada de
`stock_movements`.

```sql
SELECT SUM(CASE reason
    WHEN 'purchase'   THEN quantity
    WHEN 'adjustment' THEN quantity
    WHEN 'sale'       THEN -quantity
    WHEN 'loss'       THEN -quantity
END)
FROM stock_movements WHERE product_id = $1;
```

Guardar `products.stock_actual` como número editable garantiza que en algún
momento la DB y el depósito real no coincidan, y que nadie pueda explicar por
qué. Con un libro append-only, el stock se reconstruye y se audita siempre, y
cada unidad tiene un motivo y un autor.

El costo asumido: "stock actual" cuesta un `GROUP BY` en vez de un `SELECT`. Con
el índice `stock_movements (product_id, created_at DESC)` y decenas de miles de
filas, es despreciable.

## El libro es inmutable, y lo impone la DB

`stock_movements` rechaza `UPDATE` y `DELETE` con un trigger:

```
ERROR: stock_movements es append-only: registrá un ajuste en vez de modificar el historial
```

Un error de carga se corrige con un movimiento de `adjustment`, que es
exactamente lo que hace un depósito real: deja rastro de la corrección.

Corolario no obvio: `created_by` **no tiene foreign key**. El libro es inmutable,
así que un `ON DELETE SET NULL` tendría que tocar filas históricas cuando se
borra un usuario, y el trigger lo rechazaría. Además, la autoría de un
movimiento no debe cambiar porque después se borre la cuenta. Es una referencia
blanda, y por eso es correcta.

## Signo: la cantidad siempre es positiva

`quantity` tiene `CHECK (quantity > 0)`. El signo lo decide el motivo, en un solo
lugar (`MovementReason::sign`):

| Motivo | Efecto | Para qué |
|---|---|---|
| `purchase` | `+` | Compra a proveedor |
| `adjustment` | `+` | Corrección de inventario |
| `sale` | `−` | Venta a cliente |
| `loss` | `−` | Rotura, vencimiento, robo |

Guardar cantidades negativas permite "una venta de -5" que suma stock: un error
de signo esperando a ocurrir. Con la cantidad positiva y el signo derivado, ese
bug es representable solo si alguien cambia el motivo.

## No se puede vender lo que no hay

Antes de insertar una salida, el servicio lee el stock en la misma transacción y
rechaza si no alcanza. El mensaje de error lleva el disponible y el solicitado.

```json
{"title":"Conflict","status":409,
 "detail":"stock insuficiente: disponible 2.000, solicitado 3.000"}
```

Para evitar dos ventas simultáneas de la última unidad, la fila del producto se
bloquea con `SELECT ... FOR UPDATE` antes de leer el stock.

**Limitación conocida y documentada:** el test `record_movement_waits_for_product_lock`
prueba que la operación se serializa por producto, pero **no aísla** el
`FOR UPDATE` del servicio. El `INSERT` en `stock_movements` hace que la foreign
key adquiera `FOR KEY SHARE` sobre la fila de `products`, y ese lock también
choca con el `FOR UPDATE` del bloqueador del test. Se comprobó quitando el
`FOR UPDATE`: el test seguía pasando. La corrección del lock propio no está
cubierta por un test automático; lo que sí está cubierto es la regla de negocio
(`sale_cannot_make_stock_negative`) y el `CHECK` de la DB.

## Productos vs servicios

Un solo catálogo, dos tipos:

| | `product` | `service` |
|---|---|---|
| Stock | calculado | `null` (**no** `0`) |
| `min_stock` | permitido | prohibido (`CHECK` en DB) |
| Movimientos | permitidos | rechazados por el servicio |

`stock: null` vs `stock: "0"` no es un detalle de estilo: "no aplica" y "cero
unidades" son cosas distintas, y el cliente necesita distinguirlas para no
mostrar "sin stock" en un flete.

Un servicio tampoco puede recibir movimientos: lo verifica el servicio y lo
respalda el hecho de que `kind` se lee dentro de la transacción.

## Desactivar, nunca borrar

Un producto con movimientos no se borra (`ON DELETE RESTRICT`). Se desactiva:
`is_active = false`, desaparece de los listados por defecto, y su historial
queda intacto. Un producto sin historial sí podría borrarse, pero la operación
expuesta es solo `deactivate` — menos superficie, y un borrado accidental de
catálogo no se puede deshacer por definición.

## Dinero y cantidades en JSON: strings, no números

Todo decimal viaja como **string**:

```json
{"price": "1500.50", "stock": "12.5", "quantity": "12.5"}
```

Un `f64` en JSON pierde precisión en dinero y en cantidades fraccionarias. El
cliente parsea a decimal (`decimal.js`, `Decimal` en Go, `BigDecimal` en Java).

Formato por tipo:
- **dinero** (`price`, `unit_cost`): siempre 2 decimales → `"1500.50"`
- **cantidades** (`stock`, `quantity`, `min_stock`): sin ceros sobrantes → `"12.5"`

La columna es `numeric(12,3)`, así que el `SUM` devuelve `"12.500"` sin
normalizar; aplicarlo en el borde evita filtrar un detalle de almacenamiento al
contrato HTTP.

## API

Todas las rutas requieren sesión (401 sin cookie). Escribir catálogo es trabajo
operativo: `owner` y `encargado` pueden.

| Método | Ruta | Descripción |
|---|---|---|
| GET | `/api/v1/products` | Lista con stock calculado. Filtros: `kind`, `search`, `include_inactive`, `limit`, `offset` |
| POST | `/api/v1/products` | Alta |
| GET | `/api/v1/products/{id}` | Detalle con stock |
| PATCH | `/api/v1/products/{id}` | Actualiza solo los campos enviados |
| POST | `/api/v1/products/{id}/deactivate` | Baja lógica, idempotente |
| GET | `/api/v1/products/{id}/movements` | Historial, más nuevo primero |
| POST | `/api/v1/products/{id}/movements` | Registra movimiento |

`limit` se acota a 200: un `limit` de 10 millones es un DoS por consulta.

Ejemplo de alta y carga de stock:

```bash
curl -b jar.txt -X POST localhost:3000/api/v1/products \
  -H 'content-type: application/json' \
  -d '{"kind":"product","name":"Harina 000","sku":"HAR-001","price":"1500.50","min_stock":"10"}'

curl -b jar.txt -X POST localhost:3000/api/v1/products/<id>/movements \
  -H 'content-type: application/json' \
  -d '{"reason":"purchase","quantity":"50","unit_cost":"1200"}'
```

## Reglas de validación

| Campo | Regla | Dónde |
|---|---|---|
| `name` | no vacío, ≤ 200 chars, se recorta | servicio |
| `sku` | ≤ 64 chars, único cuando existe, `""` → `NULL` | servicio + índice parcial |
| `price` | ≥ 0, ≤ 2 decimales | servicio |
| `quantity` | > 0, ≤ 3 decimales | servicio + `CHECK` |
| `note` | truncada a 500 chars (no se rechaza) | servicio |

El SKU vacío se normaliza a `NULL` para que `""` y `NULL` no sean dos formas de
decir "sin SKU": el índice único es parcial (`WHERE sku IS NOT NULL`), y ambos
valores se comportarían distinto.

## Lo que falta

- **Edición del motivo de un movimiento**: no existe, por diseño. Se corrige con
  un ajuste.
- **Reporte de valorización de inventario**: hay `unit_cost` por movimiento, así
  que el dato está; falta la consulta y la decisión de qué costo usar (último,
  promedio ponderado, FIFO).
- **Alertas de stock bajo**: `low_stock` se calcula y se expone, pero no hay
  notificación.
- **Importación masiva** de catálogo.
