-- Slice 8: payment methods, wallets, and the money ledger.
--
-- Why payment methods are rows and not an enum: the operator must be able to add
-- "Mercado Pago" or a second bank account without a deploy. A method is business
-- data. The movement *kind*, on the other hand, is code semantics: every kind
-- needs a writer, a validation and a screen, so it is an enum, exactly like
-- `movement_reason`.
--
-- Why the wallet is copied onto every movement: `payment_methods.wallet_id` is
-- the default used when charging. If balances were computed by following
-- method -> wallet, editing a method (fixing a misconfigured account) would
-- silently rebucket every past movement and change yesterday's balance. The
-- ledger records where the money actually went, at the moment it went.
--
-- Why the balance is not a column: same reason as stock. It is the SUM of the
-- movements. A stored balance drifts the first time a transaction forgets to
-- update it, and nobody can tell which number is true.
--
-- As with stock, amounts are always positive and the kind carries the sign:
-- "an expense of -500" that adds money is a sign bug waiting to happen.

CREATE TYPE wallet_movement_kind AS ENUM (
    'sale_payment',   -- inflow: a sale was charged with this money
    'expense',        -- outflow: money left the wallet for a real payment
    'adjustment_in',  -- inflow: correction that adds money
    'adjustment_out'  -- outflow: correction that removes money
);

CREATE TABLE wallets (
    id         uuid        PRIMARY KEY,
    name       text        NOT NULL,
    is_active  boolean     NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT wallets_name_not_blank CHECK (btrim(name) <> '')
);

-- Case-insensitive: "Caja" and "caja" are the same wallet, and two of them is a
-- balance split in half.
CREATE UNIQUE INDEX wallets_name_lower_unique ON wallets (lower(name));

CREATE TABLE payment_methods (
    id         uuid        PRIMARY KEY,
    name       text        NOT NULL,
    -- The wallet this method pays into. A default, not the read path: see the
    -- header note. RESTRICT because a wallet with movements must keep existing.
    wallet_id  uuid        NOT NULL REFERENCES wallets (id) ON DELETE RESTRICT,
    is_active  boolean     NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT payment_methods_name_not_blank CHECK (btrim(name) <> '')
);

CREATE UNIQUE INDEX payment_methods_name_lower_unique ON payment_methods (lower(name));
CREATE INDEX payment_methods_wallet_idx ON payment_methods (wallet_id);

CREATE TABLE wallet_movements (
    id                uuid                 PRIMARY KEY,
    wallet_id         uuid                 NOT NULL REFERENCES wallets (id) ON DELETE RESTRICT,
    kind              wallet_movement_kind NOT NULL,
    -- Always positive. The sign lives in `kind`.
    amount            numeric(14, 2)       NOT NULL,
    -- A sale payment names the sale it collected. NULL on manual movements.
    sale_id           uuid                 REFERENCES sales (id) ON DELETE RESTRICT,
    -- Which method was used, for provenance. NULL on manual movements: an
    -- expense is paid from a wallet, not "through" a method.
    payment_method_id uuid                 REFERENCES payment_methods (id) ON DELETE RESTRICT,
    note              text,
    -- Who recorded it. No foreign key, for the same reason as
    -- `stock_movements.created_by`: the ledger is immutable, so a referenced
    -- account cannot be allowed to rewrite history when it is deleted.
    created_by        uuid,
    created_at        timestamptz          NOT NULL DEFAULT now(),

    CONSTRAINT wallet_movements_amount_positive CHECK (amount > 0),
    -- A sale payment without its sale or its method is an orphan: money that
    -- appeared in a wallet with nothing explaining it.
    CONSTRAINT wallet_movements_sale_payment_is_complete CHECK (
        kind <> 'sale_payment'
        OR (sale_id IS NOT NULL AND payment_method_id IS NOT NULL)
    ),
    -- Only a sale payment may point at a sale.
    CONSTRAINT wallet_movements_only_sale_payment_references_a_sale CHECK (
        kind = 'sale_payment' OR sale_id IS NULL
    )
);

-- The query behind every balance: SUM(amount with the sign of its kind) per
-- wallet. Same shape as stock_movements_product_idx.
CREATE INDEX wallet_movements_wallet_idx ON wallet_movements (wallet_id, created_at DESC);
CREATE INDEX wallet_movements_sale_idx ON wallet_movements (sale_id) WHERE sale_id IS NOT NULL;
CREATE INDEX wallet_movements_created_at_idx ON wallet_movements (created_at DESC);

-- Append-only, enforced by the engine and not by convention: a mistake is fixed
-- with an adjustment movement, never by editing history.
CREATE OR REPLACE FUNCTION wallet_movements_are_append_only() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'wallet_movements is append-only: record an adjustment instead of rewriting history';
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER wallet_movements_no_update
    BEFORE UPDATE OR DELETE ON wallet_movements
    FOR EACH ROW
    EXECUTE FUNCTION wallet_movements_are_append_only();

CREATE TRIGGER wallets_set_updated_at
    BEFORE UPDATE ON wallets
    FOR EACH ROW
    EXECUTE FUNCTION set_updated_at();

CREATE TRIGGER payment_methods_set_updated_at
    BEFORE UPDATE ON payment_methods
    FOR EACH ROW
    EXECUTE FUNCTION set_updated_at();
