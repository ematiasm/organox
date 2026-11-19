-- Slice 12: a retried create must not issue a second note for the same return.
--
-- A caller that lost the response can resend the request with a key. The
-- partial unique index makes each key unique within a sale while leaving
-- ordinary notes keyless, so a concurrent retry either finds the first note
-- or loses the insert and returns that note instead of writing a duplicate.

ALTER TABLE credit_notes
    ADD COLUMN idempotency_key text,
    ADD CONSTRAINT credit_notes_idempotency_key_not_blank
        CHECK (idempotency_key IS NULL OR btrim(idempotency_key) <> '');

CREATE UNIQUE INDEX credit_notes_sale_idempotency_key_unique
    ON credit_notes (sale_id, idempotency_key)
    WHERE idempotency_key IS NOT NULL;
