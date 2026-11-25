//! `piqueld-probe` in services: the daemon embeds the static probe and
//! publishes it as a Swarm config, which services whose health check runs it
//! mount at [`piqueld_probe::CONTAINER_PATH`]. Swarm stores and distributes
//! the bytes, so nothing depends on host paths.

use super::configs::ConfigKind;
use super::{BollardDocker, DockerError, TaskSpecContainerSpec};
use bollard::models::{TaskSpecContainerSpecConfigs, TaskSpecContainerSpecFile1};
use piqueld_core::{
    InstanceId,
    resource::{INSTANCE_LABEL, MANAGED_LABEL},
};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, sync::LazyLock};

/// The probe the build script compiled for the daemon's architecture.
const BINARY: &[u8] = include_bytes!(env!("PIQUELD_PROBE_BINARY"));

/// The embedded probe's Swarm config. Configs are immutable, so it is named by
/// content: a daemon whose probe differs publishes a new config, services roll
/// onto it through their health-gated update, and image cleanup removes the
/// old one once no service mounts it. Each installation publishes its own,
/// labelled with its instance, so one sharing the Docker Engine, such as a
/// development instance, never removes another's.
pub(super) struct HealthProbe;

impl HealthProbe {
    /// Label marking the configs piqueld publishes for the probe.
    pub(super) const LABEL: &str = "io.piqueld.health-probe";
    /// Root-owned, and readable and executable by every container user.
    const MODE: u32 = 0o555;

    /// `piqueld-health-probe-<first 16 hex digits of the SHA-256 of the
    /// instance ID, a NUL, and the binary's SHA-256>`. Instance IDs are up to
    /// 64 characters, too long to spell out within Swarm's name limit.
    fn name(instance: &str) -> String {
        static BINARY_DIGEST: LazyLock<[u8; 32]> = LazyLock::new(|| Sha256::digest(BINARY).into());
        let digest = Sha256::new()
            .chain_update(instance)
            .chain_update([0])
            .chain_update(*BINARY_DIGEST)
            .finalize();
        format!("piqueld-health-probe-{}", hex::encode(&digest[..8]))
    }

    /// The reference mounting `instance`'s probe config `id` at the
    /// container path.
    fn reference(id: String, instance: &str) -> TaskSpecContainerSpecConfigs {
        TaskSpecContainerSpecConfigs {
            config_id: Some(id),
            config_name: Some(Self::name(instance)),
            file: Some(TaskSpecContainerSpecFile1 {
                name: Some(piqueld_probe::CONTAINER_PATH.into()),
                uid: Some("0".into()),
                gid: Some("0".into()),
                mode: Some(Self::MODE),
            }),
            runtime: None,
        }
    }

    /// Whether a config reference mounts a file at the probe's path, which
    /// no service file may use.
    pub(super) fn at_probe_path(config: &TaskSpecContainerSpecConfigs) -> bool {
        config.file.as_ref().and_then(|file| file.name.as_deref())
            == Some(piqueld_probe::CONTAINER_PATH)
    }

    /// Whether `container`, of a service of `instance`, mounts exactly this
    /// daemon's probe config for it at the probe's path when its health check
    /// runs the probe, and nothing there otherwise. A config from another
    /// probe version or installation, such as one an older daemon published,
    /// is drift. Service files are checked separately.
    pub(super) fn matches(container: &TaskSpecContainerSpec, instance: Option<&str>) -> bool {
        let runs_probe = container
            .health_check
            .as_ref()
            .and_then(BollardDocker::observed_health)
            .is_some_and(|health| health.execution().runs_probe());
        let probes = container
            .configs
            .iter()
            .flatten()
            .filter(|config| Self::at_probe_path(config))
            .collect::<Vec<_>>();
        match probes.as_slice() {
            [] => !runs_probe,
            [config] => {
                runs_probe
                    && config.config_id.as_ref().is_some_and(|id| !id.is_empty())
                    && instance.is_some_and(|instance| {
                        **config
                            == Self::reference(
                                config.config_id.clone().unwrap_or_default(),
                                instance,
                            )
                    })
            }
            _ => false,
        }
    }
}

impl BollardDocker {
    /// Returns the reference mounting `instance`'s probe config, publishing
    /// the config first when it is missing.
    ///
    /// # Errors
    /// Returns `OwnershipConflict` or `ConfigurationConflict` when a config
    /// with the probe's name has other labels or content, and request errors
    /// otherwise.
    pub(super) async fn health_probe_reference(
        &self,
        instance: &str,
    ) -> Result<TaskSpecContainerSpecConfigs, DockerError> {
        let labels = BTreeMap::from([
            (MANAGED_LABEL.to_owned(), "true".to_owned()),
            (INSTANCE_LABEL.to_owned(), instance.to_owned()),
            (HealthProbe::LABEL.to_owned(), "true".to_owned()),
        ]);
        let name = HealthProbe::name(instance);
        self.publish_config(ConfigKind::HealthProbe, &name, &labels, BINARY)
            .await
            .map(|id| HealthProbe::reference(id, instance))
    }

    /// Removes `instance`'s probe configs of other versions that no service
    /// mounts, now or in the previous spec a rollback restores, and those
    /// daemons published before probe configs were labelled with their
    /// instance. Retained configs go once the services mounting them roll
    /// onto the current probe and on again.
    ///
    /// # Errors
    /// Returns request errors listing services or listing or removing configs.
    pub(super) async fn remove_stale_health_probes(
        &self,
        instance: &InstanceId,
    ) -> Result<(), DockerError> {
        let current = HealthProbe::name(instance.as_str());
        self.remove_unused_configs(
            ConfigKind::HealthProbe,
            &[HealthProbe::LABEL.to_owned()],
            |config| {
                config.name() == current
                    || config
                        .label(INSTANCE_LABEL)
                        .is_some_and(|owner| owner != instance.as_str())
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests;
