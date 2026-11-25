use super::*;
use piqueld_store::Store;
use std::os::unix::fs::PermissionsExt;

/// A data directory whose database an older daemon left at the schema
/// before this binary's, with a stored secret, so its key is bound to it,
/// and ingress and tailnet state. Returns its instance ID.
async fn outdated(data_dir: &Path) -> String {
    let store = Store::open_unsynced(data_dir.join(DATABASE))
        .await
        .expect("the store opens");
    let app = piqueld_core::manifest::parse_template_toml(
        "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='notes'\n\
         [[spec.services]]\nname='web'\n[spec.services.source]\ntype='image'\nimage='nginx'",
    )
    .expect("the manifest parses")
    .normalize(piqueld_core::ApplicationId::parse("app-notes-01").expect("the ID is valid"));
    store
        .save_application(&app, None, None)
        .await
        .expect("the application is saved");
    store
        .put_stored_secret(
            piqueld_store::Actor::Daemon,
            app.id(),
            "token",
            0,
            b"value".to_vec(),
            None,
        )
        .await
        .expect("the secret is stored, binding a new key");
    let instance = store.instance_id().as_str().to_owned();
    drop(store);
    let options = sqlx::sqlite::SqliteConnectOptions::new().filename(data_dir.join(DATABASE));
    let pool = sqlx::SqlitePool::connect_with(options)
        .await
        .expect("the database opens");
    let older = SCHEMA_VERSION - 1;
    sqlx::query(&format!("PRAGMA user_version = {older}"))
        .execute(&pool)
        .await
        .expect("the version changes");
    sqlx::query("UPDATE instance_metadata SET schema_version=?1")
        .bind(i64::try_from(older).expect("the version fits"))
        .execute(&pool)
        .await
        .expect("the recorded version changes");
    pool.close().await;

    let write = |path: &str, contents: &[u8]| {
        let path = data_dir.join(path);
        fs::create_dir_all(path.parent().expect("the file has a parent"))
            .expect("its directory is created");
        fs::write(&path, contents).expect("the file is written");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("the file is private");
    };
    write("tailscale/tailscaled.state", b"node");
    write("ingress/data/caddy/certificates/cert.pem", b"cert");
    write("ingress/control/state", b"runtime");
    write("snapshots/preview.bundle", b"bundle");
    std::os::unix::net::UnixListener::bind(data_dir.join("tailscale/tailscaled.sock"))
        .expect("a socket is created");
    std::os::unix::fs::symlink(
        data_dir.join("snapshots"),
        data_dir.join("ingress/data/elsewhere"),
    )
    .expect("a symlink is created");
    instance
}

/// Runs `sql` on the database in `data_dir`, or a backup, without checking
/// constraints,
/// as damage outside the daemon could leave it.
async fn tamper(data_dir: &Path, sql: &str) {
    let options = sqlx::sqlite::SqliteConnectOptions::new().filename(data_dir.join(DATABASE));
    let mut connection = sqlx::ConnectOptions::connect(&options)
        .await
        .expect("the database opens");
    sqlx::raw_sql(&format!("PRAGMA ignore_check_constraints=ON; {sql}"))
        .execute(&mut connection)
        .await
        .expect("the database changes");
}

/// The backups' names in `data_dir`, sorted, unfinished ones included.
fn backup_names(data_dir: &Path) -> Vec<String> {
    let mut names: Vec<_> = fs::read_dir(data_dir.join(BACKUPS))
        .expect("the backups directory reads")
        .map(|entry| {
            entry
                .expect("the entry reads")
                .file_name()
                .into_string()
                .expect("UTF-8")
        })
        .collect();
    names.sort();
    names
}

/// An outdated data directory, its backup before migrating, and its
/// instance ID.
async fn backed_up() -> (tempfile::TempDir, PathBuf, String) {
    let data_dir = tempfile::tempdir().expect("a data directory is created");
    let instance = outdated(data_dir.path()).await;
    let backup = Backups::new(data_dir.path())
        .before_migration()
        .await
        .expect("the backup is written")
        .expect("an outdated database is backed up");
    (data_dir, backup, instance)
}

/// The relative paths of the files under `root`, sorted.
fn files(root: &Path) -> Vec<String> {
    fn walk(root: &Path, directory: &Path, found: &mut Vec<String>) {
        for entry in fs::read_dir(directory).expect("the directory reads") {
            let path = entry.expect("the entry reads").path();
            if path.is_dir() {
                walk(root, &path, found);
            } else {
                let relative = path.strip_prefix(root).expect("the path is under the root");
                found.push(relative.display().to_string());
            }
        }
    }
    let mut found = Vec::new();
    walk(root, root, &mut found);
    found.sort();
    found
}

