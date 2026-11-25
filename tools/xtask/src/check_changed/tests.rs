use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::ci::{Job, Recipe};
use super::filters::{Filter, Filters};
use super::{Change, Crate, Outcome, Scope};
use crate::Workspace;
use crate::metadata::{Dependency, Metadata, Package, Target};

/// Filters naming every [`Filter`], `rust` reaching `workspace` through an
/// alias of an alias.
const FILTERS: &str = "
workspace: &workspace
  - Cargo.lock
crates: &crates
  - *workspace
  - crates/piqueld-core/**
daemon: [apps/piqueld/**]
rust:
  - *crates
  - apps/**
deny: ['**/Cargo.toml']
scripts: [apps/site/install.sh]
ui: [apps/piqueld-ui/**]
cli: [apps/piquelctl/**]
docker: [scripts/**]
playwright: [tests/playwright/**]
";

fn paths(paths: &[&str]) -> BTreeSet<String> {
    paths.iter().map(ToString::to_string).collect()
}

#[test]
fn globs_match_as_picomatch_does() {
    let cases = [
        ("crates/**", "crates/piqueld-core/src/lib.rs", true),
        ("crates/**", "crates/piqueld-core/.env", true),
        ("crates/**", "xcrates/lib.rs", false),
        (".github/workflows/**", ".github/workflows/ci.yml", true),
        ("**/Cargo.toml", "Cargo.toml", true),
        ("**/Cargo.toml", "apps/piquelctl/Cargo.toml", true),
        ("**/Cargo.toml", "Cargo.toml.orig", false),
        ("docs/**/*.json", "docs/schema.json", true),
        ("docs/**/*.json", "docs/user/reference/schema.json", true),
        ("*.md", "README.md", true),
        ("*.md", "docs/README.md", false),
        ("apps/*/src/*.rs", "apps/.hidden/src/lib.rs", true),
        ("apps/*/src/*.rs", "apps/piquelctl/src/cli/mod.rs", false),
    ];
    for (pattern, path, matches) in cases {
        let yaml = FILTERS.replace("[scripts/**]", &format!("['{pattern}']"));
        let filters = Filters::parse(&yaml).expect("the filters parse");
        assert_eq!(
            filters.matches(Filter::Docker, path),
            matches,
            "{pattern} against {path}"
        );
    }
}

#[test]
fn aliases_include_the_filters_they_name() {
    let filters = Filters::parse(FILTERS).expect("the filters parse");
    assert_eq!(
        filters.matching(&paths(&["Cargo.lock"])),
        BTreeSet::from([Filter::Workspace, Filter::Crates, Filter::Rust])
    );
}

#[test]
fn an_unknown_filter_is_an_error() {
    let yaml = format!("{FILTERS}frontend: [web/**]\n");
    let error = Filters::parse(&yaml).expect_err("`frontend` is no filter");
    assert!(format!("{error:#}").contains("unknown filter `frontend`"));
}

#[test]
fn the_repository_filters_name_every_filter() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    Filters::load(&root).expect("the repository's filters load");
}

/// A crate in `/w/<dir>`, depending on `dependencies`.
fn package(dir: &str, dependencies: &[&str], library: bool) -> Package {
    Package {
        name: dir.rsplit('/').next().expect("a name").to_owned(),
        manifest_path: PathBuf::from(format!("/w/{dir}/Cargo.toml")),
        dependencies: dependencies
            .iter()
            .map(|name| Dependency {
                name: (*name).to_owned(),
            })
            .collect(),
        targets: vec![Target {
            kind: vec![if library { "lib" } else { "bin" }.to_owned()],
        }],
    }
}

fn metadata() -> Metadata {
    Metadata {
        packages: vec![
            package("crates/piqueld-core", &["serde"], true),
            package("crates/piqueld-client", &["piqueld-core"], true),
            package("apps/piquelctl", &["piqueld-client"], true),
            package("apps/piqueld-ui", &["piqueld-client"], true),
            package("apps/piqueld", &["piqueld-core"], true),
            package("tools/xtask", &["piqueld-client"], false),
        ],
    }
}

fn crates(names: &[(&str, bool)]) -> Scope {
    Scope::Crates(
        names
            .iter()
            .map(|&(name, library)| Crate {
                name: name.to_owned(),
                library,
            })
            .collect(),
    )
}

#[test]
fn a_change_reaches_its_crates_dependents() {
    let changed = paths(&["crates/piqueld-client/src/lib.rs"]);
    assert_eq!(
        Scope::new(&metadata(), Path::new("/w"), changed.iter()),
        crates(&[
            ("piqueld-client", true),
            ("piquelctl", true),
            ("piqueld-ui", true),
            ("piqueld", true),
            ("xtask", false),
        ])
    );
}

