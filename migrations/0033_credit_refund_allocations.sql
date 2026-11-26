-- Attribute each `credit_refund` entry to the specific credit-bearing receipt(s)
-- or supplier payment/return-credit entry(ies) whose unapplied credit it
-- returned, so refunded entries can no longer back future allocations or
-- misroute subsequent credit-note / purchase-return refunds to the wrong wallet.

-- ============================================================================
-- Customer accounts: credit refund attribution
-- ============================================================================

CREATE TABLE customer_account_credit_refund_allocations (
    id               uuid           PRIMARY KEY,
    refund_entry_id  uuid           NOT NULL REFERENCES customer_account_entries (id) ON DELETE RESTRICT,
    receipt_entry_id uuid           NOT NULL REFERENCES customer_account_entries (id) ON DELETE RESTRICT,
    amount           numeric(14, 2) NOT NULL,
    created_at       timestamptz    NOT NULL DEFAULT now(),

    CONSTRAINT customer_account_credit_refund_allocations_amount_positive
        CHECK (amount > 0),
    CONSTRAINT customer_account_credit_refund_allocations_once_per_receipt
        UNIQUE (refund_entry_id, receipt_entry_id)
);

CREATE INDEX customer_account_credit_refund_allocations_receipt_idx
    ON customer_account_credit_refund_allocations (receipt_entry_id);

CREATE TRIGGER customer_account_credit_refund_allocations_no_update
    BEFORE UPDATE OR DELETE ON customer_account_credit_refund_allocations
    FOR EACH ROW
    EXECUTE FUNCTION customer_accounts_are_append_only();

CREATE OR REPLACE FUNCTION customer_account_credit_refund_allocations_are_valid() RETURNS trigger AS $$
DECLARE
    refund_customer  uuid;
    refund_kind      customer_account_entry_kind;
    receipt_customer uuid;
    receipt_kind     customer_account_entry_kind;
BEGIN
    SELECT customer_id, kind INTO refund_customer, refund_kind
        FROM customer_account_entries WHERE id = NEW.refund_entry_id;

    IF refund_kind IS DISTINCT FROM 'credit_refund' THEN
        RAISE EXCEPTION 'only a credit_refund entry can attribute refunded credit, not %', refund_kind;
    END IF;

    SELECT customer_id, kind INTO receipt_customer, receipt_kind
        FROM customer_account_entries WHERE id = NEW.receipt_entry_id;

    IF receipt_kind IS DISTINCT FROM 'receipt' THEN
        RAISE EXCEPTION 'a credit_refund can only consume a receipt entry, not %', receipt_kind;
    END IF;

    IF refund_customer IS DISTINCT FROM receipt_customer THEN
        RAISE EXCEPTION 'a credit_refund cannot consume a receipt of another customer';
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER customer_account_credit_refund_allocations_valid
    BEFORE INSERT ON customer_account_credit_refund_allocations
    FOR EACH ROW
    EXECUTE FUNCTION customer_account_credit_refund_allocations_are_valid();

CREATE OR REPLACE FUNCTION customer_account_credit_refund_allocations_fit_refund() RETURNS trigger AS $$
DECLARE
    refund_amount numeric(14, 2);
    attributed    numeric(14, 2);
BEGIN
    SELECT amount INTO refund_amount
        FROM customer_account_entries WHERE id = NEW.refund_entry_id;
    SELECT COALESCE(SUM(amount), 0) INTO attributed
        FROM customer_account_credit_refund_allocations
        WHERE refund_entry_id = NEW.refund_entry_id;

    IF attributed > refund_amount THEN
        RAISE EXCEPTION 'refund attributions (%) exceed the credit_refund amount (%)',
            attributed, refund_amount;
    END IF;

    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE CONSTRAINT TRIGGER customer_account_credit_refund_allocations_fit_refund
    AFTER INSERT ON customer_account_credit_refund_allocations
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    EXECUTE FUNCTION customer_account_credit_refund_allocations_fit_refund();

CREATE OR REPLACE FUNCTION customer_account_credit_refund_allocations_fit_receipt() RETURNS trigger AS $$
DECLARE
    collected numeric(14, 2);
    allocated numeric(14, 2);
    refunded  numeric(14, 2);
