//! `[ingress.tunnel]`: serves public routes through a locally managed
//! Cloudflare Tunnel instead of the host's ports 80 and 443.
//!
//! ```toml
//! [ingress.tunnel]
//! enabled = true
//! credentials_file = "cloudflared-tunnel.json"   # from `cloudflared tunnel create piqueld`
//! ```
//!
//! The credentials file is read once at startup, like other `_file` settings,
//! and must hold the tunnel's `AccountTag`, `TunnelSecret` and `TunnelID`.

use super::{Credential, CredentialError, CredentialFile, schema_from_raw};
use serde::Deserialize;
use std::path::PathBuf;
use thiserror::Error;
use uuid::Uuid;

/// The Cloudflare Tunnel public routes arrive through.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(try_from = "RawTunnelConfig")]
pub struct TunnelConfig {
    /// The tunnel's credentials while it is enabled, `None` while disabled.
    /// Disabled, public routes are served on ports 80 and 443.
    pub credentials: Option<TunnelCredentials>,
}

/// A tunnel's credentials file, as `cloudflared tunnel create` writes it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TunnelCredentials {
    /// The file's `TunnelID`.
    pub id: Uuid,
    /// The whole file, which `cloudflared` reads. Never log or display it.
    pub file: Credential,
}

impl TunnelCredentials {
    /// The tunnel's DNS name, which public hostnames need a proxied CNAME to.
    ///
    /// ```text
    /// 6ff42ae2-765d-4adf-8112-31c55c1551ef.cfargotunnel.com
    /// ```
    #[must_use]
    pub fn hostname(&self) -> String {
        format!("{}.cfargotunnel.com", self.id)
    }

    /// Reads and checks the credentials file, unless a structural check
    /// leaves it unread.
    fn read(file: CredentialFile) -> Result<Self, TunnelConfigError> {
        /// The fields `cloudflared` requires. Secrets are only checked for
        /// presence, so decoding errors never quote them.
        #[derive(Deserialize)]
        #[expect(dead_code, reason = "fields are only checked for presence")]
        struct Fields {
            #[serde(rename = "AccountTag")]
            account: serde::de::IgnoredAny,
            #[serde(rename = "TunnelSecret")]
            secret: serde::de::IgnoredAny,
            #[serde(rename = "TunnelID")]
            id: String,
        }
        let path = file.path().to_owned();
        let file = Credential::read("credentials", file)?;
        // A structural check leaves the file unread; its configuration is
        // never used, so the ID is a placeholder.
        let Some(contents) = file.value() else {
            return Ok(Self {
                id: Uuid::nil(),
                file,
            });
        };
        let fields: Fields = serde_json::from_str(contents)
            .map_err(|error| TunnelConfigError::Decode { path, error })?;
        let id = Uuid::parse_str(&fields.id).map_err(|error| TunnelConfigError::Id {
            id: fields.id,
            error,
        })?;
        Ok(Self { id, file })
    }
}

/// A Cloudflare Tunnel that serves public routes instead of the host's ports
/// 80 and 443, which are then closed. Each public hostname needs a proxied
/// CNAME record to `<tunnel-id>.cfargotunnel.com`, and Cloudflare sees
/// public traffic in plaintext.
//
// The table as written.
#[derive(Default, Deserialize, utoipa::ToSchema)]
#[serde(default, deny_unknown_fields)]
struct RawTunnelConfig {
    /// Serve public routes through the tunnel. Requires `credentials_file`.
    enabled: bool,
    /// The tunnel's credentials file, which `cloudflared tunnel create`
    /// writes. piqueld reads it only while the tunnel is enabled.
    credentials_file: Option<CredentialFile>,
}

schema_from_raw!(TunnelConfig => RawTunnelConfig);

impl TryFrom<RawTunnelConfig> for TunnelConfig {
    type Error = TunnelConfigError;
    fn try_from(raw: RawTunnelConfig) -> Result<Self, Self::Error> {
        if !raw.enabled {
            return Ok(Self::default());
        }
        let file = raw
            .credentials_file
            .ok_or(TunnelConfigError::MissingCredentials)?;
        Ok(Self {
            credentials: Some(TunnelCredentials::read(file)?),
        })
    }
}

/// `[ingress.tunnel]` could not be loaded. Messages never contain the secret.
#[derive(Debug, Error)]
pub enum TunnelConfigError {
    /// The tunnel is enabled without credentials.
    #[error("ingress.tunnel.credentials_file is required while the tunnel is enabled")]
    MissingCredentials,
    /// The credentials file could not be read.
    #[error(transparent)]
    Credential(#[from] CredentialError),
    /// The file is not a tunnel credentials file, such as `cert.pem`.
    #[error("{} is not a cloudflared tunnel credentials file: {error}", path.display())]
    Decode {
        /// Resolved path.
        path: PathBuf,
        /// What is missing or malformed.
        error: serde_json::Error,
    },
    /// `TunnelID` is not a UUID.
    #[error("the tunnel credentials' TunnelID {id:?} is not a UUID")]
    Id {
        /// The configured ID, which is not secret.
        id: String,
        /// Why it is not a UUID.
        #[source]
        error: uuid::Error,
    },
}

#[cfg(test)]
mod tests;
