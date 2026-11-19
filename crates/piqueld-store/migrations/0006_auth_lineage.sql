-- Records which credential, invitation or passkey issued each one, so revoking
-- one can find everything issued through it. Who issued existing ones was
-- never recorded, so they become roots.

-- The issuance tree. A node outlives what it names, as long as something
-- still live descends from it: a session that logged out, expired or was
-- pruned, or an invitation that was redeemed, still links what it issued to
-- its own issuer. Pruning removes nodes nothing live needs. Nothing checks a
-- node to authenticate; it only finds what to revoke.
--
-- `kind` and `id` name the row: a credential in `auth_credentials`, an
-- invitation or enrollment link in `auth_invitations`, or a passkey in
-- `auth_passkeys`. Passkey IDs come from authenticators, so they share no
-- namespace with the others. `issued_by` is NULL for roots: what the setup
-- secret, an admin recovery link, or anything before this migration created.
CREATE TABLE auth_lineage (
    node INTEGER PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN ('credential','invitation','passkey')),
    id TEXT NOT NULL,
    issued_by INTEGER REFERENCES auth_lineage(node),
    UNIQUE (kind, id)
);
CREATE INDEX auth_lineage_issued_by ON auth_lineage(issued_by);

INSERT INTO auth_lineage(kind,id) SELECT 'credential', id FROM auth_credentials;
INSERT INTO auth_lineage(kind,id) SELECT 'invitation', id FROM auth_invitations;
INSERT INTO auth_lineage(kind,id) SELECT 'passkey', id FROM auth_passkeys;
