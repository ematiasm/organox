-- Rename the operator role from `encargado` to `manager`.
--
-- The enum label is part of the API contract (`Role` serializes to it) and of
-- the CLI (`create-user --role`). English is now the default language for every
-- artifact in this project, including identifiers a human reads, so the label
-- follows. A rename keeps existing rows: no data is rewritten, no column is
-- touched.
--
-- Migrations already applied are immutable, so this is a new migration instead
-- of an edit to 0002_auth.sql.

ALTER TYPE user_role RENAME VALUE 'encargado' TO 'manager';
