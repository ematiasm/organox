-- Supplier accounts: the payables ledger.
--
-- Mirrors `customer_accounts` (`0015` and `0016`) for suppliers: `wallet_movements`
-- records real money entering or leaving wallets, while `supplier_account_entries`
-- records what the business owes a supplier (or advance credit held with them).
-- A purchase charged on account writes no wallet movement at all; a wallet
-- movement is written only when cash or bank money actually moves.
--
-- Signed `amount`: positive increases what we owe the supplier, negative reduces
-- what we owe (or builds an advance/credit balance with the supplier). Neither
-- `suppliers` nor `purchases` stores a balance or paid flag; every balance and
-- purchase outstanding is derived from this append-only ledger.

CREATE TYPE supplier_account_entry_kind AS ENUM (
    'purchase_charge', -- + a purchase left part of its total on account
    'payment',         -- - money paid against the supplier account
    'credit_refund',   -- + unapplied advance/credit refunded back by the supplier
    'adjustment_in',   -- + manual increase of payable: opening balance, correction
    'adjustment_out'   -- - manual decrease of payable: correction
);

CREATE TABLE supplier_account_entries (
    id                 uuid                        PRIMARY KEY,
    supplier_id        uuid                        NOT NULL REFERENCES suppliers (id) ON DELETE RESTRICT,
    kind               supplier_account_entry_kind NOT NULL,
    -- Signed: positive grows what we owe, negative shrinks it. Never zero.
    amount             numeric(14, 2)              NOT NULL,
    -- The purchase this entry is about, when the payable came from one.
    purchase_id        uuid                        REFERENCES purchases (id) ON DELETE RESTRICT,
    -- The movement that paid the supplier or received a credit refund back.
    wallet_movement_id uuid                        REFERENCES wallet_movements (id) ON DELETE RESTRICT,
    -- Replaying a payment or credit refund returns the first entry instead of
    -- writing twice.
    idempotency_key    text,
    note               text,
    -- No foreign key to `users`, matching `customer_account_entries` and
    -- `wallet_movements`: the ledger is immutable history.
    created_by         uuid                        NOT NULL,
    created_at         timestamptz                 NOT NULL DEFAULT now(),

    CONSTRAINT supplier_account_entries_amount_not_zero CHECK (amount <> 0),
    CONSTRAINT supplier_account_entries_sign_matches_kind CHECK (
        (kind IN ('purchase_charge', 'credit_refund', 'adjustment_in') AND amount > 0)
        OR (kind IN ('payment', 'adjustment_out') AND amount < 0)
    ),
    CONSTRAINT supplier_account_entries_purchase_link CHECK (
        (kind = 'purchase_charge') = (purchase_id IS NOT NULL)
    ),
    CONSTRAINT supplier_account_entries_wallet_link CHECK (
        (kind IN ('payment', 'credit_refund')) = (wallet_movement_id IS NOT NULL)
    ),
    CONSTRAINT supplier_account_entries_adjustment_note CHECK (
        kind NOT IN ('adjustment_in', 'adjustment_out')
        OR (note IS NOT NULL AND btrim(note) <> '')
    )
);

CREATE INDEX supplier_account_entries_supplier_idx
    ON supplier_account_entries (supplier_id, created_at);
CREATE INDEX supplier_account_entries_purchase_idx
    ON supplier_account_entries (purchase_id) WHERE purchase_id IS NOT NULL;

-- A purchase is charged to the account at most once.
CREATE UNIQUE INDEX supplier_account_entries_one_charge_per_purchase
    ON supplier_account_entries (purchase_id) WHERE kind = 'purchase_charge';
-- A wallet movement backs at most one supplier ledger entry.
CREATE UNIQUE INDEX supplier_account_entries_one_entry_per_movement
    ON supplier_account_entries (wallet_movement_id) WHERE wallet_movement_id IS NOT NULL;
