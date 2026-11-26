//! An environment's resolved application: compiling it, and projecting
//! ingress, links, telemetry and stopping onto it.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{
    Convergence, DesiredJob, DesiredNetwork, DesiredService, DesiredVolume, InstanceId, LINK_LABEL,
    ObservedApplication, Ownership, ResolutionSet, TelemetryMount,
};
use crate::manifest::NormalizedApplication;
use crate::manifest::dependencies::StartupOrder;
use crate::manifest::domain::ValidatedJob;
use crate::{
    ApiErrorCode, ApplicationName, DockerNetworkName, DockerVolumeName, EnvironmentId, Release,
    ServiceName,
};

/// Desired state for an application and its resources.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolvedApplication {
    /// Immutable secret versions selected during effective input preparation.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secret_names: BTreeMap<String, String>,
    /// Owning environment identity.
    pub id: EnvironmentId,
    /// User-facing application name.
    pub name: ApplicationName,
    /// Current control-plane instance identity.
    pub instance_id: InstanceId,
    /// Normalized application spec hash.
    pub spec_hash: String,
    /// Desired private network.
    pub networks: Vec<DesiredNetwork>,
    /// Desired persistent volumes.
    pub volumes: Vec<DesiredVolume>,
    /// Desired services.
    pub services: Vec<DesiredService>,
    /// Deployed public route intent, including when ingress is disabled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<crate::manifest::ValidatedRoute>,
    /// One-shot jobs in declared execution order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<DesiredJob>,
    /// Services other applications may reach, as deployed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exports: Vec<crate::manifest::ValidatedExport>,
    /// Services of other applications this environment reaches, as deployed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<crate::manifest::ValidatedImport>,
    /// The applications `exports` and `imports` name, bound when the target
    /// is saved; see [`crate::links::bind`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bindings: crate::links::Bindings,
}

