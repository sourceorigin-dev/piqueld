use super::*;
use std::path::Path;

async fn commit(root: &Path, contents: &str) -> String {
    tokio::fs::write(root.join("Dockerfile"), contents)
        .await
        .expect("the file is written");
    piqueld_docker::command::LoggedCommand::run_recorded(
        Checkout::command_in(root).args(["add", "."]),
        "stage fixture",
        None,
    )
    .await
    .expect("run recorded succeeds");
    piqueld_docker::command::LoggedCommand::run_recorded(
        Checkout::command_in(root).args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "fixture",
        ]),
        "commit fixture",
        None,
    )
    .await
    .expect("run recorded succeeds");
    Checkout::head(root)
        .await
        .expect("the checkout has a head")
        .to_string()
}

#[tokio::test]
async fn checkout_ignores_host_wide_git_configuration() {
    let root = tempfile::tempdir().expect("the temp dir is created");
    let system = root.path().join("gitconfig");
    tokio::fs::write(&system, "[piqueld]\n\tprobe = system\n")
        .await
        .expect("the file is written");
    let output = Checkout::command()
        .env("GIT_CONFIG_SYSTEM", &system)
        .args(["config", "--get", "piqueld.probe"])
        .output()
        .await
        .expect("output succeeds");
    assert!(output.stdout.is_empty(), "{output:?}");
}

#[tokio::test]
async fn checkout_rejects_executable_transports_even_when_host_allows_them() {
    let root = tempfile::tempdir().expect("the temp dir is created");
    let marker = root.path().join("executed");
    let helper = format!("ext::touch {}", marker.display());
    for rewritten in [false, true] {
        let mut command = Checkout::command();
        command.args(["-c", "protocol.ext.allow=always"]);
        if rewritten {
            command.args([
                "-c",
                &format!("url.{helper}.insteadOf=https://example.com/repo"),
            ]);
        }
        command
            .args(["clone", "--"])
            .arg(if rewritten {
                "https://example.com/repo"
            } else {
                &helper
            })
            .arg(root.path().join("checkout"));
        let error = piqueld_docker::command::LoggedCommand::run_recorded(
            &mut command,
            "clone fixture",
            None,
        )
        .await
        .expect_err("run recorded fails");
        assert!(
            error.to_string().contains("transport 'ext' not allowed"),
            "{error:#}"
        );
        assert!(!marker.exists());
    }
}

/// Checkouts in a temporary directory within `max_bytes`.
async fn checkouts(root: &Path, max_bytes: u64) -> Checkouts {
    Checkouts::open(root.join("checkouts"))
        .await
        .expect("the store opens")
        .with_max_bytes(std::num::NonZeroU64::new(max_bytes).expect("the value is valid"))
}

/// A repository with one commit of `Dockerfile`, returning its source.
async fn fixture(root: &Path) -> GitRepository {
    piqueld_docker::command::LoggedCommand::run_recorded(
        Checkout::command()
            .args(["init", "--initial-branch=main"])
            .arg(root),
        "initialize fixture",
        None,
    )
    .await
    .expect("run recorded succeeds");
    commit(root, "FROM scratch\n").await;
    GitRepository {
        url: root.display().to_string(),
        branch: "main".into(),
        commit: None,
    }
}

