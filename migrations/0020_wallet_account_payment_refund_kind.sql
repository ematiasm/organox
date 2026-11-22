-- T7 of the customer-accounts feature: the wallet kind a credit refund writes.
--
-- A migration of its own, for the same reason as 0009 and 0018: PostgreSQL
-- refuses to use a new enum label inside the transaction that adds it ("unsafe
-- use of new value"), and 0021's CHECK constraints compare
-- `account_payment_refund`. The label has to be committed first, alone.

ALTER TYPE wallet_movement_kind ADD VALUE IF NOT EXISTS 'account_payment_refund';
