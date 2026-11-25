use super::*;
use crate::{SCHEMA_VERSION, secrets::KeyFailure};
use piqueld_core::backup::{
    BackupRepositoryName, RepositoryCheck, RepositoryLocation, RepositoryPath,
};
use std::os::unix::fs::PermissionsExt;
use zeroize::Zeroizing;

/// Snapshots the store's database in `directory` into `name` and opens
/// the copy.
async fn snapshot(directory: &Path, name: &str) -> DatabaseCopy {
    let copy = directory.join(name);
    let mut live = DatabaseFile::open(&directory.join("db"))
        .await
        .expect("the live database opens read-only");
    live.snapshot_into(&copy)
        .await
        .expect("the snapshot is written");
    live.close().await.expect("the live database closes");
    DatabaseCopy::open(&copy).await.expect("the snapshot opens")
}

/// Whether the table `name` exists in `database`.
async fn has_table(database: &mut DatabaseFile, name: &str) -> bool {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sqlite_master WHERE name=?1")
        .bind(name)
        .fetch_one(&mut database.0)
        .await
        .expect("the schema is readable")
        == 1
}

/// A snapshot taken while the daemon reads and writes holds every commit,
/// including those only in the write-ahead log, and no uncommitted write.
#[tokio::test]
async fn snapshots_hold_exactly_the_committed_state_of_a_live_database() {
    let (directory, store) = Store::temporary().await;
    let mut reader = store.snapshot().await.expect("a reader begins");
    sqlx::query("SELECT COUNT(*) FROM instance_metadata")
        .execute(&mut *reader)
        .await
        .expect("the reader reads, pinning the log");
    sqlx::query("CREATE TABLE committed(id INTEGER)")
        .execute(&store.pool)
        .await
        .expect("a write commits");
    let (_writer, mut writer) = store.begin_immediate().await.expect("a writer begins");
    sqlx::query("CREATE TABLE uncommitted(id INTEGER)")
        .execute(&mut *writer)
        .await
        .expect("a write stays uncommitted");

    let mut copy = snapshot(directory.path(), "copy.db").await;
    assert!(has_table(&mut copy, "committed").await);
    assert!(!has_table(&mut copy, "uncommitted").await);
    assert_eq!(
        copy.identity().await.expect("the copy has an identity"),
        Some(DatabaseIdentity {
            schema_version: SCHEMA_VERSION,
            instance_id: store.instance_id().clone(),
        })
    );
    assert_eq!(
        copy.integrity_problems().await.expect("the check runs"),
        Vec::<String>::new()
    );
}

/// A copy whose metadata disagrees with its `user_version` is refused, as
/// opening it as a store would be.
#[tokio::test]
async fn identity_refuses_a_schema_version_the_metadata_contradicts() {
    let (directory, _store) = Store::temporary().await;
    drop(snapshot(directory.path(), "copy.db").await);
    let copy = directory.path().join("copy.db");
    let pool = sqlx::SqlitePool::connect_with(SqliteConnectOptions::new().filename(&copy))
        .await
        .expect("the copy opens for writing");
    sqlx::query("UPDATE instance_metadata SET schema_version=schema_version-1")
        .execute(&pool)
        .await
        .expect("the metadata changes");
    pool.close().await;
    let mut copy = DatabaseFile::open(&copy).await.expect("the copy opens");
    assert!(matches!(
        copy.identity().await,
        Err(StoreError::SchemaMismatch)
    ));
}

/// A snapshot accepts only the key its verifier was made with, and any key
/// or none before a secret is stored.
#[tokio::test]
async fn verify_key_accepts_only_the_key_the_snapshot_was_bound_to() {
    let (directory, store) = Store::temporary().await;
    let key = directory.path().join("secrets.key");
    let mut unbound = snapshot(directory.path(), "unbound.db").await;
    unbound
        .verify_key(None)
        .await
        .expect("no key is needed yet");

    let (_writer, mut tx) = store.begin_immediate().await.expect("a writer begins");
    store
        .verified_secret_cipher(&mut tx)
        .await
        .expect("the first write binds a new key");
    tx.commit().await.expect("the verifier commits");
    let mut bound = snapshot(directory.path(), "bound.db").await;
    bound
        .verify_key(Some(&key))
        .await
        .expect("the bound key matches");

    let failure = async |database: &mut DatabaseFile, key: Option<&Path>| match database
        .verify_key(key)
        .await
    {
        Err(StoreError::SecretSource(error)) => error.key_failure(),
        other => panic!("expected a key failure, got {other:?}"),
    };
    assert!(matches!(
        failure(&mut bound, None).await,
        Some(KeyFailure::Missing)
    ));
    let other = directory.path().join("other.key");
    std::fs::write(&other, [42; 32]).expect("another key is written");
    std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o600))
        .expect("the other key is private");
    assert!(matches!(
        failure(&mut bound, Some(&other)).await,
        Some(KeyFailure::AuthenticationFailed)
    ));
}

/// `VACUUM INTO` copies a row that breaks its CHECK constraint, and the
/// copy's integrity check finds it, which a read-only connection's skips.
#[tokio::test]
async fn integrity_problems_include_rows_that_break_their_constraints() {
    let (directory, store) = Store::temporary().await;
    let mut connection = store.pool.acquire().await.expect("a connection opens");
    sqlx::raw_sql(
        "CREATE TABLE checked(value TEXT CHECK (json_valid(value)));
         PRAGMA ignore_check_constraints=ON;
         INSERT INTO checked VALUES('{not json');
         PRAGMA ignore_check_constraints=OFF;",
    )
    .execute(&mut *connection)
    .await
    .expect("the broken row is written");
    drop(connection);

    let mut copy = snapshot(directory.path(), "copy.db").await;
    let problems = copy.integrity_problems().await.expect("the check runs");
    assert_eq!(problems, ["CHECK constraint failed in checked"]);
}