impl ResolvedApplication {
    /// Validates resolutions as `origin` requires, then compiles; see [`compile_application`].
    fn compile(
        app: &NormalizedApplication,
        environment: &EnvironmentId,
        instance_id: InstanceId,
        resolutions: &ResolutionSet,
        origin: SourceOrigin,
    ) -> Result<Self, Vec<CompileError>> {
        let errors = resolutions.errors(app, origin);
        if !errors.is_empty() {
            return Err(errors);
        }

        let spec_hash = app.spec_hash();
        let ownership = Ownership {
            instance_id: instance_id.clone(),
            environment_id: environment.clone(),
            service: None,
            spec_hash: spec_hash.clone(),
        };
        let private_network = DockerNetworkName::for_application(environment);
        let services = app
            .spec()
            .services
            .iter()
            .map(|service| {
                DesiredService::compile(
                    service,
                    resolutions,
                    app.files(),
                    &ownership,
                    &private_network,
                )
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| vec![error])?;
        Ok(Self {
            jobs: app
                .spec()
                .jobs
                .iter()
                .map(|job| DesiredJob::compile(job, &services, resolutions, &ownership))
                .collect::<Result<_, _>>()
                .map_err(|error| vec![error])?,
            secret_names: resolutions.secret_names.clone(),
            routes: app.spec().routes.clone(),
            exports: app.spec().exports.clone(),
            imports: app.spec().imports.clone(),
            bindings: BTreeMap::new(),
            id: environment.clone(),
            name: app.metadata().name.clone(),
            instance_id,
            spec_hash,
            networks: if app.spec().services.is_empty() {
                Vec::new()
            } else {
                vec![DesiredNetwork {
                    name: private_network.clone(),
                    labels: ownership.labels(),
                }]
            },
            volumes: app
                .spec()
                .volumes
                .iter()
                .map(|volume| DesiredVolume {
                    logical_name: volume.name.clone(),
                    name: DockerVolumeName::for_volume(environment, &volume.name),
                    labels: ownership.labels(),
                })
                .collect(),
            services,
        })
    }

    /// The ownership of the environment's resources that belong to no
    /// single service: its networks and volumes.
    fn ownership(&self) -> Ownership {
        Ownership {
            instance_id: self.instance_id.clone(),
            environment_id: self.id.clone(),
            service: None,
            spec_hash: self.spec_hash.clone(),
        }
    }

    /// Whether `observed` runs every one of its services exactly as
    /// desired (no field the planner compares drifts), converged: what
    /// promoting a release requires of its source environment right now.
    /// Project ingress first (see [`Self::with_ingress_routes`]), as
    /// reconciliation does.
    #[must_use]
    pub fn converged_in(&self, observed: &ObservedApplication) -> bool {
        self.services.iter().all(|desired| {
            observed.services.iter().any(|service| {
                service.name == desired.name.as_str()
                    && service.convergence == Convergence::Converged
                    && service.matches(desired)
            })
        })
    }

    /// Services that must converge before this job starts, inherited from its
    /// referenced service. Read the service so prepared targets saved by older
    /// versions, which cleared job container dependencies, work too.
    #[must_use]
    pub fn job_dependencies(&self, job: &DesiredJob) -> BTreeSet<&str> {
        self.services
            .dependencies_of(job.container.logical_name.as_str())
    }

    /// Projects route intent into runtime networks only when ingress is enabled.
    ///
    /// Shorthand for `with_ingress_routes` with no previously accepted routes.
    #[must_use]
    pub fn with_ingress(self, enabled: bool) -> Self {
        self.with_ingress_routes(enabled, &[])
    }

    /// Retains network access for accepted routes until their cutover succeeds.
    ///
    /// Rebuilds the ingress network projection from scratch, so it is idempotent:
    /// 1. Drops the application's ingress network and every service's attachment
    ///    to it.
    /// 2. When `enabled`, reattaches each service targeted by a desired or
    ///    `accepted` (still live) route.
    /// 3. When `enabled` and any route exists, adds the owned ingress network.
    ///
    /// Network lists are kept sorted so the result compares deterministically.
    #[must_use]
    pub fn with_ingress_routes(
        mut self,
        enabled: bool,
        accepted: &[crate::manifest::ValidatedRoute],
    ) -> Self {
        let name = DockerNetworkName::for_ingress(&self.id);
        self.networks.retain(|network| network.name != name);
        for service in &mut self.services {
            service.networks.retain(|network| network != &name);
            if enabled
                && self
                    .routes
                    .iter()
                    .chain(accepted)
                    .any(|route| route.target.service() == Some(&service.logical_name))
            {
                service.networks.push(name.clone());
                service.networks.sort();
            }
        }
        if enabled
            && self
                .routes
                .iter()
                .chain(accepted)
                .any(|route| route.target.service().is_some())
        {
            let labels = self.ownership().labels();
            self.networks.push(DesiredNetwork { name, labels });
            self.networks
                .sort_by(|left, right| left.name.cmp(&right.name));
        }
        self
    }

    /// Runs no replica of any service while `stopped`, keeping every other
    /// setting, so a stopped environment's services stay in place and start
    /// again as they were. Like links, never part of a prepared target.
    #[must_use]
    pub fn with_stopped(mut self, stopped: bool) -> Self {
        if stopped {
            for service in &mut self.services {
                service.replicas = 0;
            }
        }
        self
    }

    /// Whether the target declares exports or imports, without which it takes
    /// part in no link.
    #[must_use]
    pub fn has_links(&self) -> bool {
        !self.exports.is_empty() || !self.imports.is_empty()
    }

    /// Projects `links` this environment takes part in into networks and
    /// attachments. The importer owns one network per link, which all of its
    /// services join. The exporter's linked services join it once `observed`
    /// shows it exists, answering to `<service>.<application>` there, so
    /// importers reach no other service and no other importer.
    ///
    /// Links are never part of a prepared target: project them onto a fresh
    /// copy, as ingress, each time the target is planned.
    #[must_use]
    pub fn with_links(
        mut self,
        links: &[crate::links::Link],
        observed: &ObservedApplication,
    ) -> Self {
        let ownership = self.ownership();
        for link in links {
            if link.importer == self.id && !self.services.is_empty() {
                let name = DockerNetworkName::for_link(&self.id, &link.exporter);
                for service in &mut self.services {
                    service.networks.push(name.clone());
                }
                for job in &mut self.jobs {
                    job.container.networks.push(name.clone());
                    job.container.networks.sort();
                }
                let mut labels = ownership.labels();
                labels.insert(LINK_LABEL.into(), link.exporter.to_string());
                self.networks.push(DesiredNetwork { name, labels });
            } else if link.exporter == self.id {
                let name = DockerNetworkName::for_link(&link.importer, &self.id);
                let exists = observed.linked_networks.iter().any(|network| {
                    network.name == name.as_str()
                        && network.is_owned_by(&self.instance_id, &link.importer)
                        && network.labels.get(LINK_LABEL) == Some(&self.id.to_string())
                });
                if !exists {
                    continue;
                }
                for service in &mut self.services {
                    if link.services.contains(&service.logical_name) {
                        service.networks.push(name.clone());
                        service.link_aliases.insert(
                            name.clone(),
                            crate::manifest::links::alias(
                                &service.logical_name,
                                &link.exporter_application,
                            ),
                        );
                    }
                }
            }
        }
        for service in &mut self.services {
            service.networks.sort();
        }
        self.networks
            .sort_by(|left, right| left.name.cmp(&right.name));
        self
    }

    /// The service receiving the daemon's telemetry, if any.
    #[must_use]
    pub fn telemetry_receiver(&self) -> Option<&DesiredService> {
        self.services
            .iter()
            .find(|service| service.receives_telemetry)
    }

    /// Projects `mount`, its receiving service's socket directory while
    /// it is granted telemetry, onto its receiving service, and removes it
    /// from every other.
    ///
    /// Like links, the mount is never part of a prepared target: project it
    /// onto a fresh copy each time the target is planned.
    #[must_use]
    pub fn with_telemetry(mut self, mount: Option<&TelemetryMount>) -> Self {
        for service in &mut self.services {
            service.telemetry = mount.filter(|_| service.receives_telemetry).cloned();
        }
        self
    }

    /// Reuses immutable sources for services whose requested image is unchanged.
    ///
    /// A prior resolution is kept only when it still matches the new source
    /// (same image reference, or identical Git source and compatible commit), so
    /// redeploying an unchanged service does not re-pull or rebuild. Pinned
    /// secret versions are carried over as-is.
    #[must_use]
    pub fn reusable_resolutions(&self, application: &NormalizedApplication) -> ResolutionSet {
        ResolutionSet {
            secret_names: self.secret_names.clone(),
            sources: application
                .spec()
                .services
                .iter()
                .filter_map(|service| {
                    let prior = self
                        .services
                        .iter()
                        .find(|prior| prior.logical_name == service.name)?;
                    prior
                        .source
                        .resolves(&service.source)
                        .then(|| (service.name.clone(), prior.source.clone()))
                })
                .collect(),
        }
    }
}

/// Sanitized compilation error for unresolved runtime inputs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompileError {
    /// Stable machine-readable error code.
    pub code: String,
    /// Resource associated with the error.
    pub resource: String,
    /// Safe human-readable explanation.
    pub message: String,
}