CREATE UNIQUE INDEX supplier_account_entries_idempotency
    ON supplier_account_entries (supplier_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

-- Which completed purchases a supplier payment pays. Any remainder of a payment
-- carries no row here and is the unapplied credit (advance) with the supplier.
CREATE TABLE supplier_account_allocations (
    id          uuid           PRIMARY KEY,
    entry_id    uuid           NOT NULL REFERENCES supplier_account_entries (id) ON DELETE RESTRICT,
    purchase_id uuid           NOT NULL REFERENCES purchases (id) ON DELETE RESTRICT,
    amount      numeric(14, 2) NOT NULL,
    created_at  timestamptz    NOT NULL DEFAULT now(),

    CONSTRAINT supplier_account_allocations_amount_positive CHECK (amount > 0),
    CONSTRAINT supplier_account_allocations_once_per_purchase UNIQUE (entry_id, purchase_id)
);

CREATE INDEX supplier_account_allocations_purchase_idx
    ON supplier_account_allocations (purchase_id);

-- Append-only enforcement at the database level.
CREATE OR REPLACE FUNCTION supplier_accounts_are_append_only() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION '% is append-only: record a compensating entry instead of rewriting history', TG_TABLE_NAME;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER supplier_account_entries_no_update
    BEFORE UPDATE OR DELETE ON supplier_account_entries
    FOR EACH ROW
    EXECUTE FUNCTION supplier_accounts_are_append_only();

CREATE TRIGGER supplier_account_allocations_no_update
    BEFORE UPDATE OR DELETE ON supplier_account_allocations
    FOR EACH ROW
    EXECUTE FUNCTION supplier_accounts_are_append_only();

-- Only a `payment` entry may be allocated, and only to a completed purchase of
-- the same supplier.
CREATE OR REPLACE FUNCTION supplier_account_allocations_are_payments() RETURNS trigger AS $$
DECLARE
    payment_supplier  uuid;
    payment_kind      supplier_account_entry_kind;
    purchase_supplier uuid;
    p_status          purchase_status;
BEGIN
    SELECT supplier_id, kind INTO payment_supplier, payment_kind
        FROM supplier_account_entries WHERE id = NEW.entry_id;

    IF payment_kind IS DISTINCT FROM 'payment' THEN
        RAISE EXCEPTION 'only a payment can be allocated to a purchase, not %', payment_kind;
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

CREATE TRIGGER supplier_account_allocations_payment_only
    BEFORE INSERT ON supplier_account_allocations
    FOR EACH ROW
    EXECUTE FUNCTION supplier_account_allocations_are_payments();

-- A payment allocates at most what it paid out. Deferred so a batch of
-- allocations for one payment validates at commit.
CREATE OR REPLACE FUNCTION supplier_account_allocations_fit_payment() RETURNS trigger AS $$
DECLARE
    paid      numeric(14, 2);
    allocated numeric(14, 2);
BEGIN
    SELECT -amount INTO paid FROM supplier_account_entries WHERE id = NEW.entry_id;
    SELECT COALESCE(SUM(amount), 0) INTO allocated
        FROM supplier_account_allocations WHERE entry_id = NEW.entry_id;

    IF allocated > paid THEN
        RAISE EXCEPTION 'allocations (%) exceed the money paid (%)', allocated, paid;
    END IF;

    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE CONSTRAINT TRIGGER supplier_account_allocations_fit_payment
    AFTER INSERT ON supplier_account_allocations
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    EXECUTE FUNCTION supplier_account_allocations_fit_payment();

-- Per-purchase allocation ceiling: total allocations against a purchase never
-- exceed its on-account `purchase_charge`.
CREATE OR REPLACE FUNCTION supplier_account_allocations_fit_the_purchase() RETURNS trigger AS $$
DECLARE
    charged   numeric(14, 2);
    allocated numeric(14, 2);
BEGIN
    SELECT COALESCE(SUM(amount), 0) INTO charged
        FROM supplier_account_entries
        WHERE purchase_id = NEW.purchase_id AND kind = 'purchase_charge';
    SELECT COALESCE(SUM(amount), 0) INTO allocated
        FROM supplier_account_allocations
        WHERE purchase_id = NEW.purchase_id;

    IF allocated > charged THEN
        RAISE EXCEPTION 'allocations (%) exceed the outstanding (%) of the purchase', allocated, charged;
    END IF;

    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE CONSTRAINT TRIGGER supplier_account_allocations_fit_the_purchase
    AFTER INSERT ON supplier_account_allocations
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    EXECUTE FUNCTION supplier_account_allocations_fit_the_purchase();
