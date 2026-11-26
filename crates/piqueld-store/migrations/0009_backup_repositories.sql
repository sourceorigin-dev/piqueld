-- Backup repositories: where piqueld writes backups, in restic's repository
-- format. Each row holds the repository's daemon key, encrypted with the
-- secret master key; lost-key recovery deletes the rows, never the backups,
-- which the recovery key shown at creation still opens. Adds a table only.
CREATE TABLE backup_repositories (
    id TEXT PRIMARY KEY NOT NULL,
    name TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL CHECK (kind IN ('local')),
    location TEXT NOT NULL UNIQUE,
    nonce BLOB NOT NULL,
    ciphertext BLOB NOT NULL,
    created_at_ms INTEGER NOT NULL,
    checked_at_ms INTEGER,
    check_problem TEXT,
    CHECK (check_problem IS NULL OR checked_at_ms IS NOT NULL)
) STRICT;
