//! Backend-neutral desired, resolved, and observed Docker resource contracts.
//!
//! A deployment resolves its sources (`resolution`), compiles the
//! application into its desired resources (`application`, `desired`), and
//! compares them with what Docker reports (`observed`), using ownership
//! labels (`ownership`) to tell its own resources apart.
//! Everything is re-exported here, so callers name them `resource::<Type>`.

mod application;
mod desired;
mod observed;
mod ownership;
mod resolution;

pub use application::*;
pub use desired::*;
pub use observed::*;
pub use ownership::*;
pub use resolution::*;

use crate::manifest::ImageParts;
use crate::names::validated_string;

/// Label marking a resource as managed by piqueld.
pub const MANAGED_LABEL: &str = "io.piqueld.managed";
/// Label carrying the control-plane instance identity.
pub const INSTANCE_LABEL: &str = "io.piqueld.instance";
/// Label carrying the application identity.
pub const APPLICATION_LABEL: &str = "io.piqueld.application";
/// Label carrying the logical service identity.
pub const SERVICE_LABEL: &str = "io.piqueld.service";
/// Label carrying the normalized application spec hash.
pub const SPEC_HASH_LABEL: &str = "io.piqueld.spec-hash";
/// Label carrying the logical job identity of a one-shot job service.
pub const JOB_LABEL: &str = "io.piqueld.job";
/// Label carrying the operation that started a one-shot job run.
pub const JOB_OPERATION_LABEL: &str = "io.piqueld.job-operation";
/// Label marking the Swarm configs piqueld publishes for service files.
pub const FILE_LABEL: &str = "io.piqueld.file";
/// Label on a link network naming the exporting environment; the importing
/// environment owns the network.
pub const LINK_LABEL: &str = "io.piqueld.link";
/// Label marking the containers piqueld runs itself for ingress (the
/// gateway, the apps node and `cloudflared`) and their egress network.
pub const INGRESS_LABEL: &str = "io.piqueld.ingress";
/// Environment variable listing the ingress network's CIDRs in routed services.
/// Only the gateway and the application's own routed services share that
/// network, so `X-Forwarded-For` from these peers carries the client address.
pub const INGRESS_PROXIES_ENV: &str = "PIQUELD_INGRESS_PROXIES";
/// Where a telemetry receiver's socket directory is mounted; the collector
/// listens on `otlp.sock` inside it.
pub const TELEMETRY_TARGET: &str = "/run/piqueld/telemetry";

validated_string!(
    /// Stable control-plane instance identity.
    InstanceId,
    InstanceIdError,
    "instance IDs must be 1-64 lowercase ASCII letters, digits, or internal hyphens",
    |value: &str| crate::names::is_lowercase_label(value, 1..=64)
);

validated_string!(
    /// Explicitly tagged lowercase SHA-256 digest.
    Sha256Digest,
    Sha256DigestError,
    "SHA-256 digests must use the sha256:<64 lowercase hexadecimal digits> format",
    valid_sha256
);

/// Returns whether a logical resource name is safe for Docker naming.
///
/// Names are 1-63 bytes of lowercase ASCII letters, digits, or hyphens,
/// starting with a letter and not ending with a hyphen.
///
/// ```text
/// "web", "api-2"         -> valid
/// "2web", "Web", "web-"  -> invalid
/// ```
#[must_use]
pub fn valid_logical_name(value: &str) -> bool {
    crate::names::is_lowercase_label(value, crate::names::DNS_LABEL)
        && value.starts_with(|c: char| c.is_ascii_lowercase())
}

/// Returns the canonical repository portion of a valid image reference,
/// without tag or digest, with Docker Hub's defaults applied so equivalent
/// spellings compare equal.
///
/// ```text
/// "nginx:1.27"                    -> "docker.io/library/nginx"
/// "Registry.io:5000/team/app:v1"  -> "registry.io:5000/team/app"
/// ```
#[must_use]
pub fn image_repository(reference: &str) -> Option<String> {
    ImageParts::parse(reference).map(|parts| parts.repository())
}

/// Whether a value is `sha256:` followed by 64 lowercase hex digits.
fn valid_sha256(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

#[cfg(test)]
mod tests {
    use super::{InstanceId, InstanceIdError, Sha256Digest, Sha256DigestError};

    #[test]
    fn identity_types_validate_at_boundaries() {
        assert_eq!(
            InstanceId::parse("UPPERCASE").expect_err("the instance id is rejected"),
            InstanceIdError
        );
        serde_json::from_str::<InstanceId>(r#""instance-1""#)
            .expect("`serde_json::from_str` should succeed");
        serde_json::from_str::<InstanceId>(r#""-invalid""#)
            .expect_err("`serde_json::from_str` should be rejected");
        let digest = format!("sha256:{}", "a".repeat(64));
        assert_eq!(
            Sha256Digest::parse(&digest)
                .expect("the sha256 digest is valid")
                .as_str(),
            digest
        );
        assert_eq!(
            Sha256Digest::parse("sha256:bad").expect_err("the sha256 digest is rejected"),
            Sha256DigestError
        );
    }
}
