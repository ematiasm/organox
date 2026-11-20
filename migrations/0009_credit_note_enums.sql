-- Slice 9: the two ledger kinds a credit note needs.
--
-- Why this is a migration of its own: PostgreSQL refuses to use a new enum label
-- in the transaction that adds it ("unsafe use of new value"). `0010` compares
-- `sale_refund` inside CHECK constraints, so the label has to be committed
-- first.
--
-- Why a new kind instead of reusing `adjustment_out`: a refund must stay linked
-- to the sale it reverses and to the method that collected the money. An
-- adjustment is a manual correction with no sale behind it, and the ledger
-- constraint refuses a `sale_id` on it on purpose. The audit trail is the point.
--
-- Why a new stock reason instead of `adjustment`: returned goods are not a
-- correction of a miscount. `adjustment` would make "how much came back from
-- customers" impossible to answer without reading notes.

ALTER TYPE movement_reason ADD VALUE IF NOT EXISTS 'return';

ALTER TYPE wallet_movement_kind ADD VALUE IF NOT EXISTS 'sale_refund';
