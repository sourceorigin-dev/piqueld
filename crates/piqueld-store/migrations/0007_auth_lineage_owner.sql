-- Records which account owns each credential and passkey node of the issuance
-- tree, so revoking all of an account's credentials, or deleting it, can start
-- from its sessions and logins that already ended, whose rows are gone.
-- Existing nodes take the owner of their row; nodes whose row already went
-- keep no owner, as nothing recorded it.

-- The account that owned the credential or passkey; NULL for invitations,
-- which belong to their issuing credential's lineage instead.
ALTER TABLE auth_lineage ADD COLUMN user_id TEXT;
CREATE INDEX auth_lineage_user ON auth_lineage(user_id);

UPDATE auth_lineage SET user_id=(SELECT user_id FROM auth_credentials WHERE id=auth_lineage.id)
WHERE kind='credential';
UPDATE auth_lineage SET user_id=(SELECT user_id FROM auth_passkeys WHERE id=auth_lineage.id)
WHERE kind='passkey';
