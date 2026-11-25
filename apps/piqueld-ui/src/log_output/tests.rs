use super::*;
#[test]
fn interleaved_chunks_keep_partial_lines_and_severity() {
    let chunk = |offset, stream, text: &str| BuildLogChunk {
        offset,
        stream,
        timestamp_ms: 1000,
        text: text.into(),
    };
    let lines = LogLine::build(
        &[
            chunk(0, LogStream::Stdout, "hel"),
            chunk(3, LogStream::Stderr, "progress\n"),
            chunk(12, LogStream::Stdout, "lo\n\x1b[31mERROR\x1b[0m failed\n"),
        ],
        "web",
    );
    assert_eq!(
        lines.iter().map(|l| l.message.as_str()).collect::<Vec<_>>(),
        ["hello", "progress", "ERROR failed"]
    );
    assert_eq!(lines[1].severity(), "stderr");
    assert_eq!(lines[2].severity(), "error");
}

#[test]
fn build_lines_keep_the_timestamp_of_their_first_chunk() {
    let chunks = [
        (0, LogStream::Stdout, "first\nse"),
        (8, LogStream::Stderr, "warn"),
        (12, LogStream::Stdout, "co"),
        (14, LogStream::Stdout, "nd\nthird\nfour"),
        (27, LogStream::Stderr, "ing\n"),
        (31, LogStream::Stdout, "th\nfifth"),
    ]
    .map(|(offset, stream, text)| BuildLogChunk {
        offset,
        stream,
        timestamp_ms: offset,
        text: text.into(),
    });
    let lines = LogLine::build(&chunks, "web");
    assert_eq!(
        lines
            .iter()
            .map(|line| (line.message.as_str(), line.timestamp.as_str()))
            .collect::<Vec<_>>(),
        [
            ("first", "0"),
            ("second", "0"),
            ("warning", "8"),
            ("third", "14"),
            ("fourth", "14"),
            ("fifth", "31")
        ],
    );
}
