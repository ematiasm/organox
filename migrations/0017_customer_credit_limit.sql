-- Customer accounts: the per-customer credit limit.
--
-- No backfill on purpose: the column default NULL is itself the answer, and
-- it means "no limit". Every customer that existed before this migration
-- keeps owing whatever the books already say, and inventing a number for
-- them would be the one change nobody asked for. `0` is the other sentinel
-- and means "cash only": the limit exists, and nothing may be owed.
--
-- The CHECK keeps a limit from being negative: a negative limit would
-- describe debt the books already carry as forbidden, and it is not a state
-- an operator can mean. It is `credit_limit >= 0` with the explicit NULL
-- arm because NULL is a valid value here, not "unknown".

ALTER TABLE customers ADD COLUMN credit_limit numeric(14, 2);

ALTER TABLE customers ADD CONSTRAINT customers_credit_limit_not_negative
    CHECK (credit_limit IS NULL OR credit_limit >= 0);
