use super::*;
use piqueld_store::Store;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

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
    set_schema(data_dir, SCHEMA_VERSION - 1).await;

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

/// Records `version` as the schema of the database in `data_dir`, as a
/// daemon of that version would.
async fn set_schema(data_dir: &Path, version: u64) {
    let options = sqlx::sqlite::SqliteConnectOptions::new().filename(data_dir.join(DATABASE));
    let pool = sqlx::SqlitePool::connect_with(options)
        .await
        .expect("the database opens");
    sqlx::query(&format!("PRAGMA user_version = {version}"))
        .execute(&pool)
        .await
        .expect("the version changes");
    sqlx::query("UPDATE instance_metadata SET schema_version=?1")
        .bind(i64::try_from(version).expect("the version fits"))
        .execute(&pool)
        .await
        .expect("the recorded version changes");
    pool.close().await;
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
    fs::set_permissions(data_dir.path(), fs::Permissions::from_mode(0o700))
        .expect("the data directory is private");
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
/// into one holding only rolled-back state, as a database and key that
/// match, with the tailnet and ingress state.
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
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(data_dir.join("rolled-back-1"))
        .expect("a rolled-back directory is created");

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
            Some(piqueld_store::StoreError::Secret(
                piqueld_store::SecretFailure::Source(_)
            ))
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
    fs::create_dir(data_dir.path().join(".publish-1")).expect("a staging directory is created");
    let error = backups
        .ensure_restore_complete()
        .expect_err("the unfinished restore is refused");
    assert!(
        matches!(error.downcast_ref(), Some(Refusal::Unfinished(..))),
        "{error:#}"
    );
}

/// Finishing a restore never moves its backup over state in the data
/// directory: it stops there, leaving the restore unfinished, with the
/// entry it couldn't move still in its copy.
#[test]
fn finishing_never_replaces_state() {
    let data_dir = tempfile::tempdir().expect("a data directory is created");
    let staging = staging::Staging {
        at_ms: 1,
        publishing: true,
    };
    let path = staging.path(data_dir.path());
    fs::create_dir(&path).expect("a staging directory is created");
    fs::write(path.join("a"), b"restored").expect("a file is staged");
    fs::write(data_dir.path().join("a"), b"state").expect("state is in the way");

    let backups = Backups::new(data_dir.path());
    backups
        .finish(staging)
        .expect_err("the restore can't finish");
    assert_eq!(
        fs::read(data_dir.path().join("a")).expect("the state reads"),
        b"state"
    );
    assert_eq!(
        fs::read(path.join("a")).expect("the copy reads"),
        b"restored"
    );
    backups
        .ensure_restore_complete()
        .expect_err("the restore stays unfinished");
}

/// Rolls `data_dir` back, with ingress stopping as `stopped` says,
/// returning what it did and the instance whose ingress it stopped, if any.
async fn roll_back_stopping(
    data_dir: &Path,
    stopped: Result<()>,
) -> (Result<RollBack>, Option<String>) {
    let mut instance = None;
    let result = Backups::new(data_dir)
        .roll_back(async |stopping| {
            instance = Some(stopping.as_str().to_owned());
            stopped
        })
        .await;
    (result, instance)
}

/// Rolls `data_dir` back, as [`roll_back_stopping`] does when ingress stops.
async fn roll_back(data_dir: &Path) -> (Result<RollBack>, Option<String>) {
    roll_back_stopping(data_dir, Ok(())).await
}

/// A data directory a newer daemon migrated after backing it up, its backup
/// and its instance ID.
async fn migrated_by_a_newer_daemon() -> (tempfile::TempDir, PathBuf, String) {
    let (data_dir, backup, instance) = backed_up().await;
    set_schema(data_dir.path(), SCHEMA_VERSION + 1).await;
    (data_dir, backup, instance)
}

/// The names in the directory `path`, sorted.
fn names(path: &Path) -> Vec<String> {
    entries(path, |_| true)
        .expect("the directory reads")
        .into_iter()
        .map(|name| name.into_string().expect("UTF-8"))
        .collect()
}

/// The files under `data_dir`, but for the `-wal` and `-shm` files SQLite
/// may leave when it reads the database.
fn state(data_dir: &Path) -> Vec<String> {
    let mut files = files(data_dir);
    files.retain(|file| !file.ends_with("-wal") && !file.ends_with("-shm"));
    files
}

/// A roll-back of a missing data directory does nothing.
#[tokio::test]
async fn roll_back_leaves_a_missing_data_directory() {
    let target = tempfile::tempdir().expect("a directory is created");
    let missing = target.path().join("data");
    let (result, stopped) = roll_back(&missing).await;
    assert!(matches!(result, Ok(RollBack::Unneeded)), "{result:?}");
    assert!(!missing.exists() && stopped.is_none());
}

