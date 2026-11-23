//! The Docker resources an environment's resolved application compiles to.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{
    JOB_LABEL, JOB_OPERATION_LABEL, NetworkAttachment, Ownership, ResolutionSet, ResolvedSource,
    SERVICE_LABEL, SecretFile, valid_logical_name,
};
#[cfg(doc)]
use super::{ResolvedApplication, TELEMETRY_TARGET};
use crate::manifest::domain::{
    ValidatedJob as Job, ValidatedMount as Mount, ValidatedService as Service,
};
use crate::manifest::{
    FileMode, FileSource, JobRun, JobVolumeAccess, RepositoryFiles, RolloutPolicy, ServiceSecurity,
    ValidatedHealthCheck, ValidatedResourceLimits, ValidatedRollout,
};
use crate::{
    DockerConfigName, DockerNetworkName, DockerServiceName, DockerVolumeName, ImmutableImage,
    JobName, ServiceName, VolumeName,
};

/// Desired private overlay network state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DesiredNetwork {
    /// Canonical Docker resource name.
    pub name: DockerNetworkName,
    /// Expected ownership labels.
    pub labels: BTreeMap<String, String>,
}

impl DesiredNetwork {
    /// Returns whether the network has a canonical name and identity.
    ///
    /// Networks are application-scoped, so a service label is rejected and the
    /// name must be the application's canonical private or ingress network.
    #[must_use]
    pub fn has_valid_identity(&self) -> bool {
        let Some(application) = Ownership::labelled_environment(&self.labels) else {
            return false;
        };
        !self.labels.contains_key(SERVICE_LABEL)
            && self.name.is_canonical(&application, &self.labels)
    }
}

/// Desired persistent volume state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DesiredVolume {
    /// Manifest-level volume name.
    pub logical_name: VolumeName,
    /// Canonical Docker resource name.
    pub name: DockerVolumeName,
    /// Expected ownership labels.
    pub labels: BTreeMap<String, String>,
}

impl DesiredVolume {
    /// Returns whether the volume has a canonical name and identity.
    ///
    /// Requires valid ownership labels without a service label, a safe logical
    /// name, and a Docker name derived from the labelled application.
    #[must_use]
    pub fn has_valid_identity(&self) -> bool {
        let Some(application) = Ownership::labelled_environment(&self.labels) else {
            return false;
        };
        valid_logical_name(self.logical_name.as_str())
            && !self.labels.contains_key(SERVICE_LABEL)
            && self.name == DockerVolumeName::for_volume(&application, &self.logical_name)
    }
}

/// Desired persistent volume mount in a service.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DesiredMount {
    /// Canonical Docker volume name.
    pub volume_name: DockerVolumeName,
    /// Container target path.
    pub target: String,
    /// Whether the mount is read-only.
    pub read_only: bool,
}

/// A read-only file mounted from an immutable, content-named Swarm config.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DesiredFile {
    /// The config holding the content, named after the environment and the
    /// content's digest.
    pub config_name: DockerConfigName,
    /// Absolute container path.
    pub target: String,
    /// Permissions of the root-owned file.
    pub mode: FileMode,
    /// The file's content.
    pub content: String,
}

