-- Purchase returns (supplier returns): immutable return documents linked to
-- completed purchases, wallet refund links, and supplier-account return entries.
--
-- Mirrors `0010_credit_notes.sql`, `0011_credit_note_refund_link.sql`,
-- `0012_credit_note_idempotency.sql`, and `0016_customer_account_sale_ceiling.sql`
-- for the supplier side, with one additional capability: any already-paid
-- surplus above the purchase's open payable can either be refunded to the
-- original wallets (`wallet_refund`, writing `purchase_refund` wallet inflows)
-- or kept as unapplied advance credit with the supplier (`supplier_credit`,
-- writing a `purchase_return_credit` ledger entry that future purchases consume
-- oldest-first and `POST /suppliers/{id}/credit-refunds` can refund later).

CREATE TYPE purchase_return_surplus_mode AS ENUM (
    'wallet_refund',
    'supplier_credit'
);

CREATE TABLE purchase_returns (
    id              uuid                         PRIMARY KEY,
    -- The completed purchase being returned. RESTRICT protects history.
    purchase_id     uuid                         NOT NULL REFERENCES purchases (id) ON DELETE RESTRICT,
    -- Server-computed total using frozen `purchase_items.unit_cost`.
    -- Zero is allowed because purchases accept zero-cost promotional lines.
    total           numeric(14, 2)               NOT NULL,
    -- Operator choice for how any already-paid surplus above open debt is handled.
    surplus_mode    purchase_return_surplus_mode NOT NULL,
    reason          text                         NOT NULL,
    idempotency_key text,
    -- No FK on users: return documents are immutable history.
    created_by      uuid                         NOT NULL,
    created_at      timestamptz                  NOT NULL DEFAULT now(),

    CONSTRAINT purchase_returns_total_not_negative CHECK (total >= 0),
    CONSTRAINT purchase_returns_reason_not_blank CHECK (btrim(reason) <> ''),
    CONSTRAINT purchase_returns_idempotency_key_not_blank CHECK (
        idempotency_key IS NULL OR btrim(idempotency_key) <> ''
    )
);