/// Before a migration, the daemon's state is backed up without runtime
/// sockets, local-preview bundles or earlier backups, and the database and
/// key in the backup are a matching pair.
#[tokio::test]
async fn an_outdated_database_is_backed_up_before_it_migrates() {
    let (data_dir, backup, instance) = backed_up().await;
    let backups = Backups::new(data_dir.path());

    assert_eq!(
        files(&backup),
        [
            MANIFEST,
            "ingress/data/caddy/certificates/cert.pem",
            DATABASE,
            SECRET_KEY,
            "tailscale/tailscaled.state",
        ]
    );
    let manifest: Manifest =
        serde_json::from_slice(&fs::read(backup.join(MANIFEST)).expect("the manifest reads"))
            .expect("the manifest parses");
    assert_eq!(manifest.schema_version, SCHEMA_VERSION - 1);
    assert_eq!(manifest.instance_id, instance);
    assert_eq!(
        backups.latest_before_migration().expect("the backups list"),
        Some((backup.clone(), PreMigration::from(&manifest)))
    );
    let mode = |path: &Path| fs::metadata(path).expect("it exists").permissions().mode() & 0o777;
    assert_eq!(mode(&backup.join(SECRET_KEY)), 0o600);
    assert_eq!(mode(&backup.join("ingress")), 0o700);
    let mut database = DatabaseFile::open(&backup.join(DATABASE))
        .await
        .expect("the backup's database opens");
    database
        .verify_key(Some(&backup.join(SECRET_KEY)))
        .await
        .expect("the backup's key matches its database");
}

/// A database this binary already migrated, or none yet, needs no backup.
#[tokio::test]
async fn a_current_database_is_not_backed_up() {
    let data_dir = tempfile::tempdir().expect("a data directory is created");
    let backups = Backups::new(data_dir.path());
    assert_eq!(
        backups.before_migration().await.expect("nothing fails"),
        None
    );
    drop(
        Store::open_unsynced(data_dir.path().join(DATABASE))
            .await
            .expect("the store opens"),
    );
    assert_eq!(
        backups.before_migration().await.expect("nothing fails"),
        None
    );
    assert!(!data_dir.path().join(BACKUPS).exists());
}

/// Pruning keeps the newest backups by their generated names, deletes
/// unfinished ones, and leaves everything else alone.
#[test]
fn prune_keeps_the_newest_backups_with_generated_names() {
    let directory = tempfile::tempdir().expect("a directory is created");
    for name in [
        "pre-9-100",
        "pre-10-300",
        "pre-10-200",
        "pre-10-50",
        "pre-manual-1",
        ".partial-abc",
    ] {
        fs::create_dir(directory.path().join(name)).expect("the backup is created");
    }
    PreMigration::prune(directory.path(), 2).expect("pruning succeeds");
    let mut remaining: Vec<_> = fs::read_dir(directory.path())
        .expect("the directory reads")
        .map(|entry| {
            entry
                .expect("the entry reads")
                .file_name()
                .into_string()
                .expect("UTF-8")
        })
        .collect();
    remaining.sort();
    assert_eq!(remaining, ["pre-10-200", "pre-10-300", "pre-manual-1"]);
}

/// A snapshot that fails its integrity check, here a row VACUUM copied
/// past its CHECK constraint, is never published, so it can't replace a
/// backup that restores.
#[tokio::test]
async fn a_damaged_database_is_never_backed_up() {
    let (data_dir, backup, _) = backed_up().await;
    tamper(
        data_dir.path(),
        "UPDATE applications SET desired_json='{not json'",
    )
    .await;
    let error = Backups::new(data_dir.path())
        .before_migration()
        .await
        .expect_err("the damaged snapshot is refused");
    assert!(error.is::<Damaged>(), "{error:#}");
    let kept = backup.file_name().expect("the backup has a name");
    assert_eq!(
        backup_names(data_dir.path()),
        [kept.to_str().expect("UTF-8")]
    );
}

/// A state directory replaced by a symlink is refused rather than followed.
#[tokio::test]
async fn a_symlinked_state_directory_is_not_followed() {
    let data_dir = tempfile::tempdir().expect("a data directory is created");
    outdated(data_dir.path()).await;
    let tailscale = data_dir.path().join("tailscale");
    fs::remove_dir_all(&tailscale).expect("the tailnet state is removed");
    std::os::unix::fs::symlink(data_dir.path().join("snapshots"), &tailscale)
        .expect("a symlink replaces it");
    let error = Backups::new(data_dir.path())
        .before_migration()
        .await
        .expect_err("the symlink is refused");
    assert!(format!("{error:#}").contains("tailscale"), "{error:#}");
    assert_eq!(backup_names(data_dir.path()), Vec::<String>::new());
}

/// A backup checks out without touching the data directory, then restores
/// into a missing one as a database and key that match, with the tailnet
/// and ingress state.
#[tokio::test]
async fn a_backup_restores_into_an_empty_data_directory() {
    let (source, backup, instance) = backed_up().await;
    let target = tempfile::tempdir().expect("a directory is created");
    let data_dir = target.path().join("data");
    let backups = Backups::new(&data_dir);
    backups
        .restore(&backup, true)
        .await
        .expect("the backup checks out");
    assert!(!data_dir.exists(), "checking touches nothing");

    let manifest = backups
        .restore(&backup, false)
        .await
        .expect("the backup restores");
    assert_eq!(manifest.instance_id, instance);
    assert_eq!(
        files(&data_dir),
        [
            "ingress/data/caddy/certificates/cert.pem",
            DATABASE,
            SECRET_KEY,
            "tailscale/tailscaled.state",
        ]
    );
    assert_eq!(
        fs::read(data_dir.join(SECRET_KEY)).expect("the restored key reads"),
        fs::read(source.path().join(SECRET_KEY)).expect("the original key reads")
    );
    backups
        .ensure_restore_complete()
        .expect("the restore finished");
}

