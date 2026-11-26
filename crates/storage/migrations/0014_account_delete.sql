-- F040: full account deletion keeps the observation history as audit
-- trail. Migration 0008 already documented the intent ("an account row
-- may be deleted without rewriting history") but the hard foreign key
-- it shipped with contradicts that contract — dropping it lets
-- Store::delete_account remove the account row while its observation
-- records survive as the durable audit trail. The account.deleted
-- event appended atomically with the deletion is the machine-readable
-- audit marker; observations remain the human-readable history.

ALTER TABLE account_observations
    DROP CONSTRAINT IF EXISTS account_observations_account_id_fkey;