/// A roll-back touches nothing while this binary opens the database.
#[tokio::test]
async fn roll_back_leaves_a_database_this_binary_opens() {
    let (data_dir, _, _) = backed_up().await;
    let before = files(data_dir.path());
    let (result, stopped) = roll_back(data_dir.path()).await;
    assert!(matches!(result, Ok(RollBack::Unneeded)), "{result:?}");
    assert!(stopped.is_none());
    assert_eq!(files(data_dir.path()), before);
}

/// After a newer daemon migrated the database, a roll-back stops the
/// instance's ingress, moves the state aside and restores the newest backup
/// this binary opens, past one the newer daemon took, leaving the backups,
/// telemetry sockets and caches, such as local-change snapshots, in place.
#[tokio::test]
async fn roll_back_restores_the_newest_backup_this_binary_opens() {
    let (data_dir, backup, instance) = migrated_by_a_newer_daemon().await;
    let data_dir = data_dir.path();
    fs::create_dir_all(data_dir.join("telemetry/env")).expect("a receiver's directory is created");
    fs::write(data_dir.join("telemetry/env/otlp.sock"), b"").expect("its socket is created");
    let newer = PreMigration {
        created_at_ms: i64::MAX,
        schema_version: SCHEMA_VERSION + 1,
    };
    fs::create_dir(data_dir.join(BACKUPS).join(newer.name())).expect("a newer backup is made");

    let (result, stopped) = roll_back(data_dir).await;
    let Ok(RollBack::Restored {
        backup: restored,
        manifest,
        aside,
    }) = result
    else {
        panic!("the state is rolled back: {result:?}");
    };
    assert_eq!(restored, backup);
    assert_eq!(manifest.schema_version, SCHEMA_VERSION - 1);
    assert_eq!(stopped.as_deref(), Some(instance.as_str()));
    let aside_name = aside
        .file_name()
        .expect("it has a name")
        .to_str()
        .expect("UTF-8");
    assert_eq!(
        names(data_dir),
        [
            BACKUPS,
            "ingress",
            DATABASE,
            RollBackReport::FILE,
            aside_name,
            SECRET_KEY,
            "snapshots",
            "tailscale",
            "telemetry"
        ]
    );
    assert!(data_dir.join("telemetry/env/otlp.sock").exists());
    assert_eq!(
        names(&aside),
        [
            "ingress",
            DATABASE,
            "piqueld.db-shm",
            "piqueld.db-wal",
            SECRET_KEY,
            "tailscale"
        ]
    );
    let identity = Backups::new(data_dir)
        .identity()
        .await
        .expect("the database reads")
        .expect("it has a schema");
    assert_eq!(identity.schema_version, SCHEMA_VERSION - 1);
}

/// A rolled-back data directory keeps its backups, so going back further
/// rolls it back again.
#[tokio::test]
async fn a_rolled_back_data_directory_rolls_back_again() {
    let (data_dir, backup, _) = migrated_by_a_newer_daemon().await;
    let data_dir = data_dir.path();
    let (result, _) = roll_back(data_dir).await;
    assert!(
        matches!(result, Ok(RollBack::Restored { .. })),
        "{result:?}"
    );
    set_schema(data_dir, SCHEMA_VERSION + 1).await;

    let (result, _) = roll_back(data_dir).await;
    assert!(
        matches!(&result, Ok(RollBack::Restored { backup: again, .. }) if *again == backup),
        "{result:?}"
    );
}

/// Without a backup this binary opens, a roll-back fails before it touches
/// anything.
#[tokio::test]
async fn roll_back_without_a_fitting_backup_changes_nothing() {
    let (data_dir, backup, _) = migrated_by_a_newer_daemon().await;
    let data_dir = data_dir.path();
    fs::remove_dir_all(&backup).expect("the backup is removed");
    let before = state(data_dir);

    let (result, stopped) = roll_back(data_dir).await;
    let error = result.expect_err("the roll-back fails");
    assert!(
        matches!(error.downcast_ref(), Some(Refusal::NoBackup { .. })),
        "{error:#}"
    );
    assert!(stopped.is_none());
    assert_eq!(state(data_dir), before);
}

