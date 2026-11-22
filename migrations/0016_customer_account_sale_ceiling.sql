-- Customer accounts: the per-sale allocation ceiling, at the engine level.
--
-- Why a trigger for a rule the Rust guards already enforce: every other
-- cross-table rule in `0015` got one, and a guard that lives only in the
-- writers is a guard any raw row can route around. The trigger recomputes
-- the outstanding from the ledger itself — charge minus what the notes
-- cancelled — inside the transaction that inserts the allocation, so the
-- ceiling holds no matter which code path wrote the row.
--
-- Why deferred, like the receipt ceiling next to it: one receipt's
-- allocations arrive as a batch, and the sums are only meaningful once the
-- whole batch is in.

CREATE OR REPLACE FUNCTION customer_account_allocations_fit_the_sale() RETURNS trigger AS $$
DECLARE
    charged   numeric(14, 2);
    cancelled numeric(14, 2);
    allocated numeric(14, 2);
BEGIN
    SELECT COALESCE(SUM(amount), 0) INTO charged
        FROM customer_account_entries
        WHERE sale_id = NEW.sale_id AND kind = 'sale_charge';
    SELECT COALESCE(-SUM(amount), 0) INTO cancelled
        FROM customer_account_entries
        WHERE sale_id = NEW.sale_id AND kind = 'credit_note';
    SELECT COALESCE(SUM(amount), 0) INTO allocated
        FROM customer_account_allocations
        WHERE sale_id = NEW.sale_id;

    IF allocated > charged - cancelled THEN
        RAISE EXCEPTION 'allocations (%) exceed the outstanding (%) of the sale', allocated, charged - cancelled;
    END IF;

    RETURN NULL;
END;
$$ LANGUAGE plpgsql;

CREATE CONSTRAINT TRIGGER customer_account_allocations_fit_the_sale
    AFTER INSERT ON customer_account_allocations
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW
    EXECUTE FUNCTION customer_account_allocations_fit_the_sale();
