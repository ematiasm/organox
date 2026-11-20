-- Slice 11: the money a credit note returns names the note that caused it.
--
-- The refund split is recorded on each wallet movement. Naming the note lets
-- the read endpoint show where the money actually went instead of re-deriving
-- a split that may no longer match the ledger. A refund without a note has no
-- explanation; no other kind may pretend to be one.

ALTER TABLE wallet_movements
    ADD COLUMN credit_note_id uuid REFERENCES credit_notes (id) ON DELETE RESTRICT;

ALTER TABLE wallet_movements
    ADD CONSTRAINT wallet_movements_sale_refund_names_its_note CHECK (
        kind <> 'sale_refund' OR credit_note_id IS NOT NULL
    );

ALTER TABLE wallet_movements
    ADD CONSTRAINT wallet_movements_credit_note_only_for_sale_refunds CHECK (
        kind = 'sale_refund' OR credit_note_id IS NULL
    );

CREATE INDEX wallet_movements_credit_note_idx ON wallet_movements (credit_note_id)
    WHERE credit_note_id IS NOT NULL;
