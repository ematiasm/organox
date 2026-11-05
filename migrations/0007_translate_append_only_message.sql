-- Translate the append-only trigger message.
--
-- The message only ever reaches a developer typing an UPDATE/DELETE by hand in
-- psql: the application maps this failure to a generic 500, so no client sees
-- it. It is translated because English is now the default language for every
-- artifact, and because docs/catalog.md quotes this exact string.
--
-- Behavior is unchanged: the trigger still rejects every UPDATE and DELETE.

CREATE OR REPLACE FUNCTION stock_movements_are_append_only() RETURNS trigger AS $$
BEGIN
    RAISE EXCEPTION 'stock_movements is append-only: record an adjustment instead of rewriting history';
END;
$$ LANGUAGE plpgsql;
