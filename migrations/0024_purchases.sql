-- Purchases with persisted drafts and cash wallet outflows.
--
-- Mirrors `sales`: a draft persists across reloads and edits items and supplier
-- freely, then completion runs in one transaction to freeze `supplier_name`
-- and `total`, write `stock_movements` (`reason = 'purchase'`, with
-- `unit_cost`), and record `wallet_movements` (`kind = 'purchase_payment'`).
-- Completed and cancelled purchases are immutable facts.

CREATE TYPE purchase_status AS ENUM ('draft', 'completed', 'cancelled');

CREATE TABLE purchases (
    id            uuid            PRIMARY KEY,
    status        purchase_status NOT NULL DEFAULT 'draft',
    -- Optional while `draft` so a draft can be opened immediately; required
    -- before completion. ON DELETE RESTRICT protects history.
    supplier_id   uuid            REFERENCES suppliers (id) ON DELETE RESTRICT,
    -- Snapshot of the supplier's name written in the same statement that closes
    -- the purchase, so renaming a supplier later never rewrites past purchases.
    supplier_name text,
    -- Frozen at completion; recalculated while `draft`.
    total         numeric(14, 2),
    -- No FK on users, matching `sales.created_by` and `stock_movements`.
    created_by    uuid            NOT NULL,
    completed_by  uuid,
    note          text,
    created_at    timestamptz     NOT NULL DEFAULT now(),
    updated_at    timestamptz     NOT NULL DEFAULT now(),
    completed_at  timestamptz,

    CONSTRAINT purchases_total_not_negative CHECK (total IS NULL OR total >= 0),
    CONSTRAINT purchases_completed_has_total CHECK (
        status <> 'completed' OR total IS NOT NULL
    ),
    CONSTRAINT purchases_completed_has_timestamp CHECK (
        status <> 'completed' OR completed_at IS NOT NULL
    ),
    CONSTRAINT purchases_supplier_name_needs_its_supplier CHECK (
        supplier_name IS NULL OR supplier_id IS NOT NULL
    ),
    CONSTRAINT purchases_completed_has_supplier CHECK (
        status <> 'completed' OR (supplier_id IS NOT NULL AND supplier_name IS NOT NULL)
    )
);

CREATE INDEX purchases_status_idx ON purchases (status);
CREATE INDEX purchases_created_by_idx ON purchases (created_by);
CREATE INDEX purchases_supplier_id_idx ON purchases (supplier_id)
    WHERE supplier_id IS NOT NULL;
CREATE INDEX purchases_created_at_idx ON purchases (created_at DESC, id DESC);

CREATE TABLE purchase_items (
    id          uuid           PRIMARY KEY,
    purchase_id uuid           NOT NULL REFERENCES purchases (id) ON DELETE CASCADE,
    product_id  uuid           NOT NULL REFERENCES products (id) ON DELETE RESTRICT,
    quantity    numeric(12, 3) NOT NULL,
    unit_cost   numeric(12, 2) NOT NULL,
    created_at  timestamptz    NOT NULL DEFAULT now(),

    CONSTRAINT purchase_items_quantity_positive CHECK (quantity > 0),
    CONSTRAINT purchase_items_unit_cost_not_negative CHECK (unit_cost >= 0),
    CONSTRAINT purchase_items_unique_product UNIQUE (purchase_id, product_id)
);

CREATE INDEX purchase_items_purchase_id_idx ON purchase_items (purchase_id);
CREATE INDEX purchase_items_product_id_idx ON purchase_items (product_id);

CREATE TRIGGER purchases_set_updated_at
    BEFORE UPDATE ON purchases
    FOR EACH ROW
    EXECUTE FUNCTION set_updated_at();

-- Link purchase payments on the append-only wallet ledger.
ALTER TABLE wallet_movements
    ADD COLUMN purchase_id uuid REFERENCES purchases (id) ON DELETE RESTRICT;

CREATE INDEX wallet_movements_purchase_id_idx ON wallet_movements (purchase_id)
    WHERE purchase_id IS NOT NULL;

ALTER TABLE wallet_movements
    ADD CONSTRAINT wallet_movements_purchase_link_only_for_purchase_kinds CHECK (
        kind = 'purchase_payment' OR purchase_id IS NULL
    );

ALTER TABLE wallet_movements
    ADD CONSTRAINT wallet_movements_purchase_payment_is_complete CHECK (
        kind <> 'purchase_payment'
        OR (purchase_id IS NOT NULL AND payment_method_id IS NOT NULL)
    );
