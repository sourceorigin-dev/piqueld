-- Records when a passkey last proved a credential's holder present: a passkey
-- sign-in, or a step-up confirming an existing session. Sensitive actions,
-- such as creating credentials, need this to be recent. It is not when the
-- credential was created or last used: neither proves a passkey was used.
-- API tokens and existing credentials have none, so they start out stale.
ALTER TABLE auth_credentials ADD COLUMN authenticated_at INTEGER;
