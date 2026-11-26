//! Image reference syntax: registries, repositories, tags and digests.

use crate::names::is_lowercase_label;
use crate::resource::Sha256Digest;

/// The longest image reference accepted.
const MAX_REFERENCE_LEN: usize = 512;
/// The longest repository name, with its registry, accepted.
const MAX_NAME_LEN: usize = 255;
/// The longest tag Docker accepts.
const MAX_TAG_LEN: usize = 128;

/// An image reference split into `[registry/]path[:tag][@digest]`, the one
/// place its grammar is read.
///
/// ```text
/// "GHCR.io:443/org/app:v1@sha256:abc…"
///   -> registry "GHCR.io:443", name "GHCR.io:443/org/app", tag "v1", digest "sha256:abc…"
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ImageParts<'a> {
    /// The whole reference.
    value: &'a str,
    /// The repository as written, with its registry and without its tag or
    /// digest.
    pub(crate) name: &'a str,
    /// The registry host and port, when `name` starts with one: a first
    /// component containing `.` or `:`, or `localhost`.
    registry: Option<&'a str>,
    /// The tag, after the last `:` that follows the last `/`.
    tag: Option<&'a str>,
    /// The digest, after `@`.
    pub(crate) digest: Option<&'a str>,
}

impl<'a> ImageParts<'a> {
    /// Splits `value` without checking it.
    pub(crate) fn split(value: &'a str) -> Self {
        let (name_and_tag, digest) = value
            .split_once('@')
            .map_or((value, None), |(name, digest)| (name, Some(digest)));
        let last_slash = name_and_tag.rfind('/');
        let (name, tag) = match name_and_tag.rfind(':') {
            Some(colon) if last_slash.is_none_or(|slash| colon > slash) => {
                (&name_and_tag[..colon], Some(&name_and_tag[colon + 1..]))
            }
            _ => (name_and_tag, None),
        };
        let registry = name.split_once('/').and_then(|(first, _)| {
            (first.contains(['.', ':']) || first.eq_ignore_ascii_case("localhost")).then_some(first)
        });
        Self {
            value,
            name,
            registry,
            tag,
            digest,
        }
    }

    /// Splits `value` when it follows Docker's image reference grammar, with
    /// extra hardening: no whitespace, control characters, URL schemes
    /// (`//`), `?` or `#`.
    ///
    /// ```text
    /// "nginx", "ghcr.io/org/app:v1", "localhost:5000/app@sha256:..."  -> Some
    /// "https://ghcr.io/app", "Nginx", "app:", "a//b"                  -> None
    /// ```
    pub(crate) fn parse(value: &'a str) -> Option<Self> {
        let parts = Self::split(value);
        let valid = !value.is_empty()
            && value.len() <= MAX_REFERENCE_LEN
            && !value
                .chars()
                .any(|character| character.is_whitespace() || character.is_control())
            && !value.contains("//")
            && !value.contains(['?', '#'])
            && parts.digest.is_none_or(valid_image_digest)
            && parts.tag.is_none_or(valid_tag)
            && !parts.name.is_empty()
            && parts.name.len() <= MAX_NAME_LEN
            && parts.registry.is_none_or(valid_registry_authority)
            && parts.path().split('/').all(valid_repository_component);
        valid.then_some(parts)
    }

    /// The reference with its registry host lowercased; other parts are
    /// case-sensitive.
    ///
    /// ```text
    /// "GHCR.io/org/app:v1" -> "ghcr.io/org/app:v1"
    /// ```
    pub(crate) fn canonical(&self) -> String {
        match self.registry {
            Some(registry) => format!(
                "{}{}",
                registry.to_ascii_lowercase(),
                &self.value[registry.len()..]
            ),
            None => self.value.to_owned(),
        }
    }

    /// The canonical repository, without tag or digest, with Docker Hub's
    /// defaults applied so equivalent spellings compare equal.
    ///
    /// ```text
    /// "nginx:1.27"                    -> "docker.io/library/nginx"
    /// "user/app@sha256:..."           -> "docker.io/user/app"
    /// "index.docker.io/nginx"         -> "docker.io/library/nginx"
    /// "Registry.io:5000/team/app:v1"  -> "registry.io:5000/team/app"
    /// ```
    pub(crate) fn repository(&self) -> String {
        let path = match self.registry {
            Some(registry)
                if registry.eq_ignore_ascii_case("docker.io")
                    || registry.eq_ignore_ascii_case("index.docker.io") =>
            {
                self.path()
            }
            Some(registry) => return format!("{}/{}", registry.to_ascii_lowercase(), self.path()),
            None => self.name,
        };
        if path.contains('/') {
            format!("docker.io/{path}")
        } else {
            format!("docker.io/library/{path}")
        }
    }

    /// Whether the reference is pinned by a SHA-256 digest of 64 lowercase
    /// hexadecimal digits.
    pub(crate) fn is_pinned(&self) -> bool {
        self.digest
            .is_some_and(|digest| Sha256Digest::parse(digest).is_ok())
    }

    /// The repository path, after the registry.
    fn path(&self) -> &'a str {
        self.registry
            .map_or(self.name, |registry| &self.name[registry.len() + 1..])
    }
}

/// Whether a tag is 1-128 letters, digits, `_`, `.` or `-`, starting with a
/// letter, digit or `_`.
fn valid_tag(tag: &str) -> bool {
    tag.len() <= MAX_TAG_LEN
        && tag
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && tag
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
}

/// Whether a registry authority is `localhost` or DNS-like labels, with an
/// optional nonzero port of decimal digits only.
fn valid_registry_authority(value: &str) -> bool {
    let (host, port) = value
        .rsplit_once(':')
        .map_or((value, None), |(host, port)| (host, Some(port)));
    if port.is_some_and(|port| {
        port.is_empty()
            || !port.bytes().all(|byte| byte.is_ascii_digit())
            || port.parse::<u16>().map_or(true, |port| port == 0)
    }) {
        return false;
    }
    let lowered = host.to_ascii_lowercase();
    lowered == "localhost"
        || lowered
            .split('.')
            .all(|label| is_lowercase_label(label, 1..=usize::MAX))
}

/// Whether a repository path component is lowercase alphanumeric runs joined by
/// one of the Docker separators: `.`, `_`, `__`, or one or more `-`.
fn valid_repository_component(value: &str) -> bool {
    let bytes = value.as_bytes();
    if !bytes
        .first()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        || !bytes
            .last()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return false;
    }
    let mut index = 0;
    while index < bytes.len() {
        while index < bytes.len()
            && (bytes[index].is_ascii_lowercase() || bytes[index].is_ascii_digit())
        {
            index += 1;
        }
        if index == bytes.len() {
            return true;
        }
        let separator_start = index;
        while index < bytes.len() && matches!(bytes[index], b'.' | b'_' | b'-') {
            index += 1;
        }
        let separator = &value[separator_start..index];
        if separator.is_empty()
            || index == bytes.len()
            || !(separator == "."
                || separator == "_"
                || separator == "__"
                || separator.bytes().all(|byte| byte == b'-'))
        {
            return false;
        }
    }
    true
}

/// Whether a value matches the OCI digest grammar `algorithm:encoded`.
fn valid_image_digest(value: &str) -> bool {
    let Some((algorithm, encoded)) = value.split_once(':') else {
        return false;
    };
    let algorithm_parts = algorithm.split(['_', '+', '.', '-']).collect::<Vec<_>>();
    algorithm_parts.iter().all(|part| {
        part.bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
            && part
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    }) && encoded.len() >= 32
        && encoded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'=' | b'_' | b'-'))
}

#[cfg(test)]
mod tests;