/// Restores `backup` into a new data directory, expecting a refusal that
/// leaves the directory empty.
async fn refused(backup: &Path) -> anyhow::Error {
    let target = tempfile::tempdir().expect("a directory is created");
    let data_dir = target.path().join("data");
    let error = Backups::new(&data_dir)
        .restore(backup, false)
        .await
        .expect_err("the restore is refused");
    let left = fs::read_dir(&data_dir).map_or(0, Iterator::count);
    assert_eq!(left, 0, "a refused restore leaves nothing: {error:#}");
    error
}

/// Restore refuses a newer schema, an entry no backup holds, a key that
/// doesn't decrypt the database's secrets, a damaged database, and a data
/// directory in use.
#[tokio::test]
async fn restore_refuses_backups_it_cannot_trust() {
    let (source, backup, _) = backed_up().await;

    let extra = backup.join("extra");
    fs::write(&extra, b"").expect("an extra file is written");
    let error = refused(&backup).await;
    assert!(
        matches!(error.downcast_ref(), Some(Refusal::UnexpectedEntry(name)) if name == "extra"),
        "{error:#}"
    );
    fs::remove_file(&extra).expect("the extra file is removed");

    let key = backup.join(SECRET_KEY);
    let original = fs::read(&key).expect("the key reads");
    fs::write(&key, [7; 32]).expect("another key is written");
    let error = refused(&backup).await;
    assert!(
        error.chain().any(|cause| matches!(
            cause.downcast_ref(),
            Some(piqueld_store::StoreError::SecretSource(_))
        )),
        "{error:#}"
    );
    fs::write(&key, original).expect("the original key is written back");

    let path = backup.join(MANIFEST);
    let manifest = fs::read(&path).expect("the manifest reads");
    let mut newer: Manifest = serde_json::from_slice(&manifest).expect("the manifest parses");
    newer.schema_version = SCHEMA_VERSION + 1;
    fs::write(
        &path,
        serde_json::to_vec(&newer).expect("the manifest serializes"),
    )
    .expect("the manifest is written");
    let error = refused(&backup).await;
    assert!(
        matches!(error.downcast_ref(), Some(Refusal::NewerSchema(_))),
        "{error:#}"
    );
    fs::write(&path, manifest).expect("the original manifest is written back");

    tamper(&backup, "UPDATE applications SET desired_json='{not json'").await;
    let error = refused(&backup).await;
    assert!(error.is::<Damaged>(), "{error:#}");

    fs::set_permissions(source.path(), fs::Permissions::from_mode(0o700))
        .expect("the data directory is private");
    let error = Backups::new(source.path())
        .restore(&backup, false)
        .await
        .expect_err("a data directory in use is refused");
    assert!(
        matches!(error.downcast_ref(), Some(Refusal::NotEmpty(_))),
        "{error:#}"
    );
}

/// The daemon refuses a data directory holding an unfinished restore.
#[test]
fn an_unfinished_restore_blocks_startup() {
    let data_dir = tempfile::tempdir().expect("a data directory is created");
    let backups = Backups::new(data_dir.path());
    backups
        .ensure_restore_complete()
        .expect("nothing is unfinished");
    fs::create_dir(data_dir.path().join(".restore-abc")).expect("a staging directory is created");
    let error = backups
        .ensure_restore_complete()
        .expect_err("the unfinished restore is refused");
    assert!(
        matches!(error.downcast_ref(), Some(Refusal::Unfinished(..))),
        "{error:#}"
    );
}

/// A publish that fails part way, here on a directory no rename replaces,
/// moves back what it moved and ends the restore, so it can run again.
#[test]
fn a_failed_publish_moves_back_what_it_moved() {
    let data_dir = tempfile::tempdir().expect("a data directory is created");
    let staging = tempfile::Builder::new()
        .prefix(restore::STAGING)
        .tempdir_in(data_dir.path())
        .expect("a staging directory is created");
    fs::write(staging.path().join("a"), b"restored").expect("a file is staged");
    fs::create_dir(staging.path().join("b")).expect("a directory is staged");
    fs::create_dir(data_dir.path().join("b")).expect("a directory is in the way");
    fs::write(data_dir.path().join("b/kept"), b"").expect("it isn't empty");

    let error = Backups::new(data_dir.path())
        .publish(staging)
        .expect_err("the publish fails");
    assert!(error.downcast_ref::<Refusal>().is_none(), "{error:#}");
    assert_eq!(files(data_dir.path()), ["b/kept"]);
}