BEGIN
    SELECT -amount INTO collected
        FROM customer_account_entries WHERE id = NEW.receipt_entry_id;
    SELECT COALESCE(SUM(amount), 0) INTO allocated
        FROM customer_account_allocations WHERE entry_id = NEW.receipt_entry_id;
    SELECT COALESCE(SUM(amount), 0) INTO refunded
        FROM customer_account_credit_refund_allocations
        WHERE receipt_entry_id = NEW.receipt_entry_id;

    IF allocated + refunded > collected THEN
        RAISE EXCEPTION 'allocations (%) plus refunds (%) exceed the money the receipt collected (%)',
            allocated, refunded, collected;
    END IF;

    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE CONSTRAINT TRIGGER customer_account_credit_refund_allocations_fit_receipt
    AFTER INSERT ON customer_account_credit_refund_allocations
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    EXECUTE FUNCTION customer_account_credit_refund_allocations_fit_receipt();

-- Strengthen the existing receipt ceiling trigger so sale allocations also
-- respect credit already returned via `credit_refund`.
CREATE OR REPLACE FUNCTION customer_account_allocations_fit_receipt() RETURNS trigger AS $$
DECLARE
    collected numeric(14, 2);
    allocated numeric(14, 2);
    refunded  numeric(14, 2);
BEGIN
    SELECT -amount INTO collected
        FROM customer_account_entries WHERE id = NEW.entry_id;
    SELECT COALESCE(SUM(amount), 0) INTO allocated
        FROM customer_account_allocations WHERE entry_id = NEW.entry_id;
    SELECT COALESCE(SUM(amount), 0) INTO refunded
        FROM customer_account_credit_refund_allocations
        WHERE receipt_entry_id = NEW.entry_id;

    IF allocated + refunded > collected THEN
        RAISE EXCEPTION 'allocations (%) plus refunds (%) exceed the money the receipt collected (%)',
            allocated, refunded, collected;
    END IF;

    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

-- Backfill any historical `credit_refund` rows before enforcing completeness
-- for future inserts. Prefer receipts created at or before the refund, matching
-- the refund's wallet first and oldest-first within each group.
DO $$
DECLARE
    r         RECORD;
    cand      RECORD;
    remaining numeric(14, 2);
    take      numeric(14, 2);
BEGIN
    FOR r IN
        SELECT e.id, e.customer_id, e.amount, e.created_at, wm.wallet_id
        FROM customer_account_entries e
        LEFT JOIN wallet_movements wm ON wm.id = e.wallet_movement_id
        WHERE e.kind = 'credit_refund'
        ORDER BY e.created_at, e.id
    LOOP
        remaining := r.amount;

        FOR cand IN
            SELECT rc.id,
                   (
                       -rc.amount
                       - COALESCE(alloc.total, 0)
                       - COALESCE(ref_alloc.total, 0)
                   )::numeric(14, 2) AS available
            FROM customer_account_entries rc
            LEFT JOIN wallet_movements rwm ON rwm.id = rc.wallet_movement_id
            LEFT JOIN LATERAL (
                SELECT SUM(a.amount) AS total
                FROM customer_account_allocations a
                WHERE a.entry_id = rc.id
            ) alloc ON true
            LEFT JOIN LATERAL (
                SELECT SUM(ra.amount) AS total
                FROM customer_account_credit_refund_allocations ra
                WHERE ra.receipt_entry_id = rc.id
            ) ref_alloc ON true
            WHERE rc.customer_id = r.customer_id
              AND rc.kind = 'receipt'
              AND -rc.amount - COALESCE(alloc.total, 0) - COALESCE(ref_alloc.total, 0) > 0
            ORDER BY
                CASE WHEN rc.created_at <= r.created_at THEN 0 ELSE 1 END,
                CASE WHEN rwm.wallet_id = r.wallet_id THEN 0 ELSE 1 END,
                rc.created_at,
                rc.id
        LOOP
            EXIT WHEN remaining <= 0;
            take := LEAST(remaining, cand.available);
            INSERT INTO customer_account_credit_refund_allocations
                (id, refund_entry_id, receipt_entry_id, amount, created_at)
            VALUES
                (gen_random_uuid(), r.id, cand.id, take, r.created_at);
            remaining := remaining - take;
        END LOOP;

        IF remaining > 0 THEN
            RAISE EXCEPTION 'unable to backfill customer credit_refund % (% remaining)', r.id, remaining;
        END IF;
    END LOOP;
END;
$$;

CREATE OR REPLACE FUNCTION customer_account_credit_refund_is_attributed() RETURNS trigger AS $$
DECLARE
    attributed numeric(14, 2);
BEGIN
    SELECT COALESCE(SUM(amount), 0) INTO attributed
        FROM customer_account_credit_refund_allocations
        WHERE refund_entry_id = NEW.id;

    IF attributed <> NEW.amount THEN
        RAISE EXCEPTION 'credit_refund (%) must be fully attributed to receipts (attributed %)',
            NEW.amount, attributed;
    END IF;

    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE CONSTRAINT TRIGGER customer_account_credit_refund_is_attributed
    AFTER INSERT ON customer_account_entries
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    WHEN (NEW.kind = 'credit_refund')
    EXECUTE FUNCTION customer_account_credit_refund_is_attributed();

