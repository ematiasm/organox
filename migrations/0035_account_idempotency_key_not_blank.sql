-- Enforce non-blank idempotency keys on customer and supplier account ledgers
-- at the database layer, matching `credit_notes` (0012) and `purchase_returns` (0031).
--
-- Both tables have append-only triggers that reject UPDATE/DELETE. Previous HTTP
-- handlers mapped blank keys to NULL, so adding these CHECK constraints directly
-- protects future direct/service writes without modifying existing history.

ALTER TABLE customer_account_entries
    ADD CONSTRAINT customer_account_entries_idempotency_key_not_blank
        CHECK (idempotency_key IS NULL OR btrim(idempotency_key) <> '');

ALTER TABLE supplier_account_entries
    ADD CONSTRAINT supplier_account_entries_idempotency_key_not_blank
        CHECK (idempotency_key IS NULL OR btrim(idempotency_key) <> '');