#[tokio::test]
async fn checkout_pins_commits_and_confines_paths() {
    let repository = tempfile::tempdir().expect("the temp dir is created");
    let mut source = fixture(repository.path()).await;
    let store = tempfile::tempdir().expect("the temp dir is created");
    let checkouts = checkouts(store.path(), 1 << 30).await;
    let checkout = checkouts
        .checkout(&source, None)
        .await
        .expect("checkout succeeds");
    let first = checkout.commit.to_string();
    let second = commit(repository.path(), "FROM scratch\nLABEL version=2\n").await;
    {
        let latest = checkouts
            .checkout(&source, None)
            .await
            .expect("checkout succeeds");
        assert_eq!(latest.commit.as_str(), second);
        let tree = Checkout::command_in(&latest.root())
            .args(["ls-tree", "-r", "--name-only", "HEAD"])
            .output()
            .await
            .expect("output succeeds");
        assert_eq!(
            String::from_utf8(tree.stdout).expect("from utf8 succeeds"),
            "Dockerfile\n"
        );
    }
    source.commit = Some(first.clone());
    source.branch = "branch-no-longer-exists".into();
    assert_eq!(
        checkouts
            .checkout(&source, None)
            .await
            .expect("checkout succeeds")
            .commit
            .as_str(),
        first
    );
    checkout
        .path("../Dockerfile")
        .await
        .expect_err("`path` should be rejected");
    checkout
        .path("missing")
        .await
        .expect_err("`path` should be rejected");
    std::os::unix::fs::symlink(repository.path(), checkout.root().join("escape"))
        .expect("symlink succeeds");
    checkout
        .path("escape/Dockerfile")
        .await
        .expect_err("`path` should be rejected");
    assert_eq!(
        tokio::fs::read_to_string(checkout.path("./Dockerfile").await.expect("path succeeds"))
            .await
            .expect("read to string succeeds"),
        "FROM scratch\n"
    );
    let depth = Checkout::command_in(&checkout.root())
        .args(["rev-list", "--count", "HEAD"])
        .output()
        .await
        .expect("output succeeds");
    assert_eq!(
        String::from_utf8(depth.stdout)
            .expect("from utf8 succeeds")
            .trim(),
        "1"
    );
    drop(checkout);
    let left = std::fs::read_dir(&checkouts.root)
        .expect("read dir succeeds")
        .count();
    assert_eq!(left, 0);
}

/// A tree that can never fit is refused before it is written; one that
/// fits once another checkout is gone waits for it.
#[tokio::test]
async fn checkouts_wait_for_room_and_refuse_trees_that_never_fit() {
    let repository = tempfile::tempdir().expect("the temp dir is created");
    let source = fixture(repository.path()).await;
    let checkouts = checkouts(repository.path(), 1 << 20).await;
    let held = checkouts
        .checkout(&source, None)
        .await
        .expect("checkout succeeds");
    let waiting = checkouts.checkout(&source, None);
    tokio::pin!(waiting);
    assert!(
        tokio::time::timeout(Duration::from_millis(500), &mut waiting)
            .await
            .is_err()
    );
    drop(held);
    waiting.await.expect("the waiting checkout completes");

    let small = self::checkouts(repository.path(), 4).await;
    let error = small
        .checkout(&source, None)
        .await
        .err()
        .expect("the checkout is refused");
    assert!(error.downcast_ref::<TreeTooLarge>().is_some(), "{error:#}");
    assert_eq!(
        std::fs::read_dir(&small.root)
            .expect("read dir succeeds")
            .count(),
        0
    );
}

/// Reads that arrive while a repository is being listed share that
/// listing; a later read lists it again.
#[tokio::test]
async fn listings_are_shared_only_by_reads_that_overlap() {
    let repository = tempfile::tempdir().expect("the temp dir is created");
    let url = fixture(repository.path()).await.url;
    let listings = Listings::default();
    let [first, second] =
        futures_util::future::join_all([listings.list(&url), listings.list(&url)])
            .await
            .try_into()
            .ok()
            .expect("the size fits");
    assert!(Arc::ptr_eq(&first, &second));
    assert!(listings.repositories.lock().is_empty());
    assert!(
        first
            .as_ref()
            .as_ref()
            .expect("the value is present")
            .head("main")
            .is_some()
    );
    let later = listings.list(&url).await;
    assert!(!Arc::ptr_eq(&first, &later));
    let cancelled = listings.list(&url);
    drop(tokio::time::timeout(Duration::ZERO, cancelled).await);
    assert!(listings.repositories.lock().is_empty());
}