-- ============================================================================
-- Supplier accounts: credit refund attribution
-- ============================================================================

CREATE TABLE supplier_account_credit_refund_allocations (
    id              uuid           PRIMARY KEY,
    refund_entry_id uuid           NOT NULL REFERENCES supplier_account_entries (id) ON DELETE RESTRICT,
    source_entry_id uuid           NOT NULL REFERENCES supplier_account_entries (id) ON DELETE RESTRICT,
    amount          numeric(14, 2) NOT NULL,
    created_at      timestamptz    NOT NULL DEFAULT now(),

    CONSTRAINT supplier_account_credit_refund_allocations_amount_positive
        CHECK (amount > 0),
    CONSTRAINT supplier_account_credit_refund_allocations_once_per_source
        UNIQUE (refund_entry_id, source_entry_id)
);

CREATE INDEX supplier_account_credit_refund_allocations_source_idx
    ON supplier_account_credit_refund_allocations (source_entry_id);

CREATE TRIGGER supplier_account_credit_refund_allocations_no_update
    BEFORE UPDATE OR DELETE ON supplier_account_credit_refund_allocations
    FOR EACH ROW
    EXECUTE FUNCTION supplier_accounts_are_append_only();

CREATE OR REPLACE FUNCTION supplier_account_credit_refund_allocations_are_valid() RETURNS trigger AS $$
DECLARE
    refund_supplier uuid;
    refund_kind     supplier_account_entry_kind;
    source_supplier uuid;
    source_kind     supplier_account_entry_kind;
BEGIN
    SELECT supplier_id, kind INTO refund_supplier, refund_kind
        FROM supplier_account_entries WHERE id = NEW.refund_entry_id;

    IF refund_kind IS DISTINCT FROM 'credit_refund' THEN
        RAISE EXCEPTION 'only a credit_refund entry can attribute refunded credit, not %', refund_kind;
    END IF;

    SELECT supplier_id, kind INTO source_supplier, source_kind
        FROM supplier_account_entries WHERE id = NEW.source_entry_id;

    IF source_kind IS NULL OR source_kind NOT IN ('payment', 'purchase_return_credit') THEN
        RAISE EXCEPTION 'a credit_refund can only consume a payment or purchase_return_credit entry, not %', source_kind;
    END IF;

    IF refund_supplier IS DISTINCT FROM source_supplier THEN
        RAISE EXCEPTION 'a credit_refund cannot consume an entry of another supplier';
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER supplier_account_credit_refund_allocations_valid
    BEFORE INSERT ON supplier_account_credit_refund_allocations
    FOR EACH ROW
    EXECUTE FUNCTION supplier_account_credit_refund_allocations_are_valid();

CREATE OR REPLACE FUNCTION supplier_account_credit_refund_allocations_fit_refund() RETURNS trigger AS $$
DECLARE
    refund_amount numeric(14, 2);
    attributed    numeric(14, 2);
BEGIN
    SELECT amount INTO refund_amount
        FROM supplier_account_entries WHERE id = NEW.refund_entry_id;
    SELECT COALESCE(SUM(amount), 0) INTO attributed
        FROM supplier_account_credit_refund_allocations
        WHERE refund_entry_id = NEW.refund_entry_id;

    IF attributed > refund_amount THEN
        RAISE EXCEPTION 'refund attributions (%) exceed the credit_refund amount (%)',
            attributed, refund_amount;
    END IF;

    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE CONSTRAINT TRIGGER supplier_account_credit_refund_allocations_fit_refund
    AFTER INSERT ON supplier_account_credit_refund_allocations
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    EXECUTE FUNCTION supplier_account_credit_refund_allocations_fit_refund();

CREATE OR REPLACE FUNCTION supplier_account_credit_refund_allocations_fit_source() RETURNS trigger AS $$
DECLARE
    paid      numeric(14, 2);
    allocated numeric(14, 2);
    refunded  numeric(14, 2);
BEGIN
    SELECT -amount INTO paid
        FROM supplier_account_entries WHERE id = NEW.source_entry_id;
    SELECT COALESCE(SUM(amount), 0) INTO allocated
        FROM supplier_account_allocations WHERE entry_id = NEW.source_entry_id;
    SELECT COALESCE(SUM(amount), 0) INTO refunded
        FROM supplier_account_credit_refund_allocations
        WHERE source_entry_id = NEW.source_entry_id;

    IF allocated + refunded > paid THEN
        RAISE EXCEPTION 'allocations (%) plus refunds (%) exceed the money paid (%)',
            allocated, refunded, paid;
    END IF;

    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE CONSTRAINT TRIGGER supplier_account_credit_refund_allocations_fit_source
    AFTER INSERT ON supplier_account_credit_refund_allocations
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    EXECUTE FUNCTION supplier_account_credit_refund_allocations_fit_source();