/// Desired Docker service state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DesiredService {
    /// Manifest-level service name.
    pub logical_name: ServiceName,
    /// Canonical Docker service name.
    pub name: DockerServiceName,
    /// Immutable source resolution used by the service.
    pub source: ResolvedSource,
    /// Digest-pinned image reference.
    pub image: ImmutableImage,
    /// Desired replica count.
    pub replicas: u16,
    /// Environment variables keyed by name.
    pub environment: BTreeMap<String, String>,
    /// Container entrypoint command.
    pub command: Vec<String>,
    /// Arguments passed to the command.
    pub arguments: Vec<String>,
    /// Persistent volume mounts.
    pub mounts: Vec<DesiredMount>,
    /// Immutable secret file bindings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<SecretFile>,
    /// Read-only files from content-named configs. Omitted when empty, so
    /// targets prepared before files existed still decode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<DesiredFile>,
    /// Optional health check.
    pub healthcheck: Option<ValidatedHealthCheck>,
    /// Optional CPU and memory limits.
    pub resources: Option<ValidatedResourceLimits>,
    /// Private, ingress and link network names attached to the service.
    pub networks: Vec<DockerNetworkName>,
    /// Link networks on which importers reach this service, with the name it
    /// answers to there. Projected at runtime by
    /// [`ResolvedApplication::with_links`], never prepared.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub link_aliases: BTreeMap<DockerNetworkName, String>,
    /// Ownership labels.
    pub labels: BTreeMap<String, String>,
    /// Logical services that must converge before this service rolls out.
    /// Planning order only; it is not part of the Docker specification.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<ServiceName>,
    /// Rollout settings, resolved by [`Self::rollout_policy`]. Omitted when
    /// default, so targets prepared before the setting existed still decode.
    #[serde(default, skip_serializing_if = "ValidatedRollout::is_default")]
    pub rollout: ValidatedRollout,
    /// Opt-in hardening, which jobs share. Omitted when off, so targets
    /// prepared before the setting existed still decode.
    #[serde(default, skip_serializing_if = "ServiceSecurity::is_default")]
    pub security: ServiceSecurity,
    /// Whether the service receives the daemon's telemetry once its
    /// environment is granted it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub receives_telemetry: bool,
    /// The telemetry socket directory mounted while the environment is
    /// granted. Projected at runtime by
    /// [`ResolvedApplication::with_telemetry`], never prepared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<TelemetryMount>,
}

/// A telemetry receiver's socket directory, bind-mounted at
/// [`TELEMETRY_TARGET`], and the user the service runs as instead of the
/// image's, the daemon's, so the daemon can connect to the socket it creates.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct TelemetryMount {
    /// Absolute host directory.
    pub source: String,
    /// `uid:gid`.
    pub user: String,
}