CREATE INDEX purchase_returns_purchase_idx ON purchase_returns (purchase_id);
CREATE INDEX purchase_returns_created_at_idx ON purchase_returns (created_at DESC, id DESC);
CREATE UNIQUE INDEX purchase_returns_purchase_idempotency_key_unique
    ON purchase_returns (purchase_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

CREATE TABLE purchase_return_items (
    id                 uuid           PRIMARY KEY,
    purchase_return_id uuid           NOT NULL REFERENCES purchase_returns (id) ON DELETE RESTRICT,
    product_id         uuid           NOT NULL REFERENCES products (id) ON DELETE RESTRICT,
    quantity           numeric(12, 3) NOT NULL,
    -- Frozen unit cost copied from `purchase_items` when the return is created.
    unit_cost          numeric(12, 2) NOT NULL,

    CONSTRAINT purchase_return_items_quantity_positive CHECK (quantity > 0),
    CONSTRAINT purchase_return_items_unit_cost_not_negative CHECK (unit_cost >= 0),
    CONSTRAINT purchase_return_items_unique_product UNIQUE (purchase_return_id, product_id)
);

CREATE INDEX purchase_return_items_return_idx ON purchase_return_items (purchase_return_id);
CREATE INDEX purchase_return_items_product_idx ON purchase_return_items (product_id);

-- Append-only enforcement on return documents and lines.
CREATE OR REPLACE FUNCTION purchase_returns_are_append_only() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION '% is append-only: record a new return instead of rewriting history', TG_TABLE_NAME;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER purchase_returns_no_update
    BEFORE UPDATE OR DELETE ON purchase_returns
    FOR EACH ROW
    EXECUTE FUNCTION purchase_returns_are_append_only();

CREATE TRIGGER purchase_return_items_no_update
    BEFORE UPDATE OR DELETE ON purchase_return_items
    FOR EACH ROW
    EXECUTE FUNCTION purchase_returns_are_append_only();

-- --- Wallet movements: link `purchase_refund` to its purchase and return ----

ALTER TABLE wallet_movements
    ADD COLUMN purchase_return_id uuid REFERENCES purchase_returns (id) ON DELETE RESTRICT;

CREATE INDEX wallet_movements_purchase_return_id_idx
    ON wallet_movements (purchase_return_id)
    WHERE purchase_return_id IS NOT NULL;

ALTER TABLE wallet_movements
    DROP CONSTRAINT wallet_movements_purchase_link_only_for_purchase_kinds;
ALTER TABLE wallet_movements
    DROP CONSTRAINT wallet_movements_purchase_payment_is_complete;

ALTER TABLE wallet_movements
    ADD CONSTRAINT wallet_movements_purchase_link_only_for_purchase_kinds CHECK (
        kind IN ('purchase_payment', 'purchase_refund') OR purchase_id IS NULL
    );

ALTER TABLE wallet_movements
    ADD CONSTRAINT wallet_movements_purchase_kind_is_complete CHECK (
        kind NOT IN ('purchase_payment', 'purchase_refund')
        OR (purchase_id IS NOT NULL AND payment_method_id IS NOT NULL)
    );

ALTER TABLE wallet_movements
    ADD CONSTRAINT wallet_movements_purchase_refund_names_its_return CHECK (
        kind <> 'purchase_refund' OR purchase_return_id IS NOT NULL
    );

ALTER TABLE wallet_movements
    ADD CONSTRAINT wallet_movements_purchase_return_only_for_purchase_refunds CHECK (
        kind = 'purchase_refund' OR purchase_return_id IS NULL
    );

-- --- Supplier account entries: `purchase_return` and `purchase_return_credit` ---

ALTER TABLE supplier_account_entries
    ADD COLUMN purchase_return_id uuid REFERENCES purchase_returns (id) ON DELETE RESTRICT;

CREATE INDEX supplier_account_entries_purchase_return_idx
    ON supplier_account_entries (purchase_return_id)
    WHERE purchase_return_id IS NOT NULL;

ALTER TABLE supplier_account_entries
    DROP CONSTRAINT supplier_account_entries_sign_matches_kind;
ALTER TABLE supplier_account_entries
    DROP CONSTRAINT supplier_account_entries_purchase_link;

ALTER TABLE supplier_account_entries
    ADD CONSTRAINT supplier_account_entries_sign_matches_kind CHECK (
        (kind IN ('purchase_charge', 'credit_refund', 'adjustment_in') AND amount > 0)
        OR (kind IN ('payment', 'purchase_return', 'purchase_return_credit', 'adjustment_out') AND amount < 0)
    );

ALTER TABLE supplier_account_entries
    ADD CONSTRAINT supplier_account_entries_purchase_link CHECK (
        (kind IN ('purchase_charge', 'purchase_return')) = (purchase_id IS NOT NULL)
    );

ALTER TABLE supplier_account_entries
    ADD CONSTRAINT supplier_account_entries_purchase_return_link CHECK (
        (kind IN ('purchase_return', 'purchase_return_credit')) = (purchase_return_id IS NOT NULL)
    );

-- At most one debt cancellation and at most one credit surplus per return.
CREATE UNIQUE INDEX supplier_account_entries_one_return_cancel_per_return
    ON supplier_account_entries (purchase_return_id)
    WHERE kind = 'purchase_return';

CREATE UNIQUE INDEX supplier_account_entries_one_return_credit_per_return
    ON supplier_account_entries (purchase_return_id)
    WHERE kind = 'purchase_return_credit';

-- Allow both `payment` and `purchase_return_credit` entries to be allocated to
-- completed purchases of the same supplier.
CREATE OR REPLACE FUNCTION supplier_account_allocations_are_payments() RETURNS trigger AS $$
DECLARE
    payment_supplier  uuid;
    payment_kind      supplier_account_entry_kind;
    purchase_supplier uuid;
    p_status          purchase_status;
BEGIN
    SELECT supplier_id, kind INTO payment_supplier, payment_kind
        FROM supplier_account_entries WHERE id = NEW.entry_id;

    IF payment_kind IS NULL OR payment_kind NOT IN ('payment', 'purchase_return_credit') THEN
        RAISE EXCEPTION 'only a payment or purchase return credit can be allocated to a purchase, not %', payment_kind;
    END IF;

    SELECT supplier_id, status INTO purchase_supplier, p_status
        FROM purchases WHERE id = NEW.purchase_id;

    IF p_status IS DISTINCT FROM 'completed' THEN
        RAISE EXCEPTION 'only a completed purchase can be paid from the account, not %', p_status;
    END IF;

    IF purchase_supplier IS DISTINCT FROM payment_supplier THEN
        RAISE EXCEPTION 'a purchase of another supplier cannot be paid from this payment';
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- Per-purchase allocation ceiling: total allocations against a purchase never
-- exceed `purchase_charge - purchase_return` (its net payable).
CREATE OR REPLACE FUNCTION supplier_account_allocations_fit_the_purchase() RETURNS trigger AS $$
DECLARE
    charged   numeric(14, 2);
    cancelled numeric(14, 2);
    allocated numeric(14, 2);
BEGIN
    SELECT COALESCE(SUM(amount), 0) INTO charged
        FROM supplier_account_entries
        WHERE purchase_id = NEW.purchase_id AND kind = 'purchase_charge';
    SELECT COALESCE(-SUM(amount), 0) INTO cancelled
        FROM supplier_account_entries
        WHERE purchase_id = NEW.purchase_id AND kind = 'purchase_return';
    SELECT COALESCE(SUM(amount), 0) INTO allocated
        FROM supplier_account_allocations
        WHERE purchase_id = NEW.purchase_id;

    IF allocated > charged - cancelled THEN
        RAISE EXCEPTION 'allocations (%) exceed the outstanding (%) of the purchase', allocated, charged - cancelled;
    END IF;

    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

-- Mirror guard on debt-cancelling `purchase_return` entries: total cancellations
-- plus allocations against a purchase never exceed its `purchase_charge`.
CREATE OR REPLACE FUNCTION supplier_account_return_fits_the_purchase() RETURNS trigger AS $$
DECLARE
    charged   numeric(14, 2);
    cancelled numeric(14, 2);
    allocated numeric(14, 2);
BEGIN
    SELECT COALESCE(SUM(amount), 0) INTO charged
        FROM supplier_account_entries
        WHERE purchase_id = NEW.purchase_id AND kind = 'purchase_charge';
    SELECT COALESCE(-SUM(amount), 0) INTO cancelled
        FROM supplier_account_entries
        WHERE purchase_id = NEW.purchase_id AND kind = 'purchase_return';
    SELECT COALESCE(SUM(amount), 0) INTO allocated
        FROM supplier_account_allocations
        WHERE purchase_id = NEW.purchase_id;

    IF cancelled + allocated > charged THEN
        RAISE EXCEPTION 'return cancellation (%) plus allocations (%) exceed the purchase charge (%)',
            cancelled, allocated, charged;
    END IF;

    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE CONSTRAINT TRIGGER supplier_account_return_fits_the_purchase
    AFTER INSERT ON supplier_account_entries
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    WHEN (NEW.kind = 'purchase_return')
    EXECUTE FUNCTION supplier_account_return_fits_the_purchase();
