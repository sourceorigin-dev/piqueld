use super::{
    ApplicationView, BuildLogChunk, DeploymentPage, EnvironmentAccess, EnvironmentView, LogRecord,
    LogTime, MountedSecret, SecretAccess, SecretMetadata, StoredSecret,
};

use crate::{ApplicationId, EnvironmentId, EnvironmentName, EnvironmentSource};

#[test]
fn incomplete_characters_wait_for_the_next_read() {
    assert_eq!(BuildLogChunk::complete_prefix(b"ok\xe2\x82"), 2);
    assert_eq!(BuildLogChunk::complete_prefix(b"\xffok\xe2\x82"), 3);
    assert_eq!(BuildLogChunk::complete_prefix("ok€".as_bytes()), 5);
}

fn environment(id: &str, name: &str) -> EnvironmentView {
    EnvironmentView {
        id: EnvironmentId::parse(id).expect("the environment id is valid"),
        application_id: ApplicationId::parse("app-notes-01").expect("the application id is valid"),
        name: EnvironmentName::parse(name).expect("the environment name is valid"),
        source: EnvironmentSource::Saved,
        kind: crate::EnvironmentKind::Environment,
        sync: true,
        synced: None,
        telemetry: false,
        run_state: crate::api::RunState::Running,
        resolved_generation: None,
        delete_intent: false,
        created_at_ms: 1,
        updated_at_ms: 1,
    }
}

#[test]
fn stable_ids_select_before_names_that_look_like_them() {
    let manifest = crate::manifest::parse_template_toml(
        "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='notes'\n[spec]",
    )
    .expect("the template toml parses")
    .normalize(ApplicationId::parse("app-notes-01").expect("the application id is valid"));
    let view = ApplicationView {
        spec_hash: manifest.spec_hash(),
        application: manifest,
        generation: 1,
        delete_intent: false,
        created_at_ms: 1,
        updated_at_ms: 1,
        environments: vec![
            environment("env-impostor-01", "app-notes-01"),
            environment("app-notes-01", "production"),
        ],
        previews: Vec::new(),
        drift: Vec::new(),
        sync_check: None,
        repository: None,
    };
    assert_eq!(
        view.environment("app-notes-01")
            .expect("the environment exists")
            .name
            .as_str(),
        "production"
    );
    assert_eq!(
        view.environment("production")
            .expect("the environment exists")
            .id
            .as_str(),
        "app-notes-01"
    );
}

/// Each mounted secret reports what its next deployment would find, so
/// failures show before deploying.
#[test]
fn mounted_secrets_report_what_deploying_would_find() {
    let manifest = crate::manifest::parse_template_toml(
        r"api_version='piqueld.dev/v1alpha1'
kind='Application'
[metadata]
name='notes'
[[spec.services]]
name='web'
source={type='image',image='nginx:alpine'}
secrets=[{name='session',target='/run/secrets/1'},{name='stripe',target='/run/secrets/2'},{name='other',target='/run/secrets/3'},{name='discarded',target='/run/secrets/4'},{name='unset',target='/run/secrets/5'}]
[[spec.secrets]]
name='session'
generate={type='random',bytes=16}
",
    )
    .expect("the template toml parses")
    .normalize(ApplicationId::parse("app-notes-01").expect("the application id is valid"));
    let production = environment("app-notes-01", "production");
    let stored = |name: &str, environments, unavailable| StoredSecret {
        access: SecretAccess {
            environments,
            previews: false,
        },
        metadata: SecretMetadata {
            name: name.into(),
            generation: 3,
            updated_at_ms: 1,
            deleting: false,
            unavailable,
        },
    };
    let others = EnvironmentAccess::Only(
        [EnvironmentId::parse("env-staging-01").expect("the environment id is valid")].into(),
    );
    let stored = [
        stored("stripe", EnvironmentAccess::All, false),
        stored("other", others, false),
        stored("discarded", EnvironmentAccess::All, true),
    ];
    assert_eq!(
        MountedSecret::list(&manifest, &production, &stored),
        [
            ("discarded".into(), MountedSecret::Unavailable),
            ("other".into(), MountedSecret::Denied),
            ("session".into(), MountedSecret::Generated),
            ("stripe".into(), MountedSecret::Stored { generation: 3 }),
            ("unset".into(), MountedSecret::Missing),
        ]
    );
}

/// Terminal controls, bidirectional overrides and zero-width characters go,
/// while text, lines and an emoji sequence's joiner stay.
#[test]
fn terminal_controls_are_removed_but_text_and_lines_survive() {
    assert_eq!(
        LogRecord::clean_message("\x1b[31mred\x1b[0m\r\0\ttext\n"),
        "red\ttext\n"
    );
    assert_eq!(
        LogRecord::clean_message(
            "\x1b]0;hidden title\x07text \x1b]8;;https://example.test\x1b\\link\x1b]8;;\x1b\\"
        ),
        "text link"
    );
    assert_eq!(
        LogRecord::clean_message("evil\u{202E}gpj.exe\u{200B}\u{FEFF} 👩\u{200D}💻"),
        "evilgpj.exe 👩\u{200D}💻"
    );
}

/// A whole page also matches the summary schema, and an empty page matches
/// both, so a deployment page is documented as `anyOf`, never `oneOf`.
#[test]
fn deployment_pages_are_documented_as_any_of() {
    let schema = serde_json::to_value(<DeploymentPage as utoipa::PartialSchema>::schema())
        .expect("schema JSON");
    assert!(schema.get("oneOf").is_none());
    assert_eq!(schema["anyOf"].as_array().map(Vec::len), Some(2));
}

#[test]
fn log_times_read_and_write_docker_timestamps() {
    for (timestamp, nanos) in [
        ("1970-01-01T00:00:00Z", Some(0)),
        (
            "2026-01-01T00:00:00.000000001Z",
            Some(1_767_225_600_000_000_001),
        ),
        ("2024-02-29T23:59:59.5Z", Some(1_709_251_199_500_000_000)),
        ("2026-01-01T00:00:00+01:00", None),
        ("2026-13-01T00:00:00Z", None),
        ("9223372036854775807-01-01T00:00:00Z", None),
        ("1970-01-01T9223372036854775807:00:00Z", None),
        ("1970-01-01T00:-9223372036854775808:00Z", None),
        ("not a timestamp", None),
    ] {
        assert_eq!(LogTime::parse(timestamp), nanos.map(LogTime), "{timestamp}");
    }
    assert_eq!(
        LogTime(1_709_251_199_500_000_000).to_string(),
        "2024-02-29T23:59:59.500000000Z"
    );
}