impl DesiredService {
    /// Builds the desired Docker service for one validated service.
    ///
    /// Must only run after [`ResolutionSet::errors`] finds none: it indexes the
    /// resolved source, pinned secret names, and captured repository `files`
    /// directly and panics if missing. Services start attached to the private
    /// network only; ingress is projected later by
    /// `ResolvedApplication::with_ingress_routes`.
    #[expect(
        clippy::expect_used,
        reason = "Validated applications captured every repository file they reference."
    )]
    pub(super) fn compile(
        service: &Service,
        resolutions: &ResolutionSet,
        files: &RepositoryFiles,
        environment_ownership: &Ownership,
        private_network: &DockerNetworkName,
    ) -> Self {
        let source = resolutions.sources[&service.name].clone();
        let environment = &environment_ownership.environment_id;
        let mut ownership = environment_ownership.clone();
        ownership.service = Some(service.name.clone());
        Self {
            logical_name: service.name.clone(),
            name: DockerServiceName::for_service(environment, &service.name),
            image: source.image(),
            source,
            replicas: service.replicas,
            environment: service.environment.clone(),
            command: service.command.clone(),
            arguments: service.arguments.clone(),
            mounts: service
                .mounts
                .iter()
                .map(|mount: &Mount| DesiredMount {
                    volume_name: DockerVolumeName::for_volume(environment, &mount.volume),
                    target: mount.target.clone(),
                    read_only: mount.read_only,
                })
                .collect(),
            secrets: resolutions.secret_files(&service.secrets).collect(),
            files: service
                .files
                .iter()
                .map(|file| {
                    let content = match &file.source {
                        FileSource::Content(content) => content.as_str(),
                        FileSource::Path(path) => files
                            .get(path)
                            .expect("validated applications captured their repository files"),
                    };
                    DesiredFile {
                        config_name: DockerConfigName::for_file(environment, content, file.mode),
                        target: file.target.clone(),
                        mode: file.mode,
                        content: content.to_owned(),
                    }
                })
                .collect(),
            healthcheck: service.healthcheck.clone(),
            resources: service.resources.clone(),
            networks: vec![private_network.clone()],
            link_aliases: BTreeMap::new(),
            labels: ownership.labels(),
            depends_on: service.depends_on.clone(),
            rollout: service.rollout,
            security: service.security,
            receives_telemetry: service.receives_telemetry,
            telemetry: None,
        }
    }

    /// The update policy Docker applies to this service; see
    /// [`ValidatedRollout::policy`]. A telemetry receiver's socket counts as
    /// a writable mount, granted or not, so its order doesn't change with
    /// the grant.
    #[must_use]
    pub fn rollout_policy(&self) -> RolloutPolicy {
        self.rollout.policy(
            self.mounts
                .iter()
                .map(|mount| mount.read_only)
                .chain(self.receives_telemetry.then_some(false)),
        )
    }

    /// Returns whether the service has a canonical name and identity.
    ///
    /// Requires valid ownership labels whose service label equals the logical
    /// name, and a Docker name derived from the labelled application.
    #[must_use]
    pub fn has_valid_identity(&self) -> bool {
        let Some(application) = Ownership::labelled_environment(&self.labels) else {
            return false;
        };
        valid_logical_name(self.logical_name.as_str())
            && self.labels.get(SERVICE_LABEL).map(String::as_str)
                == Some(self.logical_name.as_str())
            && self.name == DockerServiceName::for_service(&application, &self.logical_name)
    }

    /// Returns the attachments piqueld authors for this service. The logical
    /// name is an alias only on the application's private network, so peers
    /// reach `postgres:5432` while same-named services in other applications
    /// stay isolated on their own networks. On a link network, an exported
    /// service answers to its link alias. Without valid ownership labels
    /// naming the application, the private network gets no alias.
    #[must_use]
    pub fn network_attachments(&self) -> Vec<NetworkAttachment> {
        let private = Ownership::labelled_environment(&self.labels)
            .map(|application| DockerNetworkName::for_application(&application));
        self.networks
            .iter()
            .map(|network| NetworkAttachment {
                network: network.to_string(),
                aliases: if private.as_ref() == Some(network) {
                    vec![self.logical_name.to_string()]
                } else {
                    self.link_aliases
                        .get(network)
                        .cloned()
                        .into_iter()
                        .collect()
                },
            })
            .collect()
    }

    /// Returns the application's ingress network when public routes target this service.
    #[must_use]
    pub fn ingress_network(&self) -> Option<&DockerNetworkName> {
        let application = Ownership::labelled_environment(&self.labels)?;
        let ingress = DockerNetworkName::for_ingress(&application);
        self.networks.iter().find(|network| **network == ingress)
    }
}

/// Desired one-shot job, run as a Swarm replicated job before its deployment point.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DesiredJob {
    /// Manifest-level job name.
    pub logical_name: JobName,
    /// Deployment point at which the job runs.
    pub run: JobRun,
    /// Seconds the job may run before the deployment fails.
    pub timeout_seconds: u32,
    /// How runs use the service's volumes. Targets prepared before this
    /// setting existed read them only.
    #[serde(default)]
    pub volume_access: JobVolumeAccess,
    /// Whether the job's own service stops before it runs. Omitted when unset,
    /// like targets prepared before this setting existed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stop_service: bool,
    /// The referenced service's container settings with the job's command,
    /// Docker name, and labels. Health checks never apply to jobs.
    pub container: DesiredService,
}

