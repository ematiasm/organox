-- Rebanada 3: catálogo de productos y servicios, y el libro de stock.
--
-- Decisión central de esta migración: el stock NO es una columna.
-- Es la suma de los movimientos. Guardar `products.stock_actual` como número
-- editable garantiza que en algún momento la DB y el depósito no coincidan, y
-- que nadie pueda explicar por qué. Con un libro append-only, el stock siempre
-- se puede reconstruir y auditar.
--
-- Consecuencia: `stock_movements` es de solo inserción. No hay UPDATE ni
-- DELETE. Un error se corrige con un movimiento de ajuste, que es exactamente
-- lo que hace un depósito real.

CREATE TYPE product_kind AS ENUM ('product', 'service');

-- Motivo de un movimiento. Determina el signo permitido: no se puede "vender"
-- stock negativo ni "comprar" en negativo. El sí/ no es decorativo, es la
-- restricción de integridad que evita que el libro mienta.
CREATE TYPE movement_reason AS ENUM (
    'purchase',  -- entrada: compra a proveedor
    'sale',      -- salida: venta a cliente
    'adjustment',-- cualquiera: corrección de inventario
    'loss'       -- salida: rotura, vencimiento, robo
);

CREATE TABLE products (
    id          uuid          PRIMARY KEY,
    kind        product_kind  NOT NULL,
    sku         text,
    name        text          NOT NULL,
    description text,
    -- Dinero en NUMERIC, nunca en float. `price` puede ser NULL: un servicio
    -- puede no tener precio fijo (se cotiza por trabajo).
    price       numeric(12, 2),
    -- Stock mínimo solo tiene sentido para productos. Para servicios queda NULL.
    min_stock   numeric(12, 3),
    is_active   boolean       NOT NULL DEFAULT true,
    created_at  timestamptz   NOT NULL DEFAULT now(),
    updated_at  timestamptz   NOT NULL DEFAULT now(),

    CONSTRAINT products_name_not_blank CHECK (btrim(name) <> ''),
    CONSTRAINT products_sku_not_blank CHECK (sku IS NULL OR btrim(sku) <> ''),
    CONSTRAINT products_price_not_negative CHECK (price IS NULL OR price >= 0),
    CONSTRAINT products_min_stock_not_negative CHECK (min_stock IS NULL OR min_stock >= 0),
    -- Un servicio no tiene stock mínimo ni lleva movimientos. Se refuerza en la
    -- DB, no solo en el código: es una invariante del dominio.
    CONSTRAINT products_service_has_no_min_stock CHECK (
        kind <> 'service' OR min_stock IS NULL
    )
);

-- El SKU es opcional pero único cuando existe. Índice parcial: permite N filas
-- con sku NULL sin que colisionen entre sí.
CREATE UNIQUE INDEX products_sku_unique ON products (sku) WHERE sku IS NOT NULL;
CREATE INDEX products_kind_active_idx ON products (kind, is_active);
-- Búsqueda por nombre. `lower()` para que "Harina" encuentre "harina".
CREATE INDEX products_name_lower_idx ON products (lower(name));

CREATE TABLE stock_movements (
    id           uuid            PRIMARY KEY,
    product_id   uuid            NOT NULL REFERENCES products (id) ON DELETE RESTRICT,
    reason       movement_reason NOT NULL,
    -- Siempre positivo: el signo lo determina el motivo, no el dato. Guardar
    -- cantidades negativas permite "una venta de -5 unidades" que suma stock,
    -- que es un error de signo esperando a ocurrir.
    quantity     numeric(12, 3)  NOT NULL,
    -- Costo unitario al momento del movimiento. Se congela acá porque el precio
    -- de compra cambia y el costo histórico no debe cambiar con él.
    unit_cost    numeric(12, 2),
    note         text,
    -- Quién lo registró. NULL permite movimientos del sistema (importación
    -- inicial).
    --
    -- SIN foreign key, a propósito: el libro es inmutable, así que un
    -- `ON DELETE SET NULL` tendría que MODIFICAR filas históricas cuando se
    -- borra un usuario, y el trigger de append-only lo rechaza. Además, la
    -- autoría de un movimiento no debe cambiar porque después se borre la
    -- cuenta: el historial registra quién lo hizo, no si esa cuenta sigue
    -- existiendo. Es una referencia blanda y por eso es correcta.
    created_by   uuid,
    created_at   timestamptz     NOT NULL DEFAULT now(),

    CONSTRAINT stock_movements_quantity_positive CHECK (quantity > 0),
    CONSTRAINT stock_movements_unit_cost_not_negative CHECK (unit_cost IS NULL OR unit_cost >= 0)
);

-- Consultas de stock: SUM(quantity firmada) por producto. Este índice es el que
-- hace que el cálculo sea barato.
CREATE INDEX stock_movements_product_idx ON stock_movements (product_id, created_at DESC);
CREATE INDEX stock_movements_created_at_idx ON stock_movements (created_at DESC);

-- El libro es append-only. Se bloquea a nivel de motor, no por convención: un
-- `UPDATE` accidental falla en vez de corromper el historial.
CREATE OR REPLACE FUNCTION stock_movements_are_append_only() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'stock_movements es append-only: registrá un ajuste en vez de modificar el historial';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER stock_movements_no_update
    BEFORE UPDATE OR DELETE ON stock_movements
    FOR EACH ROW
    EXECUTE FUNCTION stock_movements_are_append_only();

CREATE TRIGGER products_set_updated_at
    BEFORE UPDATE ON products
    FOR EACH ROW
    EXECUTE FUNCTION set_updated_at();
