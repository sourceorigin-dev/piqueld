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
    instance
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
    let data_dir = tempfile::tempdir().expect("a data directory is created");
    let instance = outdated(data_dir.path()).await;
    let backups = Backups::new(data_dir.path());
    let backup = backups
        .before_migration()
        .await
        .expect("the backup is written")
        .expect("an outdated database is backed up");

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
