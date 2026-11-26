//! Volumes: the Docker volumes the daemon tracks, and the names environments
//! bind to them.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{
    ApplicationId, ApplicationName, DockerVolumeName, EnvironmentId, EnvironmentName, VolumeId,
    VolumeName,
};

crate::names::string_enum! {
    #[serde]
    /// Where a volume lives.
    pub enum VolumePool {
        /// A Docker local volume on the daemon's host.
        Local => "local",
    }
}

crate::names::string_enum! {
    #[serde]
    /// Whether an environment uses a volume.
    pub enum VolumeState {
        /// An environment binds one of its manifest volume names to it.
        Attached => "attached",
        /// No environment binds it: its environment was deleted, or the
        /// daemon found it in Docker bound to no name. It keeps its data
        /// until it is attached again or deleted.
        Detached => "detached",
    }
}

/// A Docker volume the daemon tracks. An environment binds each of its
/// manifest volume names to one, so a name can later point at another
/// volume without touching the data of the first.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct VolumeView {
    /// Stable volume identifier.
    pub id: VolumeId,
    /// Its Docker name: derived from its ID, or for volumes created before
    /// volumes were tracked, from its first environment and name.
    pub docker_name: DockerVolumeName,
    /// Where it lives.
    pub pool: VolumePool,
    /// Whether an environment binds a name to it.
    pub state: VolumeState,
    /// The environment and name it is bound to, while attached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<VolumeBinding>,
    /// Where it was bound when its environment was deleted, while detached,
    /// if known. Volumes adopted from Docker after their environment was
    /// deleted have none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detached_from: Option<DetachedFrom>,
    /// Disk space it used when last measured, in bytes, if Docker could
    /// measure it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    /// When its size was last measured, in Unix milliseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub measured_at_ms: Option<i64>,
    /// When the daemon created or adopted it, in Unix milliseconds.
    pub created_at_ms: i64,
}

impl VolumeView {
    /// Where it is bound, or where it came from while detached:
    /// `shop/production as data`, `from shop/staging as data`, or `from an
    /// unknown environment`.
    #[must_use]
    pub fn whereabouts(&self) -> String {
        if let Some(binding) = &self.binding {
            return format!(
                "{}/{} as {}",
                binding.application, binding.environment, binding.name
            );
        }
        match &self.detached_from {
            Some(DetachedFrom {
                application,
                environment,
                name: Some(name),
            }) => format!("from {application}/{environment} as {name}"),
            Some(DetachedFrom {
                application,
                environment,
                name: None,
            }) => format!("from {application}/{environment}"),
            None => "from an unknown environment".to_owned(),
        }
    }

    /// Its measured size, such as `48.2 MB`, or `-` before it is measured.
    #[must_use]
    pub fn size(&self) -> String {
        self.size_bytes.map_or_else(
            || "-".to_owned(),
            |bytes| crate::observability::Bytes(bytes).to_string(),
        )
    }
}

/// The environment and manifest volume name an attached volume is bound to.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct VolumeBinding {
    /// The application the environment belongs to.
    pub application_id: ApplicationId,
    /// Its name.
    pub application: ApplicationName,
    /// The environment.
    pub environment_id: EnvironmentId,
    /// Its name.
    pub environment: EnvironmentName,
    /// The manifest volume name bound to the volume.
    pub name: VolumeName,
}

/// Where a detached volume was bound when its environment was deleted, by
/// name, since the environment no longer exists.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct DetachedFrom {
    /// The application's name.
    pub application: ApplicationName,
    /// The environment's name.
    pub environment: EnvironmentName,
    /// The manifest volume name, unless only the environment is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<VolumeName>,
}

/// Attaches a detached volume to an environment under a manifest volume
/// name that is not bound yet.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AttachVolumeRequest {
    /// The detached volume to attach.
    pub volume: VolumeId,
}
