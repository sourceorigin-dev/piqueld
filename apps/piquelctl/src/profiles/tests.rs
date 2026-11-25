use super::Profiles;
use std::path::PathBuf;

struct Files {
    directory: tempfile::TempDir,
}

impl Files {
    fn new() -> Self {
        Self {
            directory: tempfile::tempdir().unwrap(),
        }
    }

    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.directory.path().join(name);
        std::fs::write(&path, text).unwrap();
        path
    }
}

#[test]
fn user_profiles_replace_whole_system_entries_before_validation() {
    let files = Files::new();
    let system = files.write(
        "system.toml",
        r#"
[profiles.prod]
url = "http://127.0.0.1:7845"
timeout = "2m"
[profiles.dev]
socket = "/tmp/system.sock"
url = "invalid"
timeout = "invalid"
[profiles.system]
socket = "/tmp/system-only.sock"
"#,
    );
    let user = files.write(
        "user.toml",
        r#"
[profiles.prod]
socket = "/tmp/user.sock"
[profiles.dev]
url = "http://localhost:7846"
[profiles.user]
socket = "/tmp/user-only.sock"
"#,
    );
    let profiles = Profiles::load_files([(system.clone(), false), (user.clone(), false)]).unwrap();
    assert_eq!(
        profiles
            .profiles
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["dev", "prod", "system", "user"]
    );
    let prod = &profiles.profiles["prod"];
    assert_eq!(prod.endpoint(), "/tmp/user.sock");
    assert!(prod.url.is_none());
    assert!(prod.timeout.is_none());
    assert_eq!(prod.source, user);
    assert_eq!(profiles.profiles["system"].source, system);
}

#[test]
fn later_files_replace_the_default_which_must_name_a_merged_profile() {
    let files = Files::new();
    let system = files.write(
        "system.toml",
        "default = 'dev'\n[profiles.prod]\nurl = 'http://127.0.0.1:7845'",
    );
    let user = files.write("user.toml", "[profiles.dev]\nsocket = '/tmp/dev.sock'");
    let profiles = Profiles::load_files([(system.clone(), false), (user.clone(), false)]).unwrap();
    assert_eq!(profiles.default_name(), "dev");
    let later = files.write("later.toml", "default = 'prod'");
    let profiles =
        Profiles::load_files([(system.clone(), false), (user, false), (later, false)]).unwrap();
    assert_eq!(profiles.default_name(), "prod");
    let error = Profiles::load_files([(system.clone(), false)])
        .err()
        .unwrap()
        .to_string();
    assert!(
        error.contains(&format!(
            "Unknown default connection profile dev in {}",
            system.display()
        )),
        "{error}"
    );
}

#[test]
fn missing_optional_files_are_ignored_but_required_files_fail_with_path() {
    let files = Files::new();
    let missing = files.directory.path().join("missing.toml");
    let valid = files.write("valid.toml", "[profiles.dev]\nsocket = '/tmp/dev.sock'");
    let profiles = Profiles::load_files([(missing.clone(), false), (valid, false)]).unwrap();
    assert!(profiles.profiles.contains_key("dev"));
    assert!(
        Profiles::load_files([(missing.clone(), false)])
            .unwrap()
            .profiles
            .is_empty()
    );
    let error = Profiles::load_files([(missing.clone(), true)])
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains(missing.to_str().unwrap()));
}

#[test]
fn malformed_and_unreadable_optional_files_fail_with_path() {
    let files = Files::new();
    let malformed = files.write("malformed.toml", "[profiles.");
    for path in [malformed, files.directory.path().to_owned()] {
        let error = Profiles::load_files([(path.clone(), false)])
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains(path.to_str().unwrap()), "{error}");
    }
}

#[test]
fn invalid_effective_profiles_report_name_and_source() {
    let files = Files::new();
    for (settings, expected) in [
        (
            "socket = '/tmp/dev.sock'\nurl = 'http://localhost'",
            "exactly one socket or URL",
        ),
        ("timeout = '2s'", "exactly one socket or URL"),
        ("url = 'invalid'", "not a valid URL"),
        (
            "socket = '/tmp/dev.sock'\ntimeout = 'bad'",
            "invalid timeout",
        ),
    ] {
        let path = files.write("invalid.toml", &format!("[profiles.broken]\n{settings}"));
        let error = Profiles::load_files([(path.clone(), false)])
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("broken")
                && error.contains(path.to_str().unwrap())
                && error.contains(expected),
            "{error}"
        );
    }
}
