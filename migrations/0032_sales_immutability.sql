-- Closed sales (`completed` or `cancelled`) and their items are immutable facts.
--
-- Drafts remain editable until they transition to `completed` or `cancelled`.
-- Once closed, any UPDATE or DELETE on `sales`, or any INSERT, UPDATE, or DELETE
-- on `sale_items` referencing a closed sale, is rejected at the database layer.

CREATE OR REPLACE FUNCTION sales_closed_are_immutable() RETURNS trigger AS $$
BEGIN
    IF OLD.status <> 'draft' THEN
        RAISE EXCEPTION 'closed sales are immutable: cannot % sale % with status %',
            TG_OP, OLD.id, OLD.status;
    END IF;

    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER sales_prevent_closed_mutation
    BEFORE UPDATE OR DELETE ON sales
    FOR EACH ROW
    EXECUTE FUNCTION sales_closed_are_immutable();

-- Lock the parent `sales` row `FOR SHARE` so even a direct `sale_items` write
-- racing with completion (`FOR UPDATE`) or cancellation (`FOR NO KEY UPDATE`)
-- serializes against the closing transaction and sees the committed status.
CREATE OR REPLACE FUNCTION sale_items_closed_sale_are_immutable() RETURNS trigger AS $$
DECLARE
    v_status sale_status;
BEGIN
    IF TG_OP IN ('UPDATE', 'DELETE') THEN
        SELECT status INTO v_status
        FROM sales
        WHERE id = OLD.sale_id
        FOR SHARE;

        IF FOUND AND v_status <> 'draft' THEN
            RAISE EXCEPTION 'sale_items of closed sale % (status: %) are immutable: cannot %',
                OLD.sale_id, v_status, TG_OP;
        END IF;
    END IF;

    IF TG_OP = 'INSERT' OR (TG_OP = 'UPDATE' AND NEW.sale_id IS DISTINCT FROM OLD.sale_id) THEN
        SELECT status INTO v_status
        FROM sales
        WHERE id = NEW.sale_id
        FOR SHARE;

        IF FOUND AND v_status <> 'draft' THEN
            RAISE EXCEPTION 'sale_items of closed sale % (status: %) are immutable: cannot %',
                NEW.sale_id, v_status, TG_OP;
        END IF;
    END IF;

    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER sale_items_prevent_closed_mutation
    BEFORE INSERT OR UPDATE OR DELETE ON sale_items
    FOR EACH ROW
    EXECUTE FUNCTION sale_items_closed_sale_are_immutable();
