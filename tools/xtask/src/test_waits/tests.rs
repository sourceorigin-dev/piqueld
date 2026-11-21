use std::path::PathBuf;

use super::Source;

/// Files of a workspace, each line ending in `// flagged` a sleep the check
/// must report, and every other sleep one it must allow.
const FILES: [(&str, &str); 8] = [
    // A test file, in any import form, macros included.
    (
        "src/a/tests.rs",
        r"
use std::thread;
use tokio::time::{self, sleep as nap};
fn helper() { thread::sleep(D); } // flagged
async fn waits() {
    std::thread::sleep(D); // flagged
    time::sleep(D).await; // flagged
    nap(D).await; // flagged
    ::tokio::time::sleep(D).await; // flagged
    tokio::select! { () = tokio::time::sleep_until(T) => {} } // flagged
}
",
    ),
    // Integration tests, and a module importing its parent's names.
    (
        "tests/cli/main.rs",
        "use std::thread;\n#[test]\nfn waits() { thread::sleep(D); } // flagged\n",
    ),
    (
        "tests/cli/child.rs",
        "use super::*;\nfn waits() { thread::sleep(D); } // flagged\n",
    ),
    // Paused time allows tokio's sleeps, from the pause on, but not the
    // thread's.
    (
        "src/b/tests.rs",
        r"
#[tokio::test(start_paused = true)]
async fn paused() {
    tokio::time::sleep(D).await;
    std::thread::sleep(D); // flagged
}
#[tokio::test]
async fn paused_later() {
    tokio::time::sleep(D).await; // flagged
    tokio::time::pause();
    tokio::time::sleep(D).await;
}
#[tokio::test]
async fn not_paused() {
    tokio::time::sleep(D).await; // flagged
}
",
    ),
    // A reason in the comment block directly above allows a wait.
    (
        "src/c/tests.rs",
        r"
fn waits() {
    // wall-clock: a real subprocess signals nothing.
    std::thread::sleep(D);
    // wall-clock: a reason long enough that it
    // wraps onto a second line.
    std::thread::sleep(D);
    // wall-clock:
    std::thread::sleep(D); // flagged
    // wall-clock: not directly above.

    std::thread::sleep(D); // flagged
}
",
    ),
    // Production code sleeps; only its test items and modules are checked.
    (
        "src/lib.rs",
        r#"
#[cfg(test)]
mod fixtures;
#[cfg(any(test, feature = "testing"))]
mod testing;
fn backoff() { std::thread::sleep(D); }
#[cfg(not(test))]
fn real() { std::thread::sleep(D); }
#[cfg(all(test, unix))]
fn unix_only() { std::thread::sleep(D); } // flagged
#[cfg(test)]
mod inline {
    fn helper() { std::thread::sleep(D); } // flagged
}
struct Fixture;
impl Fixture {
    #[test]
    fn waits() { std::thread::sleep(D); } // flagged
}
"#,
    ),
    (
        "src/fixtures.rs",
        "fn waits() { std::thread::sleep(D); } // flagged\n",
    ),
    // Compiled into the crate by its `testing` feature too.
    (
        "src/testing.rs",
        "fn backoff() { std::thread::sleep(D); }\n",
    ),
];

#[test]
fn only_unexplained_sleeps_in_test_code_are_flagged() {
    let sources: Vec<Source> = FILES
        .iter()
        .map(|(path, text)| Source::parse(PathBuf::from(path), (*text).to_owned()).unwrap())
        .collect();
    let mut flagged: Vec<String> = Source::test_waits(&sources)
        .iter()
        .map(|wait| format!("{}:{}", wait.path.display(), wait.line))
        .collect();
    let mut expected: Vec<String> = FILES
        .iter()
        .flat_map(|(path, text)| {
            text.lines()
                .enumerate()
                .filter(|(_, line)| line.ends_with("// flagged"))
                .map(move |(index, _)| format!("{path}:{}", index + 1))
        })
        .collect();
    flagged.sort();
    expected.sort();
    assert_eq!(flagged, expected);
}
