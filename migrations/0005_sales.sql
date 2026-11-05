-- Rebanada 6: ventas con borrador persistido.
--
-- Por qué un borrador en la base y no un carrito en el navegador:
--
--   * sobrevive un F5 y un corte de red;
--   * el stock se valida contra la DB en cada ítem, no en el cliente;
--   * dos cajeros no pueden vender la misma unidad (el borrador reserva);
--   * el total lo calcula el servidor, que es el único que puede confiar en sí
--     mismo para cobrar.
--
-- El estado `draft` es una venta que todavía no descontó stock. Al confirmar
-- pasa a `completed` y recién ahí se escriben los `stock_movements`. Un borrador
-- abandonado no deja rastro en el libro de stock, que es lo que se quiere.

CREATE TYPE sale_status AS ENUM ('draft', 'completed', 'cancelled');

CREATE TABLE sales (
    id           uuid          PRIMARY KEY,
    status       sale_status   NOT NULL DEFAULT 'draft',
    -- Total congelado al confirmar. Mientras es borrador se recalcula; al
    -- completar queda fijo, porque el precio puede cambiar después y una venta
    -- cerrada no debe moverse.
    total        numeric(14, 2),
    -- Quién abrió el borrador. SIN foreign key por la misma razón que
    -- stock_movements.created_by: el registro de una venta no debe cambiar
    -- porque después se borre la cuenta.
    created_by   uuid          NOT NULL,
    -- Quién cobró. Puede diferir del que abrió el borrador (el dueño cierra la
    -- caja de un turno del encargado).
    completed_by uuid,
    note         text,
    created_at   timestamptz   NOT NULL DEFAULT now(),
    updated_at   timestamptz   NOT NULL DEFAULT now(),
    completed_at timestamptz,

    CONSTRAINT sales_total_not_negative CHECK (total IS NULL OR total >= 0),
    -- Un borrador no tiene total; una venta completada sí. Sin esto, una venta
    -- cerrada sin total es un agujero contable.
    CONSTRAINT sales_completed_has_total CHECK (
        status <> 'completed' OR total IS NOT NULL
    ),
    CONSTRAINT sales_completed_has_timestamp CHECK (
        status <> 'completed' OR completed_at IS NOT NULL
    )
);

CREATE INDEX sales_status_idx ON sales (status);
CREATE INDEX sales_created_by_idx ON sales (created_by);
CREATE INDEX sales_created_at_idx ON sales (created_at DESC);

CREATE TABLE sale_items (
    id         uuid          PRIMARY KEY,
    sale_id    uuid          NOT NULL REFERENCES sales (id) ON DELETE CASCADE,
    product_id uuid          NOT NULL REFERENCES products (id) ON DELETE RESTRICT,
    -- Cantidad positiva. El signo lo pone el movimiento de stock al confirmar.
    quantity   numeric(12, 3) NOT NULL,
    -- Precio unitario copiado al agregar el ítem. Congelado a propósito: si el
    -- producto cambia de precio mientras el cajero cobra, el total del carrito
    -- no debe saltar. En una venta completada esto es el precio cobrado.
    unit_price numeric(12, 2) NOT NULL,
    created_at timestamptz   NOT NULL DEFAULT now(),

    CONSTRAINT sale_items_quantity_positive CHECK (quantity > 0),
    CONSTRAINT sale_items_unit_price_not_negative CHECK (unit_price >= 0),
    -- El mismo producto no puede estar dos veces en la misma venta: se suma la
    -- cantidad. Evita dos filas que descuentan stock por separado y confunden
    -- el reporte.
    CONSTRAINT sale_items_unique_product UNIQUE (sale_id, product_id)
);

CREATE INDEX sale_items_sale_id_idx ON sale_items (sale_id);
CREATE INDEX sale_items_product_id_idx ON sale_items (product_id);

CREATE TRIGGER sales_set_updated_at
    BEFORE UPDATE ON sales
    FOR EACH ROW
    EXECUTE FUNCTION set_updated_at();
