-- Slice 1: customers, the party a sale may later be attached to.
--
-- A customer is deactivated, never deleted. Sales and credit notes will point
-- at this row, and a row that disappears breaks history; the API therefore has
-- no DELETE path, and the foreign key added in 0014 is ON DELETE RESTRICT for
-- the same reason.
--
-- `tax_id` is optional but unique when present: a walk-in buyer is still worth
-- registering, and two customers may not share a document. The uniqueness is
-- on `upper(tax_id)` because a document has no meaningful case, and a
-- case-sensitive index would let the same taxpayer in twice by spelling it
-- differently.

CREATE TABLE customers (
    id         uuid        PRIMARY KEY,
    name       text        NOT NULL,
    tax_id     text,
    email      text,
    phone      text,
    is_active  boolean     NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT customers_name_not_blank CHECK (btrim(name) <> ''),
    -- Empty string and NULL must not be two ways of saying "no document": the
    -- partial unique index below would treat them differently.
    CONSTRAINT customers_tax_id_not_blank CHECK (tax_id IS NULL OR btrim(tax_id) <> ''),
    CONSTRAINT customers_email_not_blank CHECK (email IS NULL OR btrim(email) <> ''),
    CONSTRAINT customers_phone_not_blank CHECK (phone IS NULL OR btrim(phone) <> '')
);

-- Partial: any number of customers without a document, never two with the same
-- one. The application maps this index name to a 409, so it is part of the
-- contract and must not be renamed.
CREATE UNIQUE INDEX customers_tax_id_unique ON customers (upper(tax_id))
    WHERE tax_id IS NOT NULL;

-- Case-insensitive name lookups. A leading-wildcard `ILIKE '%x%'` cannot use a
-- btree index, and the search does exactly that; this index is here for
-- equality and prefix queries and for parity with `products`. A customer table
-- is small enough that the sequential scan is not the bottleneck.
CREATE INDEX customers_name_lower_idx ON customers (lower(name));
CREATE INDEX customers_active_idx ON customers (is_active);

CREATE TRIGGER customers_set_updated_at
    BEFORE UPDATE ON customers
    FOR EACH ROW
    EXECUTE FUNCTION set_updated_at();