/// The repository's attributes cannot convert, and so grow, the files
/// checked out: they are the committed bytes.
#[tokio::test]
async fn checkouts_are_the_committed_bytes() {
    let repository = tempfile::tempdir().expect("the temp dir is created");
    let source = fixture(repository.path()).await;
    tokio::fs::write(repository.path().join(".gitattributes"), "* ident\n")
        .await
        .expect("the file is written");
    commit(repository.path(), "$Id$\n").await;
    let checkouts = checkouts(repository.path(), 1 << 30).await;
    let checkout = checkouts
        .checkout(&source, None)
        .await
        .expect("checkout succeeds");
    let dockerfile = checkout.path("Dockerfile").await.expect("path succeeds");
    assert_eq!(
        tokio::fs::read_to_string(dockerfile)
            .await
            .expect("read to string succeeds"),
        "$Id$\n"
    );
}

/// A checkout under a restrictive umask still has Git's modes, so an
/// unprivileged user in an image built from it can read its files.
#[tokio::test]
async fn checkouts_have_the_committed_modes() {
    use std::os::unix::fs::PermissionsExt;
    let repository = tempfile::tempdir().expect("the temp dir is created");
    let source = fixture(repository.path()).await;
    let script = repository.path().join("bin/run");
    tokio::fs::create_dir(repository.path().join("bin"))
        .await
        .expect("create dir succeeds");
    tokio::fs::write(&script, "#!/bin/sh\n")
        .await
        .expect("the file is written");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .expect("set permissions succeeds");
    commit(repository.path(), "FROM scratch\n").await;
    let checkouts = checkouts(repository.path(), 1 << 30).await;
    let checkout = checkouts
        .checkout(&source, None)
        .await
        .expect("checkout succeeds");
    let root = checkout.root();
    for (path, mode) in [("Dockerfile", 0o600), ("bin", 0o700), ("bin/run", 0o700)] {
        std::fs::set_permissions(root.join(path), std::fs::Permissions::from_mode(mode))
            .expect("set permissions succeeds");
    }
    checkout
        .restore_modes()
        .await
        .expect("restore modes succeeds");
    for (path, mode) in [("Dockerfile", 0o644), ("bin", 0o755), ("bin/run", 0o755)] {
        let actual = std::fs::metadata(root.join(path))
            .expect("the file has metadata")
            .permissions()
            .mode();
        assert_eq!(actual & 0o777, mode, "{path}");
    }
}

/// Directories are charged too: a deep tree of tiny files takes far
/// more than its files' bytes.
#[tokio::test]
async fn checkouts_charge_their_directories() {
    let repository = tempfile::tempdir().expect("the temp dir is created");
    let source = fixture(repository.path()).await;
    for directory in 0..50 {
        let path = repository.path().join(format!("{directory}/a/b/c"));
        tokio::fs::create_dir_all(&path)
            .await
            .expect("the directory is created");
        tokio::fs::write(path.join("f"), "x")
            .await
            .expect("the file is written");
    }
    let head = commit(repository.path(), "FROM scratch\n").await;
    let checkouts = checkouts(repository.path(), 1 << 30).await;
    let tree = checkouts
        .measure(Path::new(&source.url), &head)
        .await
        .expect("measure succeeds");
    assert_eq!(tree, 251 * (4096 + 256));
}

/// A download that grows past its limit is stopped, and one that
/// finishes past it, before it is checked, fails.
#[tokio::test]
async fn downloads_past_their_limit_are_stopped() {
    let directory = tempfile::tempdir().expect("the temp dir is created");
    let pack = directory.path().join("pack");
    let stalled = async {
        tokio::fs::write(&pack, vec![0; 8192])
            .await
            .expect("the file is written");
        std::future::pending::<anyhow::Result<()>>().await
    };
    let finished = async {
        tokio::fs::write(&pack, vec![0; 8192])
            .await
            .expect("the file is written");
        anyhow::Ok(())
    };
    for result in [
        Checkouts::bounded(directory.path(), 4096, stalled).await,
        Checkouts::bounded(directory.path(), 4096, finished).await,
    ] {
        let error = result.expect_err("the bounded checkout is refused");
        assert!(error.downcast_ref::<TreeTooLarge>().is_some(), "{error:#}");
    }
}