impl DesiredJob {
    /// Builds a job's run as a copy of its service's container: its command,
    /// the environment variables and secrets it adds, one replica, no health
    /// check, dependencies, or rollout settings, and its own Docker name and
    /// labels. Validation guarantees that every job references a declared
    /// service and overrides none of its settings; like
    /// [`DesiredService::compile`], it panics on an unpinned secret.
    #[expect(
        clippy::expect_used,
        reason = "Validated jobs reference declared services."
    )]
    pub(super) fn compile(
        job: &Job,
        services: &[DesiredService],
        resolutions: &ResolutionSet,
        environment_ownership: &Ownership,
    ) -> Self {
        let mut container = services
            .iter()
            .find(|service| service.logical_name == job.service)
            .expect("validated jobs reference declared services")
            .clone();
        container.name =
            DockerServiceName::for_job(&environment_ownership.environment_id, &job.name);
        container.replicas = 1;
        container.command.clone_from(&job.command);
        container.arguments.clear();
        container.environment.extend(job.environment.clone());
        container
            .secrets
            .extend(resolutions.secret_files(&job.secrets));
        container.healthcheck = None;
        container.depends_on.clear();
        container.rollout = ValidatedRollout::default();
        container.receives_telemetry = false;
        container.labels = environment_ownership.labels();
        container
            .labels
            .insert(JOB_LABEL.into(), job.name.to_string());
        Self {
            logical_name: job.name.clone(),
            run: job.run,
            timeout_seconds: job.timeout_seconds,
            volume_access: job.volume_access,
            stop_service: job.stop_service,
            container,
        }
    }

    /// Returns whether the job has a canonical name and identity.
    #[must_use]
    pub fn has_valid_identity(&self) -> bool {
        let labels = &self.container.labels;
        let Some(application) = Ownership::labelled_environment(labels) else {
            return false;
        };
        !labels.contains_key(SERVICE_LABEL)
            && labels.get(JOB_LABEL).map(String::as_str) == Some(self.logical_name.as_str())
            && self.container.name == DockerServiceName::for_job(&application, &self.logical_name)
    }

    /// Volumes the job writes. No other container may use them while it runs,
    /// so services mounting them stop first. Empty for read-only jobs.
    pub fn exclusive_volumes(&self) -> impl Iterator<Item = &DockerVolumeName> {
        self.container
            .mounts
            .iter()
            .filter(|mount| self.volume_access == JobVolumeAccess::Exclusive && !mount.read_only)
            .map(|mount| &mount.volume_name)
    }

    /// Whether the job holds `volume`, a Docker volume name, exclusively.
    fn holds_volume(&self, volume: &str) -> bool {
        self.exclusive_volumes()
            .any(|exclusive| exclusive.as_str() == volume)
    }

    /// Docker name of the job's own service when the job stops it before it
    /// runs.
    #[must_use]
    pub fn stopped_service(&self) -> Option<DockerServiceName> {
        let environment = Ownership::labelled_environment(&self.container.labels)?;
        self.stop_service
            .then(|| DockerServiceName::for_service(&environment, &self.container.logical_name))
    }

    /// Whether the job stops services before it runs: its own, or those
    /// mounting a volume it holds exclusively.
    #[must_use]
    pub fn stops_services(&self) -> bool {
        self.stop_service || self.exclusive_volumes().next().is_some()
    }

    /// Whether the service named `name`, a Docker service name, which mounts
    /// `volumes`, must stop before the job runs and stay stopped until the
    /// rollout: it is the job's stopped service, or mounts a volume the job
    /// holds exclusively.
    #[must_use]
    pub fn stops<'a>(&self, name: &str, mut volumes: impl Iterator<Item = &'a str>) -> bool {
        self.stopped_service()
            .is_some_and(|service| service.as_str() == name)
            || volumes.any(|volume| self.holds_volume(volume))
    }

    /// Returns this job's run for `operation`. Runs of read-only jobs mount
    /// every volume read-only.
    #[must_use]
    pub fn for_operation(&self, operation: &str) -> DesiredJobRun {
        let mut job = self.clone();
        if job.volume_access == JobVolumeAccess::ReadOnly {
            for mount in &mut job.container.mounts {
                mount.read_only = true;
            }
        }
        job.container
            .labels
            .insert(JOB_OPERATION_LABEL.into(), operation.into());
        DesiredJobRun {
            job,
            operation: operation.into(),
        }
    }
}

/// One operation's run of a job. Its operation label lets a retried or
/// restarted operation find and resume its own run instead of starting over.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesiredJobRun {
    job: DesiredJob,
    operation: String,
}

impl DesiredJobRun {
    /// Operation that starts this run.
    #[must_use]
    pub fn operation(&self) -> &str {
        &self.operation
    }
}

impl std::ops::Deref for DesiredJobRun {
    type Target = DesiredJob;

    fn deref(&self) -> &DesiredJob {
        &self.job
    }
}