/// A roll-back whose ingress doesn't stop fails before it moves anything,
/// and leaves no copy of the backup behind.
#[tokio::test]
async fn roll_back_fails_unchanged_when_ingress_does_not_stop() {
    let (data_dir, _, _) = migrated_by_a_newer_daemon().await;
    let data_dir = data_dir.path();
    let before = state(data_dir);

    let (result, _) = roll_back_stopping(data_dir, Err(anyhow::anyhow!("Docker is down"))).await;
    result.expect_err("the roll-back fails");
    assert_eq!(state(data_dir), before);
}

/// A copy of a backup left by a restore stopped while verifying it blocks
/// nothing: the next restore removes it.
#[tokio::test]
async fn a_copy_left_while_verifying_is_removed() {
    let (_source, backup, _) = backed_up().await;
    let target = tempfile::tempdir().expect("a directory is created");
    let data_dir = target.path().join("data");
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(data_dir.join(".staged-1"))
        .expect("a stale copy is left");

    Backups::new(&data_dir)
        .restore(&backup, false)
        .await
        .expect("the backup restores");
    assert!(!data_dir.join(".staged-1").exists());
}

/// A roll-back whose backup fails to publish once the state moved aside,
/// here because its copy holds an entry the data directory keeps, stays
/// unfinished, and the next roll-back finishes it once that is fixed.
#[tokio::test]
async fn a_roll_back_that_cannot_publish_is_finished_by_the_next() {
    let (data_dir, _, _) = migrated_by_a_newer_daemon().await;
    let data_dir = data_dir.path();
    fs::create_dir_all(data_dir.join("telemetry/env")).expect("a receiver's directory is created");
    fs::write(data_dir.join("telemetry/env/otlp.sock"), b"").expect("its socket is created");

    Backups::new(data_dir)
        .roll_back(async |_| {
            for name in entries(data_dir, |name| {
                name.as_encoded_bytes().starts_with(b".staged-")
            })? {
                fs::create_dir(data_dir.join(name).join("telemetry"))?;
            }
            Ok(())
        })
        .await
        .expect_err("the roll-back fails");
    let backups = Backups::new(data_dir);
    let staging = backups
        .unfinished()
        .expect("the data directory reads")
        .expect("the roll-back is unfinished");
    assert!(staging.publishing, "{staging:?}");
    fs::remove_dir(staging.path(data_dir).join("telemetry")).expect("the conflict is removed");

    let (result, stopped) = roll_back(data_dir).await;
    assert!(matches!(result, Ok(RollBack::Finished)), "{result:?}");
    assert!(stopped.is_none());
    backups
        .ensure_restore_complete()
        .expect("the roll-back finished");
    let identity = backups
        .identity()
        .await
        .expect("the database reads")
        .expect("it has a schema");
    assert_eq!(identity.schema_version, SCHEMA_VERSION - 1);
}

/// A roll-back interrupted while it moved the state aside is finished by the
/// next one: the rest of the state moves aside and the backup into place.
#[tokio::test]
async fn a_roll_back_interrupted_moving_the_state_is_finished() {
    let (data_dir, backup, _) = migrated_by_a_newer_daemon().await;
    let data_dir = data_dir.path();
    let backups = Backups::new(data_dir);
    let manifest = Backups::manifest(&backup).expect("the manifest reads");
    let staging = backups
        .mark(
            backups
                .copy_verified(&backup, &manifest)
                .await
                .expect("the backup verifies"),
            piqueld_store::now_ms(),
        )
        .expect("the restore starts");
    let aside = data_dir.join(RolledBack::name(staging.at_ms));
    fs::create_dir(&aside).expect("the rolled-back directory is made");
    fs::rename(data_dir.join("ingress"), aside.join("ingress")).expect("part of the state moves");

    let (result, _) = roll_back(data_dir).await;
    assert!(matches!(result, Ok(RollBack::Finished)), "{result:?}");
    assert!(names(&aside).contains(&DATABASE.to_owned()));
    assert!(names(&aside).contains(&"ingress".to_owned()));
    let identity = backups
        .identity()
        .await
        .expect("the database reads")
        .expect("it has a schema");
    assert_eq!(identity.schema_version, SCHEMA_VERSION - 1);
}

/// A roll-back keeps the newest rolled-back directories, its own included,
/// and leaves ones a person named.
#[tokio::test]
async fn a_roll_back_keeps_the_newest_rolled_back_directories() {
    let (data_dir, _, _) = migrated_by_a_newer_daemon().await;
    let data_dir = data_dir.path();
    for name in [
        "rolled-back-1",
        "rolled-back-2",
        "rolled-back-3",
        "rolled-back-recovered",
    ] {
        fs::create_dir(data_dir.join(name)).expect("an earlier roll-back's directory is made");
    }

    let (result, _) = roll_back(data_dir).await;
    let Ok(RollBack::Restored { aside, .. }) = result else {
        panic!("the state is rolled back: {result:?}");
    };
    let mut rolled_back = names(data_dir);
    rolled_back.retain(|name| name.starts_with("rolled-back-"));
    let newest = aside
        .file_name()
        .expect("it has a name")
        .to_str()
        .expect("UTF-8");
    let mut kept = vec![
        "rolled-back-2",
        "rolled-back-3",
        newest,
        "rolled-back-recovered",
    ];
    kept.sort_unstable();
    assert_eq!(rolled_back, kept);
}

