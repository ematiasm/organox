-- Extend the customer-link CHECK constraints on `wallet_movements` for
-- `account_payment_refund`.
--
-- Just like `account_payment`, a credit refund hands money back for a specific
-- customer through a payment method, never for a sale or a credit note. Both
-- account kinds must carry `customer_id` and `payment_method_id`, and no other
-- kind may carry a `customer_id`.

ALTER TABLE wallet_movements
    DROP CONSTRAINT wallet_movements_customer_link_only_for_account_kinds,
    ADD CONSTRAINT wallet_movements_customer_link_only_for_account_kinds CHECK (
        kind IN ('account_payment', 'account_payment_refund') OR customer_id IS NULL
    );

ALTER TABLE wallet_movements
    DROP CONSTRAINT wallet_movements_account_payment_is_complete,
    ADD CONSTRAINT wallet_movements_account_payment_is_complete CHECK (
        kind NOT IN ('account_payment', 'account_payment_refund')
        OR (customer_id IS NOT NULL AND payment_method_id IS NOT NULL)
    );
