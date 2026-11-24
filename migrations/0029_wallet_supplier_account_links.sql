-- Supplier link and cross-column link exclusivity on `wallet_movements`.
--
-- Mirrors `0019_wallet_account_payment.sql` and `0021_wallet_account_payment_refund.sql`:
-- `supplier_account_payment` and `supplier_account_refund` must identify both
-- the supplier and the payment method, and no other movement kind may carry a
-- `supplier_id`. An explicit cross-column exclusivity CHECK also ensures a
-- movement never attributes money to more than one domain entity at once.

ALTER TABLE wallet_movements
    ADD COLUMN supplier_id uuid REFERENCES suppliers (id) ON DELETE RESTRICT;

ALTER TABLE wallet_movements
    ADD CONSTRAINT wallet_movements_supplier_link_only_for_account_kinds CHECK (
        kind IN ('supplier_account_payment', 'supplier_account_refund')
        OR supplier_id IS NULL
    );

ALTER TABLE wallet_movements
    ADD CONSTRAINT wallet_movements_supplier_account_is_complete CHECK (
        kind NOT IN ('supplier_account_payment', 'supplier_account_refund')
        OR (supplier_id IS NOT NULL AND payment_method_id IS NOT NULL)
    );

ALTER TABLE wallet_movements
    ADD CONSTRAINT wallet_movements_at_most_one_entity_link CHECK (
        num_nonnulls(sale_id, purchase_id, customer_id, supplier_id) <= 1
    );

CREATE INDEX wallet_movements_supplier_idx
    ON wallet_movements (supplier_id) WHERE supplier_id IS NOT NULL;
