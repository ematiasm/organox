-- Customer accounts: the debt ledger.
--
-- Why a second ledger instead of a new payment method: `wallet_movements`
-- records money sitting in a wallet; this table records money a customer owes.
-- A wallet movement is written only when money truly moves, so a sale charged
-- to the account writes no wallet movement at all. Mixing the two would let the
-- cash count include receivables, and that error is silent.
--
-- Why the amount is signed here, while wallet amounts are positive and the sign
-- lives in the kind: a wallet balance is `SUM(CASE kind ...)` and that CASE is
-- duplicated in every query that sums a balance — `sale_refund` had to touch
-- six of them. Here a single signed column carries the effect, one CHECK keeps
-- it honest, and every sum is `SUM(amount)`.
--
-- Why nothing stores a balance: same reason as wallets and stock. The balance is
-- `SUM(amount)`. A stored balance drifts the first time a transaction forgets
-- to update it, and after that nobody can tell which number is true.
--
-- Why there is a separate `due` concept missing: a sale's outstanding amount is
-- derived (charge - cancelled by notes - allocated receipts), never stored.

CREATE TYPE customer_account_entry_kind AS ENUM (
    'sale_charge',   -- + a sale left part of its total owed
    'receipt',       -- - money collected against the account
    'credit_note',   -- - a note cancelled part of a sale's debt
    'credit_refund', -- + money handed back against a credit balance
    'adjustment_in', -- + manual increase: opening balance, correction
    'adjustment_out' -- - manual decrease: correction
);

CREATE TABLE customer_account_entries (
    id                 uuid                          PRIMARY KEY,
    customer_id        uuid                          NOT NULL REFERENCES customers (id) ON DELETE RESTRICT,
    kind               customer_account_entry_kind   NOT NULL,
    -- Signed: positive grows the debt, negative shrinks it. Never zero.
    amount             numeric(14, 2)                NOT NULL,
    -- The sale this entry is about, when the debt is attributable to one.
    sale_id            uuid                          REFERENCES sales (id) ON DELETE RESTRICT,
    -- The note that cancelled debt, when the entry came from a credit note.
    credit_note_id     uuid                          REFERENCES credit_notes (id) ON DELETE RESTRICT,
    -- The movement that brought or returned the money, for receipts and refunds.
    wallet_movement_id uuid                          REFERENCES wallet_movements (id) ON DELETE RESTRICT,
    -- Replaying a receipt returns the first one instead of collecting twice.
    idempotency_key    text,
    note               text,
    -- No foreign key to `users`, for the same reason as
    -- `wallet_movements.created_by`: the ledger is immutable, so deleting an
    -- account must not be able to rewrite or orphan history.
    created_by         uuid                          NOT NULL,
    created_at         timestamptz                   NOT NULL DEFAULT now(),

    CONSTRAINT customer_account_entries_amount_not_zero CHECK (amount <> 0),
    -- One place decides which direction each kind moves the balance. Without
    -- this, a receipt stored positive would silently double a debt.
    CONSTRAINT customer_account_entries_sign_matches_kind CHECK (
        (kind IN ('sale_charge', 'credit_refund', 'adjustment_in') AND amount > 0)
        OR (kind IN ('receipt', 'credit_note', 'adjustment_out') AND amount < 0)
    ),
    -- A charge or a note is always about one sale, and no other kind is.
    CONSTRAINT customer_account_entries_sale_link CHECK (
        (kind IN ('sale_charge', 'credit_note')) = (sale_id IS NOT NULL)
    ),
    CONSTRAINT customer_account_entries_credit_note_link CHECK (
        (kind = 'credit_note') = (credit_note_id IS NOT NULL)
    ),
    -- Receipts and refunds name the movement that carried the money, so the
    -- ledger can never claim money that no wallet saw.
    CONSTRAINT customer_account_entries_wallet_link CHECK (
        (kind IN ('receipt', 'credit_refund')) = (wallet_movement_id IS NOT NULL)
    ),
    -- A manual adjustment without a reason is untraceable money.
    CONSTRAINT customer_account_entries_adjustment_note CHECK (
        kind NOT IN ('adjustment_in', 'adjustment_out')
        OR (note IS NOT NULL AND btrim(note) <> '')
    )
);

CREATE INDEX customer_account_entries_customer_idx
    ON customer_account_entries (customer_id, created_at);
CREATE INDEX customer_account_entries_sale_idx
    ON customer_account_entries (sale_id) WHERE sale_id IS NOT NULL;

-- A sale is charged to the account once, ever: a retried completion cannot
-- bill the same sale twice.
CREATE UNIQUE INDEX customer_account_entries_one_charge_per_sale
    ON customer_account_entries (sale_id) WHERE kind = 'sale_charge';
