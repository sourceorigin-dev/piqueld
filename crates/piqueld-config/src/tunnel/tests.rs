use super::*;
use crate::DaemonConfig;

const ID: &str = "6ff42ae2-765d-4adf-8112-31c55c1551ef";

/// Parses `[ingress.tunnel]` with `credentials` in its credentials file.
fn load(credentials: &str) -> Result<TunnelConfig, String> {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), credentials).unwrap();
    let document = format!(
        "[ingress.tunnel]\nenabled = true\ncredentials_file = '{}'",
        file.path().display()
    );
    DaemonConfig::from_toml(&document)
        .map(|config| config.ingress.tunnel)
        .map_err(|error| format!("{:#}", anyhow::Error::new(error)))
}

#[test]
fn credentials_are_read_once_enabled() {
    let tunnel = load(&format!(
        r#"{{"AccountTag":"account","TunnelSecret":"c2VjcmV0","TunnelID":"{ID}"}}"#
    ))
    .unwrap();
    let credentials = tunnel.credentials.unwrap();
    assert_eq!(credentials.id.to_string(), ID);
    assert_eq!(credentials.hostname(), format!("{ID}.cfargotunnel.com"));
    // Disabled, the file is not read at all.
    let disabled = DaemonConfig::from_toml(
        "[ingress.tunnel]\nenabled = false\ncredentials_file = '/missing/credentials.json'",
    )
    .unwrap();
    assert_eq!(disabled.ingress.tunnel, TunnelConfig::default());
}

#[test]
fn invalid_credentials_are_rejected_without_quoting_the_secret() {
    for (credentials, cause) in [
        (
            r#"{"AccountTag":"a","TunnelID":"x"}"#.to_owned(),
            "TunnelSecret",
        ),
        (
            r#"{"AccountTag":"a","TunnelSecret":"hunter2","TunnelID":"tunnel"}"#.into(),
            "not a UUID",
        ),
        (
            "-----BEGIN ARGO TUNNEL TOKEN-----".into(),
            "not a cloudflared",
        ),
    ] {
        let error = load(&credentials).unwrap_err();
        assert!(error.contains(cause), "{error}");
        assert!(!error.contains("hunter2"), "{error}");
    }
    let error = DaemonConfig::from_toml("[ingress.tunnel]\nenabled = true").unwrap_err();
    assert!(format!("{:#}", anyhow::Error::new(error)).contains("credentials_file is required"));
}
