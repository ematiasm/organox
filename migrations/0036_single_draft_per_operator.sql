-- Enforce at most one open draft per operator in sales and purchases.
--
-- Both `sales::open_draft` and `purchases::open_draft` reuse an operator's
-- existing draft instead of creating empty carts on reloads or double-clicks.
-- Without a partial unique index, concurrent `POST .../draft` requests race
-- past the check-then-insert and create duplicate open drafts.

CREATE UNIQUE INDEX sales_one_open_draft_per_user_idx
    ON sales (created_by)
    WHERE status = 'draft';

CREATE UNIQUE INDEX purchases_one_open_draft_per_user_idx
    ON purchases (created_by)
    WHERE status = 'draft';
