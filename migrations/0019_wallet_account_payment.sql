-- The customer link on the money ledger.
--
-- Why the column is needed at all: a receipt claims "this movement is the money
-- I collected for this customer", and the amount check alone cannot prove whose
-- money it was. A movement that belongs to another customer's account would let
-- one customer's collection pay somebody else's debt, and the two books would
-- still agree on the amount. `customer_id` is the attribution; the ledger writer
-- compares it in `ensure_movement_backing`.
--
-- Why nullable: every movement written before this feature has no customer, and
-- a manual adjustment still has none. RESTRICT because the money ledger is
-- immutable history: deleting a customer must not orphan the movements that
-- collected their money.

ALTER TABLE wallet_movements
    ADD COLUMN customer_id uuid REFERENCES customers (id) ON DELETE RESTRICT;

-- Only the account kinds may carry a customer. NOTE: the credit-refund slice
-- (T7) must extend this constraint when `account_payment_refund` arrives, or
-- its movements will be refused by it.
ALTER TABLE wallet_movements
    ADD CONSTRAINT wallet_movements_customer_link_only_for_account_kinds CHECK (
        kind = 'account_payment' OR customer_id IS NULL
    );

-- An account payment without its customer or its method is money that appeared
-- in a wallet with nothing explaining it, the same orphan rule a sale payment
-- already follows.
ALTER TABLE wallet_movements
    ADD CONSTRAINT wallet_movements_account_payment_is_complete CHECK (
        kind <> 'account_payment'
        OR (customer_id IS NOT NULL AND payment_method_id IS NOT NULL)
    );

-- The customer-side lookups (statement, receivables) read a customer's
-- movements; partial because most movements carry no customer at all.
CREATE INDEX wallet_movements_customer_idx
    ON wallet_movements (customer_id) WHERE customer_id IS NOT NULL;
