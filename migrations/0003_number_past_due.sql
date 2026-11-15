-- Monthly number rent is only charged when the balance covers it. When it doesn't, the number
-- is marked past due and released after a grace period, so unpaid numbers never cost the operator.
ALTER TABLE phone_numbers ADD COLUMN past_due_since timestamptz;
