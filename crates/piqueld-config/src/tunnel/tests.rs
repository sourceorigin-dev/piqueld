use super::*;
use crate::DaemonConfig;

const ID: &str = "6ff42ae2-765d-4adf-8112-31c55c1551ef";

/// Parses `[ingress.tunnel]` with `credentials` in its credentials file.
fn load(credentials: &str) -> Result<TunnelConfig, String> {
    let file = tempfile::NamedTempFile::new().expect("the temp file is created");
    std::fs::write(file.path(), credentials).expect("the file is written");
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
    .expect("the tunnel credentials load");
    let credentials = tunnel.credentials.expect("the tunnel has credentials");
    assert_eq!(credentials.id.to_string(), ID);
    assert_eq!(credentials.hostname(), format!("{ID}.cfargotunnel.com"));
    let disabled = DaemonConfig::from_toml(
        "[ingress.tunnel]\nenabled = false\ncredentials_file = '/missing/credentials.json'",
    )
    .expect("the disabled tunnel parses");
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
        let error = load(&credentials).expect_err("the credentials are invalid");
        assert!(error.contains(cause), "{error}");
        assert!(!error.contains("hunter2"), "{error}");
    }
    let error = DaemonConfig::from_toml("[ingress.tunnel]\nenabled = true")
        .expect_err("the config is invalid");
    assert!(format!("{:#}", anyhow::Error::new(error)).contains("credentials_file is required"));
}
