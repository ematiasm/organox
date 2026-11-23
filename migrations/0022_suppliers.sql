-- Suppliers registry: the party a purchase points at.
--
-- A supplier is deactivated, never deleted. Purchases will point at this row
-- with ON DELETE RESTRICT so historical purchases stay intact; the API
-- therefore has no DELETE path.
--
-- `tax_id` is optional and unique when present, compared without case through
-- a partial unique index on `upper(tax_id)`.

CREATE TABLE suppliers (
    id         uuid        PRIMARY KEY,
    name       text        NOT NULL,
    tax_id     text,
    email      text,
    phone      text,
    is_active  boolean     NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),

    CONSTRAINT suppliers_name_not_blank CHECK (btrim(name) <> ''),
    CONSTRAINT suppliers_tax_id_not_blank CHECK (tax_id IS NULL OR btrim(tax_id) <> ''),
    CONSTRAINT suppliers_email_not_blank CHECK (email IS NULL OR btrim(email) <> ''),
    CONSTRAINT suppliers_phone_not_blank CHECK (phone IS NULL OR btrim(phone) <> '')
);

-- Partial: any number of suppliers without a document, never two with the same
-- one. The application maps this index name to a 409, so it is part of the
-- contract and must not be renamed.
CREATE UNIQUE INDEX suppliers_tax_id_unique ON suppliers (upper(tax_id))
    WHERE tax_id IS NOT NULL;

CREATE INDEX suppliers_name_lower_idx ON suppliers (lower(name));
CREATE INDEX suppliers_active_idx ON suppliers (is_active);

CREATE TRIGGER suppliers_set_updated_at
    BEFORE UPDATE ON suppliers
    FOR EACH ROW
    EXECUTE FUNCTION set_updated_at();
