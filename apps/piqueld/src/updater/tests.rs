use super::*;

/// A layout in a temporary directory.
fn layout(root: &Path) -> Layout {
    let layout = Layout {
        builds: root.join("lib"),
        host: Some(HostFiles {
            bin: root.join("bin"),
            units: root.join("units"),
        }),
    };
    fs::create_dir_all(&layout.builds).expect("the builds directory is created");
    fs::create_dir_all(root.join("bin")).expect("the bin directory is created");
    layout
}

/// A build directory for `version`.
fn build(layout: &Layout, version: &str) {
    let directory = layout.build(version);
    fs::create_dir_all(&directory).expect("the build is created");
    fs::write(directory.join("piqueld"), version).expect("the daemon is written");
    fs::write(directory.join("piquelctl"), version).expect("the CLI is written");
}

/// Switching builds moves `current` and the binaries with it, and pruning
/// keeps only the current and previous builds.
#[test]
fn links_follow_the_current_build_and_pruning_keeps_the_previous() {
    let temp = tempfile::tempdir().expect("a temporary directory");
    let layout = layout(temp.path());
    for version in ["0.1.0", "0.2.0", "0.3.0"] {
        build(&layout, version);
    }
    fs::create_dir(layout.builds.join(".0.4.0.partial")).expect("a leftover is created");
    layout
        .link("previous", "0.2.0")
        .expect("previous is linked");
    layout.link("current", "0.3.0").expect("current is linked");
    layout.link_binaries().expect("the binaries are linked");

    layout.prune().expect("old builds are pruned");

    assert_eq!(layout.linked("current").as_deref(), Some("0.3.0"));
    assert_eq!(layout.linked("previous").as_deref(), Some("0.2.0"));
    assert_eq!(
        fs::read_to_string(temp.path().join("bin/piqueld")).expect("the daemon resolves"),
        "0.3.0"
    );
    let mut left: Vec<_> = fs::read_dir(&layout.builds)
        .expect("the builds are listed")
        .map(|entry| {
            entry
                .expect("an entry")
                .file_name()
                .into_string()
                .expect("a name")
        })
        .collect();
    left.sort();
    assert_eq!(left, ["0.2.0", "0.3.0", "current", "previous"]);
}

/// A version names a build's directory, so one that would leave the builds
/// directory is refused.
#[test]
fn versions_that_leave_the_builds_directory_are_refused() {
    for version in ["../etc", ".hidden", "0.3.0/..", "", "current", "previous"] {
        assert!(Build::version(version).is_err(), "{version:?}");
    }
    assert_eq!(
        Build::version("v0.4.0-nightly.20261125").expect("a nightly's version"),
        "0.4.0-nightly.20261125"
    );
}