-- Strengthen the existing payment ceiling trigger so purchase allocations also
-- respect credit already returned via `credit_refund`.
CREATE OR REPLACE FUNCTION supplier_account_allocations_fit_payment() RETURNS trigger AS $$
DECLARE
    paid      numeric(14, 2);
    allocated numeric(14, 2);
    refunded  numeric(14, 2);
BEGIN
    SELECT -amount INTO paid
        FROM supplier_account_entries WHERE id = NEW.entry_id;
    SELECT COALESCE(SUM(amount), 0) INTO allocated
        FROM supplier_account_allocations WHERE entry_id = NEW.entry_id;
    SELECT COALESCE(SUM(amount), 0) INTO refunded
        FROM supplier_account_credit_refund_allocations
        WHERE source_entry_id = NEW.entry_id;

    IF allocated + refunded > paid THEN
        RAISE EXCEPTION 'allocations (%) plus refunds (%) exceed the money paid (%)',
            allocated, refunded, paid;
    END IF;

    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

-- Backfill any historical supplier `credit_refund` rows before enforcing
-- completeness for future inserts.
DO $$
DECLARE
    r         RECORD;
    cand      RECORD;
    remaining numeric(14, 2);
    take      numeric(14, 2);
BEGIN
    FOR r IN
        SELECT e.id, e.supplier_id, e.amount, e.created_at, wm.wallet_id
        FROM supplier_account_entries e
        LEFT JOIN wallet_movements wm ON wm.id = e.wallet_movement_id
        WHERE e.kind = 'credit_refund'
        ORDER BY e.created_at, e.id
    LOOP
        remaining := r.amount;

        FOR cand IN
            SELECT src.id,
                   (
                       -src.amount
                       - COALESCE(alloc.total, 0)
                       - COALESCE(ref_alloc.total, 0)
                   )::numeric(14, 2) AS available
            FROM supplier_account_entries src
            LEFT JOIN wallet_movements swm ON swm.id = src.wallet_movement_id
            LEFT JOIN LATERAL (
                SELECT SUM(a.amount) AS total
                FROM supplier_account_allocations a
                WHERE a.entry_id = src.id
            ) alloc ON true
            LEFT JOIN LATERAL (
                SELECT SUM(ra.amount) AS total
                FROM supplier_account_credit_refund_allocations ra
                WHERE ra.source_entry_id = src.id
            ) ref_alloc ON true
            WHERE src.supplier_id = r.supplier_id
              AND src.kind IN ('payment', 'purchase_return_credit')
              AND -src.amount - COALESCE(alloc.total, 0) - COALESCE(ref_alloc.total, 0) > 0
            ORDER BY
                CASE WHEN src.created_at <= r.created_at THEN 0 ELSE 1 END,
                CASE WHEN swm.wallet_id = r.wallet_id THEN 0 ELSE 1 END,
                src.created_at,
                src.id
        LOOP
            EXIT WHEN remaining <= 0;
            take := LEAST(remaining, cand.available);
            INSERT INTO supplier_account_credit_refund_allocations
                (id, refund_entry_id, source_entry_id, amount, created_at)
            VALUES
                (gen_random_uuid(), r.id, cand.id, take, r.created_at);
            remaining := remaining - take;
        END LOOP;

        IF remaining > 0 THEN
            RAISE EXCEPTION 'unable to backfill supplier credit_refund % (% remaining)', r.id, remaining;
        END IF;
    END LOOP;
END;
$$;

CREATE OR REPLACE FUNCTION supplier_account_credit_refund_is_attributed() RETURNS trigger AS $$
DECLARE
    attributed numeric(14, 2);
BEGIN
    SELECT COALESCE(SUM(amount), 0) INTO attributed
        FROM supplier_account_credit_refund_allocations
        WHERE refund_entry_id = NEW.id;

    IF attributed <> NEW.amount THEN
        RAISE EXCEPTION 'credit_refund (%) must be fully attributed to payments or return credits (attributed %)',
            NEW.amount, attributed;
    END IF;

    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE CONSTRAINT TRIGGER supplier_account_credit_refund_is_attributed
    AFTER INSERT ON supplier_account_entries
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    WHEN (NEW.kind = 'credit_refund')
    EXECUTE FUNCTION supplier_account_credit_refund_is_attributed();
