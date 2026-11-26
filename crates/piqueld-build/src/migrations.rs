//! Checks of the store's SQL migrations, which its build script runs: the
//! lock file holding each migration's checksum, and the rule that new
//! tables are `STRICT`.

use sha2::{Digest, Sha256};

/// The lock file, next to the migrations: one `NNNN <sha256>` line per
/// migration, so editing one shows in review as a changed lock line.
pub const LOCK: &str = "migrations/checksums";

/// One migration file.
pub struct Migration<'a> {
    /// Its number, the schema version it upgrades to.
    pub version: u64,
    /// Its file name, such as `0001_schema.sql`.
    pub name: &'a str,
    /// Its SQL.
    pub sql: &'a str,
}

/// Why the migrations fail a check, saying how to fix it.
#[derive(PartialEq, Eq, thiserror::Error)]
pub enum MigrationError {
    /// A migration has no line in the lock file.
    #[error("{name} has no checksum in {LOCK}; add the line `{version:04} {checksum}`")]
    Unlocked {
        /// The migration's file name.
        name: String,
        /// Its number.
        version: u64,
        /// Its SHA-256.
        checksum: String,
    },
    /// A migration differs from its locked checksum.
    #[error(
        "{name} changed: {LOCK} locks {locked}, but it is now {checksum}. A released migration never changes, since databases that applied it won't run it again: revert it and add a new migration. If it isn't released yet, update its line to `{version:04} {checksum}`"
    )]
    Changed {
        /// The migration's file name.
        name: String,
        /// Its number.
        version: u64,
        /// The checksum the lock file holds.
        locked: String,
        /// Its SHA-256 now.
        checksum: String,
    },
    /// The lock file names a migration that doesn't exist.
    #[error("{LOCK} locks migration {version:04}, which doesn't exist; remove its line")]
    Missing {
        /// The locked number.
        version: u64,
    },
    /// A lock file line isn't `NNNN <sha256>`.
    #[error("{LOCK} line {line} is not `NNNN <sha256>`: {text}")]
    Malformed {
        /// Its 1-based line number.
        line: usize,
        /// The line.
        text: String,
    },
    /// A new migration creates a table that isn't `STRICT`.
    #[error(
        "{name} creates table {table} without STRICT; end its definition with `) STRICT;`, as every table migration {first_strict:04} or later creates must be"
    )]
    NotStrict {
        /// The migration's file name.
        name: String,
        /// The table.
        table: String,
        /// The first migration the rule applies to.
        first_strict: u64,
    },
}

/// Shows the message, as a build script's `main` prints the error it returns
/// with `Debug`.
impl std::fmt::Debug for MigrationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self}")
    }
}

impl Migration<'_> {
    /// The SHA-256 of its SQL, in lowercase hex.
    #[must_use]
    pub fn checksum(&self) -> String {
        format!("{:x}", Sha256::digest(self.sql.as_bytes()))
    }

    /// Checks `migrations` against `lock`, the lock file's contents: each
    /// must have its checksum there, and the file names no other. Lines
    /// starting with `#` are comments.
    ///
    /// # Errors
    /// Returns the first [`MigrationError`] found.
    pub fn check_lock(migrations: &[Self], lock: &str) -> Result<(), MigrationError> {
        let mut locked = Vec::new();
        for (index, text) in lock.lines().enumerate() {
            if text.trim().is_empty() || text.starts_with('#') {
                continue;
            }
            let entry = text
                .split_once(' ')
                .and_then(|(version, checksum)| Some((version.parse::<u64>().ok()?, checksum)));
            let Some(entry) = entry else {
                return Err(MigrationError::Malformed {
                    line: index + 1,
                    text: text.to_owned(),
                });
            };
            locked.push(entry);
        }
        for migration in migrations {
            let checksum = migration.checksum();
            let (name, version) = (migration.name.to_owned(), migration.version);
            match locked.iter().find(|(locked, _)| *locked == version) {
                None => {
                    return Err(MigrationError::Unlocked {
                        name,
                        version,
                        checksum,
                    });
                }
                Some((_, locked)) if *locked != checksum => {
                    let locked = (*locked).to_owned();
                    return Err(MigrationError::Changed {
                        name,
                        version,
                        locked,
                        checksum,
                    });
                }
                Some(_) => {}
            }
        }
        match locked
            .iter()
            .find(|(version, _)| !migrations.iter().any(|m| m.version == *version))
        {
            Some((version, _)) => Err(MigrationError::Missing { version: *version }),
            None => Ok(()),
        }
    }

    /// Checks that every table migrations from `first_strict` on create is
    /// `STRICT`. Earlier ones predate the rule.
    ///
    /// # Errors
    /// Returns [`MigrationError::NotStrict`] for the first that isn't.
    pub fn check_strict(migrations: &[Self], first_strict: u64) -> Result<(), MigrationError> {
        for migration in migrations.iter().filter(|m| m.version >= first_strict) {
            if let Some(table) = migration.loose_tables().into_iter().next() {
                return Err(MigrationError::NotStrict {
                    name: migration.name.to_owned(),
                    table,
                    first_strict,
                });
            }
        }
        Ok(())
    }

    /// The tables its `CREATE TABLE` statements create without `STRICT`:
    /// the table options after a definition's last `)` don't name it.
    fn loose_tables(&self) -> Vec<String> {
        let code = code(self.sql);
        code.split(';')
            .filter_map(|statement| {
                let mut words = statement.split_whitespace().map(str::to_ascii_uppercase);
                let create = words.next().as_deref() == Some("CREATE");
                let mut word = words.next()?;
                if matches!(word.as_str(), "TEMP" | "TEMPORARY") {
                    word = words.next()?;
                }
                if !create || word != "TABLE" {
                    return None;
                }
                let (_, options) = statement.rsplit_once(')')?;
                let strict = options
                    .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .any(|option| option.eq_ignore_ascii_case("STRICT"));
                (!strict).then(|| table_name(statement))
            })
            .collect()
    }
}

