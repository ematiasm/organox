-- Slice 10: credit notes, the correction mechanism for a charged sale.
--
-- A customer returns part or all of a sale. The sale itself is a fact: it is
-- never edited, never re-opened, never marked "returned". The correction is a
-- new document that points at it, so both stay auditable and a sale can be
-- returned in several visits.
--
-- Why the total is stored and not computed on read: it is the number the
-- customer was handed. Lines and price are frozen at the moment of the note,
-- exactly as `sale_items` freezes the price of a sale; recalculating it later
-- would let a product price change rewrite history.
--
-- Why lines point at `products` and not at `sale_items`: a line needs the
-- product, the quantity and the price that was charged. The price is copied
-- from the sale line when the note is created, so the constraint that matters
-- (quantity credited never exceeds quantity sold) is expressed against the
-- sale, which is the aggregate that owns the limit.
--
-- Quantities follow `sale_items`: positive, three decimals, unique product per
-- note. A negative "return" would be a sale, and a sale already exists.

CREATE TABLE credit_notes (
    id         uuid          PRIMARY KEY,
    -- The charged sale being corrected. RESTRICT: a credited sale must keep
    -- existing, because the note's meaning is relative to it.
    sale_id    uuid          NOT NULL REFERENCES sales (id) ON DELETE RESTRICT,
    -- The amount refunded, computed by the server from the lines.
    total      numeric(14, 2) NOT NULL,
    -- Required: a credit note without a reason cannot be explained to the
    -- customer or to a reviewer.
    reason     text          NOT NULL,
    -- No foreign key, same reason as `sales.created_by`: the document is
    -- immutable evidence, so a deleted account must not rewrite it.
    created_by uuid          NOT NULL,
    created_at timestamptz   NOT NULL DEFAULT now(),

    CONSTRAINT credit_notes_total_not_negative CHECK (total >= 0),
    CONSTRAINT credit_notes_reason_not_blank CHECK (btrim(reason) <> '')
);

CREATE INDEX credit_notes_sale_idx ON credit_notes (sale_id);
CREATE INDEX credit_notes_created_at_idx ON credit_notes (created_at DESC);

CREATE TABLE credit_note_items (
    id             uuid          PRIMARY KEY,
    credit_note_id uuid          NOT NULL REFERENCES credit_notes (id) ON DELETE CASCADE,
    product_id     uuid          NOT NULL REFERENCES products (id) ON DELETE RESTRICT,
    quantity       numeric(12, 3) NOT NULL,
    -- The price charged on the sale line, copied when the note is created.
    unit_price     numeric(12, 2) NOT NULL,

    CONSTRAINT credit_note_items_quantity_positive CHECK (quantity > 0),
    CONSTRAINT credit_note_items_unit_price_not_negative CHECK (unit_price >= 0),
    -- One line per product: "two lines of the same product" is a quantity, and
    -- two rows would let a client smuggle the limit past the sum.
    CONSTRAINT credit_note_items_unique_product UNIQUE (credit_note_id, product_id)
);

CREATE INDEX credit_note_items_product_idx ON credit_note_items (product_id);

-- Money: a refund is an outflow that keeps its sale and its method.
--
-- The two constraints below replace the sale-payment-only pair from `0008`.
-- They are additive: every existing row still satisfies them, because a
-- `sale_payment` was already required to name both, and no other kind could
-- name a sale.
ALTER TABLE wallet_movements
    DROP CONSTRAINT wallet_movements_sale_payment_is_complete;
ALTER TABLE wallet_movements
    DROP CONSTRAINT wallet_movements_only_sale_payment_references_a_sale;

-- A refund without its sale or its method would be money that left a wallet
-- with nothing explaining it, same orphan rule as a sale payment.
ALTER TABLE wallet_movements
    ADD CONSTRAINT wallet_movements_sale_kind_is_complete CHECK (
        kind NOT IN ('sale_payment', 'sale_refund')
        OR (sale_id IS NOT NULL AND payment_method_id IS NOT NULL)
    );

-- Only the two sale kinds point at a sale. An expense or an adjustment is paid
-- from a wallet, not "through" a sale, and must not pretend otherwise.
ALTER TABLE wallet_movements
    ADD CONSTRAINT wallet_movements_sale_link_only_for_sale_kinds CHECK (
        kind IN ('sale_payment', 'sale_refund') OR sale_id IS NULL
    );
