use super::*;
use crate::{SCHEMA_VERSION, secrets::KeyFailure};
use std::os::unix::fs::PermissionsExt;

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