/// `sql` without comments, and with single-quoted text blanked out, so
/// keywords, parentheses and `;` inside them don't count.
fn code(sql: &str) -> String {
    let mut code = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                code.push(c);
                for quoted in chars.by_ref() {
                    if quoted == '\'' {
                        break;
                    }
                    code.push(' ');
                }
                code.push(c);
            }
            '-' if chars.peek() == Some(&'-') => {
                chars.by_ref().find(|&c| c == '\n');
                code.push('\n');
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut previous = ' ';
                chars.by_ref().find(|&c| {
                    let end = previous == '*' && c == '/';
                    previous = c;
                    end
                });
                code.push(' ');
            }
            c => code.push(c),
        }
    }
    code
}

/// The table a `CREATE TABLE` `statement` names, after any `IF NOT EXISTS`.
fn table_name(statement: &str) -> String {
    let words: Vec<&str> = statement
        .split(|c: char| c.is_whitespace() || c == '(')
        .filter(|word| !word.is_empty())
        .collect();
    let table = words
        .iter()
        .position(|word| word.eq_ignore_ascii_case("TABLE"));
    let mut rest = words[table.map_or(words.len(), |table| table + 1)..].iter();
    let mut name = rest.next();
    if name.is_some_and(|word| word.eq_ignore_ascii_case("IF")) {
        name = rest.nth(2);
    }
    name.map_or_else(String::new, |name| (*name).to_owned())
}

#[cfg(test)]
mod tests {
    use super::{Migration, MigrationError};

    #[test]
    fn the_lock_refuses_unlocked_changed_and_missing_migrations() {
        let name = "0001_a.sql";
        let migrations = [Migration {
            version: 1,
            name,
            sql: "CREATE TABLE a(id INTEGER);",
        }];
        let checksum = migrations[0].checksum();
        let lock = format!("# Locked.\n0001 {checksum}\n");
        assert_eq!(Migration::check_lock(&migrations, &lock), Ok(()));
        assert!(matches!(
            Migration::check_lock(&migrations, "# Locked.\n"),
            Err(MigrationError::Unlocked { version: 1, .. })
        ));
        let edited = [Migration {
            version: 1,
            name,
            sql: "CREATE TABLE a(id TEXT);",
        }];
        assert!(matches!(
            Migration::check_lock(&edited, &lock),
            Err(MigrationError::Changed { version: 1, locked, .. }) if locked == checksum
        ));
        assert_eq!(
            Migration::check_lock(&[], &lock),
            Err(MigrationError::Missing { version: 1 })
        );
    }

    #[test]
    fn tables_from_the_first_strict_migration_on_must_be_strict() {
        let migrations = [
            Migration {
                version: 1,
                name: "0001_a.sql",
                sql: "CREATE TABLE old(id INTEGER);",
            },
            Migration {
                version: 2,
                name: "0002_b.sql",
                sql: "-- CREATE TABLE commented(id INTEGER);
                    CREATE TABLE IF NOT EXISTS kept(kind TEXT CHECK(kind IN ('a)', 'b;'))) strict;
                    CREATE TRIGGER t AFTER INSERT ON kept BEGIN DELETE FROM kept; END;
                    CREATE TABLE loose(id INTEGER) WITHOUT ROWID;",
            },
        ];
        assert_eq!(
            Migration::check_strict(&migrations, 2),
            Err(MigrationError::NotStrict {
                name: "0002_b.sql".into(),
                table: "loose".into(),
                first_strict: 2,
            })
        );
    }
}