impl CompileError {
    /// An error with the stable `code` at `resource`.
    pub(super) fn new(
        code: ApiErrorCode,
        resource: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            resource: resource.into(),
            message: message.into(),
        }
    }

    /// `service` has no resolved source.
    pub(super) fn source_unresolved(service: &ServiceName) -> Self {
        Self::new(
            ApiErrorCode::SourceUnresolved,
            service.as_str(),
            "service source has not been resolved to an immutable image",
        )
    }

    /// `service` mounts the repository file at `path`, which wasn't captured.
    pub(super) fn file_unresolved(service: &ServiceName, path: &str) -> Self {
        Self::new(
            ApiErrorCode::FileUnresolved,
            service.as_str(),
            format!("repository file {path} was not captured with the manifest"),
        )
    }

    /// `job` runs a service the application doesn't declare.
    pub(super) fn job_service_missing(job: &ValidatedJob) -> Self {
        Self::new(
            ApiErrorCode::JobServiceMissing,
            job.name.as_str(),
            format!("job runs undeclared service {}", job.service),
        )
    }

    /// `owner`, a service or job, mounts `secret`, which isn't pinned.
    pub(super) fn secret_unresolved(owner: &str, secret: &str) -> Self {
        Self::new(
            ApiErrorCode::SecretUnresolved,
            owner,
            format!("secret {secret} has not been pinned"),
        )
    }
}

/// Reports compilation errors at their resource, e.g. a release's
/// `release_incompatible` build input `web.source.build.args.VITE_ORIGIN`.
impl From<Vec<CompileError>> for crate::ValidationErrors {
    fn from(errors: Vec<CompileError>) -> Self {
        Self(
            errors
                .into_iter()
                .map(|error| crate::ValidationError {
                    code: error.code,
                    path: error.resource,
                    message: error.message,
                })
                .collect(),
        )
    }
}

/// Compiles normalized intent for one environment once all image references
/// have immutable resolutions: one private network (when there are services),
/// one Docker volume per declared volume and one `DesiredService` per service,
/// all named after and labelled with `environment`.
///
/// # Errors
///
/// Returns diagnostics for a service whose image resolution is missing or
/// not an immutable reference to the requested repository.
///
/// # Panics
///
/// Panics on a malformed spec hash, a missing validated resolution or a job
/// naming an undeclared service, all internal bugs.
pub fn compile_application(
    app: &NormalizedApplication,
    environment: &EnvironmentId,
    instance_id: InstanceId,
    resolutions: &ResolutionSet,
) -> Result<ResolvedApplication, Vec<CompileError>> {
    ResolvedApplication::compile(
        app,
        environment,
        instance_id,
        resolutions,
        SourceOrigin::Prepared,
    )
}

/// Compiles `app`, a release's manifest rendered for `environment` by
/// [`Release::render`], with the release's own images and `secret_names`
/// pinned for this deployment. Nothing is rebuilt, so every build input must
/// render as it did for the release: each that does not is a
/// `release_incompatible` error whose resource names the field, e.g.
/// `web.source.build.args.VITE_ORIGIN`.
///
/// # Errors
///
/// Returns those errors, or the errors of [`compile_application`].
///
/// # Panics
///
/// Panics in the same internal-bug cases as [`compile_application`].
pub fn compile_release(
    app: &NormalizedApplication,
    environment: &EnvironmentId,
    instance_id: InstanceId,
    release: &Release,
    secret_names: BTreeMap<String, String>,
) -> Result<ResolvedApplication, Vec<CompileError>> {
    let resolutions = ResolutionSet {
        sources: release.sources().clone(),
        secret_names,
    };
    ResolvedApplication::compile(
        app,
        environment,
        instance_id,
        &resolutions,
        SourceOrigin::Release,
    )
}

/// Where compiled sources come from, which decides how the source check
/// reports a source that does not match its rendered service.
#[derive(Clone, Copy)]
pub(super) enum SourceOrigin {
    /// Prepared for this rendering, so a mismatch is an internal error.
    Prepared,
    /// Prepared for a release, whose build inputs the rendering must reproduce.
    Release,
}
