-- Slice 2: the sale names the customer it serves.
--
-- `customer_id` is OPTIONAL: a walk-in buyer with no registered customer is
-- still a sale, and a NOT NULL column would break every anonymous row already
-- charged. The reference is ON DELETE RESTRICT because customers are
-- deactivated, never deleted (0013): history points at this row, and there is
-- no DELETE route anyway -- RESTRICT is the last line of defense if one ever
-- appears.
--
-- `customer_name` is the SNAPSHOT written when the sale completes. A
-- completed sale is a fact: renaming the customer afterwards must not change
-- what the charged sale says, exactly as `sale_items.unit_price` freezes the
-- price at the moment it was charged. Drafts store no snapshot; while the sale
-- is still editable the API reads the joined live name instead.

ALTER TABLE sales
    ADD COLUMN customer_id uuid REFERENCES customers (id) ON DELETE RESTRICT;

ALTER TABLE sales
    ADD COLUMN customer_name text;

-- A snapshot without the customer it froze would be history pointing at
-- nothing. The reverse is normal and constant: every draft has a customer
-- (or none) and no snapshot yet.
ALTER TABLE sales
    ADD CONSTRAINT sales_customer_name_needs_its_customer CHECK (
        customer_id IS NOT NULL OR customer_name IS NULL
    );

-- Lookups from the customer side ("which sales point at this customer") and
-- the join every sale read performs. Partial, like 0011: the rows without a
-- customer are not worth indexing for this lookup.
CREATE INDEX sales_customer_id_idx ON sales (customer_id)
    WHERE customer_id IS NOT NULL;
