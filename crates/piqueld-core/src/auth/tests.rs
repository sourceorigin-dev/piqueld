use super::*;

/// Secrets never appear in `Debug` output; the other fields do.
#[test]
fn debug_output_redacts_secrets() {
    let token = DeviceToken {
        status: "complete".into(),
        token: Some("pqd_secret".into()),
        user: None,
    };
    assert_eq!(
        format!("{token:?}"),
        r#"DeviceToken { status: "complete", user: None, token: <redacted> }"#
    );
}

/// Account action descriptions quote and escape what the caller sent, so
/// it cannot forge lines in the audit trail, and mark revocations that
/// don't cascade.
#[test]
fn descriptions_escape_caller_values() {
    let revoke = |cascade| Manage::RevokeCredential {
        id: "a\nb".into(),
        cascade,
    };
    assert_eq!(revoke(true).describe(), r#"revoke_credential "a\nb""#);
    assert_eq!(
        revoke(false).describe(),
        r#"revoke_credential "a\nb" (no cascade)"#
    );
}

/// An omitted `cascade` cascades, so only an explicit `false` opts out.
#[test]
fn revocations_cascade_unless_asked_not_to() {
    let omitted: Manage = serde_json::from_str(r#"{"action":"revoke_credential","id":"a"}"#)
        .expect("a revocation without cascade parses");
    assert!(matches!(
        omitted,
        Manage::RevokeCredential { cascade: true, .. }
    ));
}