/// A restore into a data directory holding an unfinished one says so, rather
/// than that the directory isn't empty.
#[tokio::test]
async fn restore_names_an_unfinished_restore() {
    let (_source, backup, _) = backed_up().await;
    let target = tempfile::tempdir().expect("a directory is created");
    let data_dir = target.path().join("data");
    fs::DirBuilder::new()
        .mode(0o700)
        .recursive(true)
        .create(data_dir.join(".publish-1"))
        .expect("an unfinished restore is left");

    let error = Backups::new(&data_dir)
        .restore(&backup, false)
        .await
        .expect_err("the restore is refused");
    assert!(
        matches!(error.downcast_ref(), Some(Refusal::Unfinished(..))),
        "{error:#}"
    );
}

/// A roll-back leaves a report of what it did, which the daemon records as a
/// `daemon_rolled_back` event once it starts, then removes. The fixture's
/// backup only records an older schema, so the store opens it once the
/// current one is recorded again.
#[tokio::test]
async fn a_roll_back_is_reported_once_the_daemon_starts() {
    let (data_dir, backup, _) = migrated_by_a_newer_daemon().await;
    let data_dir = data_dir.path();
    let (result, _) = roll_back(data_dir).await;
    let Ok(RollBack::Restored { aside, .. }) = result else {
        panic!("the state is rolled back: {result:?}");
    };
    let report = RollBackReport::read(data_dir)
        .await
        .expect("the report reads")
        .expect("the roll-back left a report");
    assert_eq!(
        report,
        RollBackReport {
            backup: backup
                .file_name()
                .and_then(|name| name.to_str())
                .expect("the backup has a name")
                .to_owned(),
            from_schema: SCHEMA_VERSION + 1,
            to_schema: SCHEMA_VERSION - 1,
            replaced: aside,
        }
    );

    set_schema(data_dir, SCHEMA_VERSION).await;
    let store = Store::open_unsynced(data_dir.join(DATABASE))
        .await
        .expect("the restored database opens");
    let backups = Backups::new(data_dir);
    backups
        .report_roll_back(&store)
        .await
        .expect("the roll-back is reported");
    assert!(!data_dir.join(RollBackReport::FILE).exists());
    drop(store);
    let options = sqlx::sqlite::SqliteConnectOptions::new().filename(data_dir.join(DATABASE));
    let pool = sqlx::SqlitePool::connect_with(options)
        .await
        .expect("the database opens");
    let reported: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT scope, resource FROM events WHERE kind='daemon_rolled_back'")
            .fetch_all(&pool)
            .await
            .expect("the events read");
    assert_eq!(reported, [("daemon".to_owned(), Some(report.backup))]);
}

/// A report a roll-back left before it began, naming a rolled-back directory
/// that doesn't exist, is removed without announcing a roll-back.
#[tokio::test]
async fn a_report_of_a_roll_back_that_never_began_is_dropped() {
    let (data_dir, _, _) = backed_up().await;
    let data_dir = data_dir.path();
    set_schema(data_dir, SCHEMA_VERSION).await;
    RollBackReport {
        backup: "pre-1-1".to_owned(),
        from_schema: SCHEMA_VERSION + 1,
        to_schema: 1,
        replaced: data_dir.join("rolled-back-1"),
    }
    .write(data_dir)
    .await
    .expect("the report is written");
    let store = Store::open_unsynced(data_dir.join(DATABASE))
        .await
        .expect("the database opens");

    Backups::new(data_dir)
        .report_roll_back(&store)
        .await
        .expect("the report is dropped");
    assert!(!data_dir.join(RollBackReport::FILE).exists());
    drop(store);
    let options = sqlx::sqlite::SqliteConnectOptions::new().filename(data_dir.join(DATABASE));
    let pool = sqlx::SqlitePool::connect_with(options)
        .await
        .expect("the database opens");
    let reported: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE kind='daemon_rolled_back'")
            .fetch_one(&pool)
            .await
            .expect("the events count");
    assert_eq!(reported, 0);
}