/// The statements [`DatabaseFile::identity`] has read a database's identity
/// with since `piqueld roll-back` first shipped. An older binary rolling back
/// runs them on a newer database, so every migration must keep them working:
/// change them only together with a migration that keeps the old ones too.
const IDENTITY_STATEMENTS: [&str; 3] = [
    "PRAGMA user_version",
    "SELECT schema_version FROM instance_metadata WHERE singleton=1",
    "SELECT instance_id FROM instance_metadata WHERE singleton=1",
];

/// A database at the latest schema still answers the statements older
/// binaries read its identity with.
#[tokio::test]
async fn older_binaries_read_the_identity_of_the_latest_schema() {
    let (directory, _store) = Store::temporary().await;
    let mut database = snapshot(directory.path(), "copy.db").await;
    for statement in IDENTITY_STATEMENTS {
        let row = sqlx::query(statement)
            .fetch_optional(&mut database.0.0)
            .await
            .unwrap_or_else(|error| panic!("{statement} fails: {error}"));
        assert!(row.is_some(), "{statement} returns nothing");
    }
}

/// The repository `name` at the local directory `path`.
fn local(name: &str, path: &str) -> (BackupRepositoryName, RepositoryLocation) {
    let name = BackupRepositoryName::parse(name).expect("the name is valid");
    let path = RepositoryPath::parse(path).expect("the path is valid");
    (name, RepositoryLocation::Local { path })
}

/// A repository's key is stored encrypted and read back whole; a second
/// repository can't take its name or location; checks and removal are
/// recorded, and the audit trail never holds the key.
#[tokio::test]
async fn backup_repositories_keep_their_key_and_their_name_and_location_unique() {
    let (_directory, store) = Store::temporary().await;
    let route = "POST /api/v1/backup/repositories";
    let request = crate::testing::audit_request(route);
    let author = crate::Author {
        actor: crate::Actor::Daemon,
        request: Some(&request),
    };
    let (name, location) = local("offsite", "/srv/backups");
    let key = Zeroizing::new("daemon-key".to_owned());
    let added = store
        .add_backup_repository(author, &name, &location, &key)
        .await
        .expect("the repository is added");
    let id: String = sqlx::query_scalar("SELECT id FROM backup_repositories")
        .fetch_one(&store.pool)
        .await
        .expect("the repository has an ID");
    let ciphertext: Vec<u8> = sqlx::query_scalar("SELECT ciphertext FROM backup_repositories")
        .fetch_one(&store.pool)
        .await
        .expect("the key is stored");
    assert!(
        !ciphertext
            .windows(key.len())
            .any(|bytes| bytes == key.as_bytes())
    );
    let (read, read_key) = store
        .backup_repository_key(&name)
        .await
        .expect("the key decrypts");
    assert_eq!((read, read_key.as_str()), (added.clone(), "daemon-key"));

    let (other, elsewhere) = local("other", "/srv/elsewhere");
    let taken = store.add_backup_repository(author, &name, &elsewhere, &key);
    assert!(matches!(
        taken.await,
        Err(StoreError::Repository(RepositoryError::NameTaken(_)))
    ));
    let taken = store.check_new_backup_repository(&other, &location);
    assert!(matches!(
        taken.await,
        Err(StoreError::Repository(RepositoryError::LocationTaken(taken))) if taken == name
    ));

    let check = RepositoryCheck {
        checked_at_ms: 7,
        problem: Some("pack abc is missing".to_owned()),
    };
    store
        .record_backup_repository_check(author, &name, &check)
        .await
        .expect("the check is recorded");
    let listed = store
        .backup_repositories()
        .await
        .expect("repositories are listed");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].last_check, Some(check));
    store
        .delete_backup_repository(author, &name)
        .await
        .expect("the repository is removed");
    assert!(matches!(
        store.backup_repository(&name).await,
        Err(StoreError::NotFound)
    ));
    assert_eq!(
        store.audit_actions().await,
        [
            format!("{route} backup repository \"offsite\" {id} created at local \"/srv/backups\""),
            format!("{route} backup repository \"offsite\" {id} check failed"),
            format!("{route} backup repository \"offsite\" {id} removed"),
        ]
    );
}

/// Recovering a lost master key removes the repositories whose keys it
/// encrypted, and counts them.
#[tokio::test]
async fn key_recovery_removes_backup_repositories() {
    let (directory, store) = Store::temporary().await;
    let (name, location) = local("offsite", "/srv/backups");
    let key = Zeroizing::new("daemon-key".to_owned());
    store
        .add_backup_repository(crate::Actor::Daemon, &name, &location, &key)
        .await
        .expect("the repository is added");
    std::fs::write(directory.path().join("secrets.key"), [42; 32])
        .expect("the lost key is written");
    let recovery = store
        .recover_secret_key(crate::Actor::Daemon)
        .await
        .expect("the lost key is recovered");
    assert_eq!(recovery.deleted_backup_repositories, 1);
    assert_eq!(
        store
            .backup_repositories()
            .await
            .expect("repositories are listed"),
        []
    );
}
