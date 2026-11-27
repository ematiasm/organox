-- Database-layer immutability, foreign-key restrictions, and ledger ceiling
-- backstops (#112).
--
-- 1. `credit_notes` and `credit_note_items` become append-only (matching
--    `purchase_returns` in `0031`), and `credit_note_items` switches from
--    `ON DELETE CASCADE` to `ON DELETE RESTRICT`.
-- 2. Closed `purchases` (`completed` or `cancelled`) and their `purchase_items`
--    become immutable (matching `sales` in `0032`), locking the parent purchase
--    `FOR SHARE` so concurrent item writes serialize against close/cancel.
-- 3. `purchase_items` and `sale_items` switch from `ON DELETE CASCADE` to
--    `ON DELETE RESTRICT` so child lines can never be cascade-deleted even if a
--    trigger is temporarily disabled (application code never deletes sales or
--    purchases; drafts are soft-cancelled).
-- 4. `customer_account_entries` gains the per-sale `credit_note` ceiling trigger
--    matching `supplier_account_return_fits_the_purchase` (`0031`).
--
-- Note on `unapplied_credit >= 0`: `0033_credit_refund_allocations.sql` already
-- enforces the strictly stronger per-receipt / per-source-entry ceiling
-- (`allocated + refunded_attributed <= source_amount`) together with mandatory
-- full refund attribution on both customer and supplier ledgers, which
-- mathematically guarantees `unapplied_credit >= 0`.

-- ============================================================================
-- 1. Credit notes: append-only triggers + ON DELETE RESTRICT
-- ============================================================================

CREATE OR REPLACE FUNCTION credit_notes_are_append_only() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION '% is append-only: record a new credit note instead of rewriting history',
        TG_TABLE_NAME;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER credit_notes_no_update
    BEFORE UPDATE OR DELETE ON credit_notes
    FOR EACH ROW
    EXECUTE FUNCTION credit_notes_are_append_only();

CREATE TRIGGER credit_note_items_no_update
    BEFORE UPDATE OR DELETE ON credit_note_items
    FOR EACH ROW
    EXECUTE FUNCTION credit_notes_are_append_only();

ALTER TABLE credit_note_items
    DROP CONSTRAINT credit_note_items_credit_note_id_fkey,
    ADD CONSTRAINT credit_note_items_credit_note_id_fkey
        FOREIGN KEY (credit_note_id) REFERENCES credit_notes (id) ON DELETE RESTRICT;

-- ============================================================================
-- 2. Purchases: closed-row immutability triggers + ON DELETE RESTRICT
-- ============================================================================

CREATE OR REPLACE FUNCTION purchases_closed_are_immutable() RETURNS trigger AS $$
BEGIN
    IF OLD.status <> 'draft' THEN
        RAISE EXCEPTION 'closed purchases are immutable: cannot % purchase % with status %',
            TG_OP, OLD.id, OLD.status;
    END IF;

    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER purchases_prevent_closed_mutation
    BEFORE UPDATE OR DELETE ON purchases
    FOR EACH ROW
    EXECUTE FUNCTION purchases_closed_are_immutable();

-- Lock the parent `purchases` row `FOR SHARE` so even a direct `purchase_items`
-- write racing with completion (`FOR UPDATE`) or cancellation (`FOR NO KEY UPDATE`)
-- serializes against the closing transaction and sees the committed status.
CREATE OR REPLACE FUNCTION purchase_items_closed_purchase_are_immutable() RETURNS trigger AS $$
DECLARE
    v_status purchase_status;
BEGIN
    IF TG_OP IN ('UPDATE', 'DELETE') THEN
        SELECT status INTO v_status
        FROM purchases
        WHERE id = OLD.purchase_id
        FOR SHARE;

        IF FOUND AND v_status <> 'draft' THEN
            RAISE EXCEPTION 'purchase_items of closed purchase % (status: %) are immutable: cannot %',
                OLD.purchase_id, v_status, TG_OP;
        END IF;
    END IF;

    IF TG_OP = 'INSERT' OR (TG_OP = 'UPDATE' AND NEW.purchase_id IS DISTINCT FROM OLD.purchase_id) THEN
        SELECT status INTO v_status
        FROM purchases
        WHERE id = NEW.purchase_id
        FOR SHARE;

        IF FOUND AND v_status <> 'draft' THEN
            RAISE EXCEPTION 'purchase_items of closed purchase % (status: %) are immutable: cannot %',
                NEW.purchase_id, v_status, TG_OP;
        END IF;
    END IF;

    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER purchase_items_prevent_closed_mutation
    BEFORE INSERT OR UPDATE OR DELETE ON purchase_items
    FOR EACH ROW
    EXECUTE FUNCTION purchase_items_closed_purchase_are_immutable();

ALTER TABLE purchase_items
    DROP CONSTRAINT purchase_items_purchase_id_fkey,
    ADD CONSTRAINT purchase_items_purchase_id_fkey
        FOREIGN KEY (purchase_id) REFERENCES purchases (id) ON DELETE RESTRICT;

ALTER TABLE sale_items
    DROP CONSTRAINT sale_items_sale_id_fkey,
    ADD CONSTRAINT sale_items_sale_id_fkey
        FOREIGN KEY (sale_id) REFERENCES sales (id) ON DELETE RESTRICT;

-- ============================================================================
-- 3. Customer accounts: per-sale credit-note ceiling
-- ============================================================================

-- Mirror guard on debt-cancelling `credit_note` entries: total cancellations
-- plus allocations against a sale never exceed its `sale_charge`.
CREATE OR REPLACE FUNCTION customer_account_credit_note_fits_the_sale() RETURNS trigger AS $$
DECLARE
    charged   numeric(14, 2);
    cancelled numeric(14, 2);
    allocated numeric(14, 2);
BEGIN
    SELECT COALESCE(SUM(amount), 0) INTO charged
        FROM customer_account_entries
        WHERE sale_id = NEW.sale_id AND kind = 'sale_charge';
    SELECT COALESCE(-SUM(amount), 0) INTO cancelled
        FROM customer_account_entries
        WHERE sale_id = NEW.sale_id AND kind = 'credit_note';
    SELECT COALESCE(SUM(amount), 0) INTO allocated
        FROM customer_account_allocations
        WHERE sale_id = NEW.sale_id;

    IF cancelled + allocated > charged THEN
        RAISE EXCEPTION 'credit note cancellation (%) plus allocations (%) exceed the sale charge (%)',
            cancelled, allocated, charged;
    END IF;

    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE CONSTRAINT TRIGGER customer_account_credit_note_fits_the_sale
    AFTER INSERT ON customer_account_entries
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    WHEN (NEW.kind = 'credit_note')
    EXECUTE FUNCTION customer_account_credit_note_fits_the_sale();
