use super::*;
use piqueld_client::{BuildState, LogStream};
use serde_json::json;

/// Build `id` in `state`.
fn build(id: i64, state: &str) -> BuildRecord {
    serde_json::from_value(json!({
        "id": id,
        "environment_id": "app-shop0001",
        "operation_id": format!("op-{id}"),
        "service": "web",
        "source": { "type": "image", "image": "nginx:alpine" },
        "state": state,
        "started_at_ms": id,
        "finished_at_ms": null,
        "commit": null,
        "image_id": null,
        "log_bytes": 0,
        "log_truncated": false,
        "log_expired": false,
    }))
    .expect("build")
}

#[test]
fn a_refreshed_first_page_keeps_older_pages() {
    let mut builds = vec![
        build(9, "running"),
        build(8, "succeeded"),
        build(5, "failed"),
        build(4, "succeeded"),
    ];
    builds.merge_newest(vec![build(10, "running"), build(9, "succeeded")]);
    assert_eq!(
        builds
            .iter()
            .map(|build| (build.id, build.state))
            .collect::<Vec<_>>(),
        [
            (10, BuildState::Running),
            (9, BuildState::Succeeded),
            (8, BuildState::Succeeded),
            (5, BuildState::Failed),
            (4, BuildState::Succeeded),
        ]
    );
}

#[test]
fn build_arguments_quote_values_with_spaces_and_quotes() {
    let args = BTreeMap::from([
        ("NAME".to_owned(), "two words"),
        ("GREETING".to_owned(), r#"say "hi""#),
    ]);
    assert_eq!(
        build_arguments(&args),
        r#"GREETING="say \"hi\"" NAME="two words""#
    );
}

/// A stdout chunk of `text` at byte `offset`.
fn chunk(offset: i64, text: &str) -> BuildLogChunk {
    BuildLogChunk {
        offset,
        timestamp_ms: offset,
        stream: LogStream::Stdout,
        text: text.to_owned(),
    }
}

/// Each chunk's offset and text.
fn shown(chunks: &[BuildLogChunk]) -> Vec<(i64, &str)> {
    chunks
        .iter()
        .map(|chunk| (chunk.offset, chunk.text.as_str()))
        .collect()
}

#[test]
fn refetched_newest_output_keeps_older_chunks() {
    let mut chunks = vec![
        chunk(0, "a"),
        chunk(10, "b"),
        chunk(20, "c"),
        chunk(30, "d"),
    ];
    let kept = chunks.merge_newest(vec![chunk(20, "c"), chunk(30, "d+"), chunk(40, "e")]);
    assert!(kept);
    assert_eq!(
        shown(&chunks),
        [(0, "a"), (10, "b"), (20, "c"), (30, "d+"), (40, "e")]
    );
}

#[test]
fn refetched_newest_output_alone_keeps_its_own_cursor() {
    let mut chunks = vec![chunk(20, "c")];
    assert!(!chunks.merge_newest(vec![chunk(20, "c"), chunk(30, "d")]));
    assert_eq!(shown(&chunks), [(20, "c"), (30, "d")]);
}
