-- Wallet movement kind for paying down a supplier account (outflow, -1).
--
-- Added in its own migration because Postgres forbids using a newly added enum
-- label inside the same transaction that introduces it.

ALTER TYPE wallet_movement_kind ADD VALUE 'supplier_account_payment';
