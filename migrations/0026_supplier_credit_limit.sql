-- Supplier accounts: the per-supplier credit limit.
--
-- Mirrors `0017_customer_credit_limit.sql`: NULL means "no limit", 0 means
-- "cash only", and a positive value caps total exposure (`balance + on_account`).
-- Existing suppliers default to NULL so nothing silently blocks them.

ALTER TABLE suppliers ADD COLUMN credit_limit numeric(14, 2);

ALTER TABLE suppliers ADD CONSTRAINT suppliers_credit_limit_not_negative
    CHECK (credit_limit IS NULL OR credit_limit >= 0);
