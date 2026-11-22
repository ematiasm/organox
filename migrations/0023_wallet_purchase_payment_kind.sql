-- Wallet movement kind written when completing a cash purchase.
--
-- Own migration because PostgreSQL refuses to reference a newly added enum
-- label in CHECK constraints within the same transaction ("unsafe use of new
-- value"). The label commits here first, then 0024 adds the `purchases` tables
-- and the `wallet_movements` link constraints.

ALTER TYPE wallet_movement_kind ADD VALUE IF NOT EXISTS 'purchase_payment';