-- A note cancels debt once.
CREATE UNIQUE INDEX customer_account_entries_one_entry_per_note
    ON customer_account_entries (credit_note_id) WHERE credit_note_id IS NOT NULL;
-- A wallet movement backs at most one ledger entry, receipt or refund.
CREATE UNIQUE INDEX customer_account_entries_one_entry_per_movement
    ON customer_account_entries (wallet_movement_id) WHERE wallet_movement_id IS NOT NULL;
CREATE UNIQUE INDEX customer_account_entries_receipt_idempotency
    ON customer_account_entries (customer_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

-- Which sales a receipt pays. The remainder of a receipt carries no row here and
-- that is exactly what an available credit is: money collected, not allocated.
CREATE TABLE customer_account_allocations (
    id         uuid           PRIMARY KEY,
    entry_id   uuid           NOT NULL REFERENCES customer_account_entries (id) ON DELETE RESTRICT,
    sale_id    uuid           NOT NULL REFERENCES sales (id) ON DELETE RESTRICT,
    amount     numeric(14, 2) NOT NULL,
    created_at timestamptz    NOT NULL DEFAULT now(),

    CONSTRAINT customer_account_allocations_amount_positive CHECK (amount > 0),
    CONSTRAINT customer_account_allocations_once_per_sale UNIQUE (entry_id, sale_id)
);

CREATE INDEX customer_account_allocations_sale_idx
    ON customer_account_allocations (sale_id);

-- Append-only, enforced by the engine and not by convention: a mistake is fixed
-- with a compensating entry, never by editing history.
CREATE OR REPLACE FUNCTION customer_accounts_are_append_only() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION '% is append-only: record a compensating entry instead of rewriting history', TG_TABLE_NAME;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER customer_account_entries_no_update
    BEFORE UPDATE OR DELETE ON customer_account_entries
    FOR EACH ROW
    EXECUTE FUNCTION customer_accounts_are_append_only();

CREATE TRIGGER customer_account_allocations_no_update
    BEFORE UPDATE OR DELETE ON customer_account_allocations
    FOR EACH ROW
    EXECUTE FUNCTION customer_accounts_are_append_only();

-- An allocation spans two tables, so no CHECK can express it: only a receipt
-- says "this money pays these sales", and it can only pay a completed sale of
-- the same customer. Paying somebody else's sale with your money would move a
-- debt between two people, which is worse than losing the row.
CREATE OR REPLACE FUNCTION customer_account_allocations_are_receipts() RETURNS trigger AS $$
DECLARE
    receipt_customer uuid;
    receipt_kind     customer_account_entry_kind;
    sale_customer    uuid;
    sale_status      sale_status;
BEGIN
    SELECT customer_id, kind INTO receipt_customer, receipt_kind
        FROM customer_account_entries WHERE id = NEW.entry_id;

    IF receipt_kind IS DISTINCT FROM 'receipt' THEN
        RAISE EXCEPTION 'only a receipt can be allocated to a sale, not %', receipt_kind;
    END IF;

    SELECT customer_id, status INTO sale_customer, sale_status
        FROM sales WHERE id = NEW.sale_id;

    IF sale_status IS DISTINCT FROM 'completed' THEN
        RAISE EXCEPTION 'only a completed sale can be paid from the account, not %', sale_status;
    END IF;

    IF sale_customer IS DISTINCT FROM receipt_customer THEN
        RAISE EXCEPTION 'a sale of another customer cannot be paid from this receipt';
    END IF;

    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER customer_account_allocations_receipt_only
    BEFORE INSERT ON customer_account_allocations
    FOR EACH ROW
    EXECUTE FUNCTION customer_account_allocations_are_receipts();

-- A receipt pays at most what it collected. Deferred, because the allocations of
-- one receipt arrive as a batch and the intermediate sums are meaningless.
CREATE OR REPLACE FUNCTION customer_account_allocations_fit_receipt() RETURNS trigger AS $$
DECLARE
    collected numeric(14, 2);
    allocated numeric(14, 2);
BEGIN
    SELECT -amount INTO collected FROM customer_account_entries WHERE id = NEW.entry_id;
    SELECT COALESCE(SUM(amount), 0) INTO allocated
        FROM customer_account_allocations WHERE entry_id = NEW.entry_id;

    IF allocated > collected THEN
        RAISE EXCEPTION 'allocations (%) exceed the money the receipt collected (%)', allocated, collected;
    END IF;

    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE CONSTRAINT TRIGGER customer_account_allocations_fit_receipt
    AFTER INSERT ON customer_account_allocations
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    EXECUTE FUNCTION customer_account_allocations_fit_receipt();