#[test]
fn a_dashboard_change_reaches_the_daemon_embedding_it() {
    let changed = paths(&["apps/piqueld-ui/src/app.rs"]);
    assert_eq!(
        Scope::new(&metadata(), Path::new("/w"), changed.iter()),
        crates(&[("piqueld-ui", true), ("piqueld", true)])
    );
}

#[test]
fn a_path_in_no_crate_reaches_the_workspace() {
    let changed = paths(&["apps/piquelctl/src/main.rs", "Cargo.lock"]);
    assert_eq!(
        Scope::new(&metadata(), Path::new("/w"), changed.iter()),
        Scope::Workspace {
            path: "Cargo.lock".to_owned()
        }
    );
}

/// The command and outcome of `recipe` in `job` for `change`.
fn plan(change: &Change, job: Job, recipe: Recipe) -> (String, Outcome) {
    let step = change.plan(job, recipe);
    (step.command.join(" "), step.outcome)
}

#[test]
fn checks_narrow_to_the_crates_the_change_reaches() {
    let change = Change {
        filters: BTreeSet::from([Filter::Rust, Filter::Cli]),
        scope: crates(&[("piquelctl", true), ("xtask", false)]),
    };
    let cases = [
        (
            Job::Checks,
            Recipe::FmtCheck,
            "just fmt-check",
            Outcome::ToRun,
        ),
        (
            Job::RustLint,
            Recipe::Lint,
            "just lint --package=piquelctl --package=xtask",
            Outcome::ToRun,
        ),
        (
            Job::RustTest,
            Recipe::DocTest,
            "just doc-test --package=piquelctl",
            Outcome::ToRun,
        ),
        (
            Job::RustLint,
            Recipe::OpenapiCheck,
            "just openapi-check",
            Outcome::ToRun,
        ),
        (
            Job::WasmUi,
            Recipe::UiCheck,
            "just ui-check",
            Outcome::Skipped("no change matches ui".to_owned()),
        ),
        (
            Job::MacosCli,
            Recipe::ValidateCli,
            "just validate-cli",
            Outcome::LeftToCi("runs on macOS, which drafts skip too"),
        ),
    ];
    for (job, recipe, command, outcome) in cases {
        assert_eq!(
            plan(&change, job, recipe),
            (command.to_owned(), outcome),
            "{recipe:?}"
        );
    }
}

#[test]
fn checks_of_crates_the_change_misses_are_skipped() {
    let change = Change {
        filters: BTreeSet::from([Filter::Rust]),
        scope: crates(&[("xtask", false)]),
    };
    assert_eq!(
        plan(&change, Job::RustTest, Recipe::DocTest).1,
        Outcome::Skipped("no crate the change reaches has a library".to_owned())
    );
    assert_eq!(
        plan(&change, Job::RustLint, Recipe::OpenapiCheck).1,
        Outcome::Skipped("the change reaches none of piqueld, piquelctl".to_owned())
    );
}

#[test]
fn checks_run_on_the_workspace_when_a_change_reaches_it() {
    let change = Change {
        filters: BTreeSet::from([Filter::Rust]),
        scope: Scope::Workspace {
            path: "Cargo.lock".to_owned(),
        },
    };
    assert_eq!(
        plan(&change, Job::RustTest, Recipe::Test),
        ("just test".to_owned(), Outcome::ToRun)
    );
}

/// Runs git in `dir` as a test author.
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .current_dir(dir)
        .status()
        .expect("git runs");
    assert!(status.success(), "git {}", args.join(" "));
}

#[test]
fn changed_paths_count_every_change_since_the_merge_base() {
    let repository = tempfile::tempdir().expect("a temporary directory");
    let root = repository.path();
    let write = |path: &str| std::fs::write(root.join(path), path).expect("the file is written");
    git(root, &["init", "--quiet", "--initial-branch=main"]);
    for path in ["committed", "renamed", "unstaged"] {
        write(path);
    }
    std::fs::write(root.join(".gitignore"), "ignored\n").expect("the file is written");
    git(root, &["add", "."]);
    git(root, &["commit", "--quiet", "--message=base"]);
    write("on-main");
    git(root, &["add", "on-main"]);
    git(root, &["commit", "--quiet", "--message=main"]);
    git(root, &["checkout", "--quiet", "-b", "change", "HEAD~"]);
    std::fs::write(root.join("committed"), "changed").expect("the file is written");
    git(root, &["mv", "renamed", "moved"]);
    git(root, &["commit", "--quiet", "--all", "--message=change"]);
    write("staged");
    git(root, &["add", "staged"]);
    std::fs::write(root.join("unstaged"), "changed").expect("the file is written");
    write("untracked");
    write("ignored");

    let workspace = Workspace {
        root: root.to_owned(),
    };
    let (_, changed) = workspace
        .changed_paths("main")
        .expect("the paths are listed");
    assert_eq!(
        changed,
        paths(&[
            "committed",
            "moved",
            "renamed",
            "staged",
            "unstaged",
            "untracked",
        ])
    );
}
