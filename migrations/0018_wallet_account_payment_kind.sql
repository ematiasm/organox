-- T4 of the customer-accounts feature: the wallet kind a collection writes.
--
-- Why this is a migration of its own, exactly like 0009: PostgreSQL refuses to
-- use a new enum label inside the transaction that adds it ("unsafe use of new
-- value"), and 0019's CHECK constraints compare `account_payment`. The label
-- has to be committed first, alone.

ALTER TYPE wallet_movement_kind ADD VALUE IF NOT EXISTS 'account_payment';
