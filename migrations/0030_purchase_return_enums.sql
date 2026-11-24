-- Enum values for purchase returns (supplier returns).
--
-- PostgreSQL requires new enum labels to commit before subsequent migrations
-- reference them in CHECK constraints or trigger functions.

ALTER TYPE movement_reason ADD VALUE 'purchase_return';
ALTER TYPE wallet_movement_kind ADD VALUE 'purchase_refund';
ALTER TYPE supplier_account_entry_kind ADD VALUE 'purchase_return';
ALTER TYPE supplier_account_entry_kind ADD VALUE 'purchase_return_credit';
