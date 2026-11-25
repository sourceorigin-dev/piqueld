//! Typed changes to saved application configuration. No edit performs runtime work.
use crate::manifest::{
    ApplicationManifest, Build, Export, GitRepository, HealthCheck, Import, Job, Mount,
    RepositoryManifest, ResourceLimits, Rollback, Rollout, Route, SecretDeclaration, SecretMount,
    Service, ServiceFile, ServiceSecurity, Source, SourceRepository, Template, Typed, Variable,
    Visibility, Volume,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

// Declares one `{ "value": ... }` request body per field type used by the field edit
// endpoints. `deserialize_with` makes `value` required even when its type is `Option`,
// so clients must send an explicit `null` to clear a setting.
macro_rules! value_request {
    ($($name:ident: $ty:ty;)*) => {$ (
        #[doc = concat!("Typed replacement value for `", stringify!($name), "`.")]
        #[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
        #[serde(deny_unknown_fields)]
        pub struct $name {
            /// New value; null explicitly clears an optional setting.
            #[serde(deserialize_with = "Deserialize::deserialize")]
            #[schema(required = true)]
            pub value: $ty,
        }
    )*};
}
value_request! {
    StringValue: String;
    OptionalStringValue: Option<String>;
    TemplateValue: Template;
    TemplatesValue: Vec<Template>;
    ReplicasValue: Typed<u16>;
    SecondsValue: Typed<u32>;
    CountValue: Typed<u32>;
    CpuValue: Option<Typed<u32>>;
    MemoryValue: Option<Typed<u64>>;
    StringsValue: Vec<String>;
    VariablesValue: Variables;
    SourceValue: Source;
    HealthValue: Option<HealthCheck>;
    ResourcesValue: Option<ResourceLimits>;
    RolloutValue: Rollout;
    SecurityValue: ServiceSecurity;
    BoolValue: bool;
    EnvironmentValue: BTreeMap<String, Template>;
    MountsValue: Vec<Mount>;
    SecretsValue: Vec<SecretMount>;
    FilesValue: Vec<ServiceFile>;
    VolumesValue: Vec<Volume>;
    RoutesValue: Vec<Route>;
    JobsValue: Vec<Job>;
    GeneratedSecretsValue: Vec<SecretDeclaration>;
    ExportsValue: Vec<Export>;
    ImportsValue: Vec<Import>;
    RepositoryValue: Option<RepositoryManifest>;
    RepositorySyncValue: crate::sync::RepositorySync;
    VisibilityValue: Visibility;
    RollbackValue: Option<Rollback>;
}

/// Source and scaling settings saved together by the dashboard form.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ServiceGeneral {
    /// Explicit source variant.
    pub source: Source,
    /// Desired replicas.
    pub replicas: Typed<u16>,
}

/// Container entrypoint and arguments saved together.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ServiceProcess {
    /// Entrypoint elements.
    pub command: Vec<Template>,
    /// Argument elements.
    pub arguments: Vec<Template>,
}

/// An edit to an existing application, addressed by its stable identity.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "field", content = "value", rename_all = "snake_case")]
pub enum ApplicationEdit {
    /// Change application display name.
    Name(String),
    /// Connect or disconnect a repository manifest.
    Repository(Option<RepositoryManifest>),
    /// Change the repository URL.
    RepositoryUrl(String),
    /// Change the manifest path.
    RepositoryPath(String),
    /// Change how environments and previews follow pushes.
    RepositorySync(crate::sync::RepositorySync),
    /// Add a new service, rejecting duplicate names.
    AddService(Box<Service>),
    /// Remove a service declaration.
    RemoveService(String),
    /// Edit one existing service.
    Service {
        /// Existing logical service name.
        name: String,
        /// Typed setting change.
        edit: ServiceEdit,
    },
    /// Add a named volume declaration.
    AddVolume(Volume),
    /// Replace the named volume declarations.
    Volumes(Vec<Volume>),
    /// Replace public routes owned by this application.
    Routes(Vec<Route>),
    /// Replace the one-shot jobs; their order is their execution order.
    Jobs(Vec<Job>),
    /// Replace the declarations of secrets piqueld generates, `spec.secrets`.
    /// Values already generated are kept, even for removed declarations.
    GeneratedSecrets(Vec<SecretDeclaration>),
    /// Replace the services other applications may reach.
    Exports(Vec<Export>),
    /// Replace the services of other applications this one reaches.
    Imports(Vec<Import>),
    /// Remove a volume declaration; validation rejects remaining mounts.
    RemoveVolume(String),
    /// Replace every variable value.
    Variables(Variables),
    /// Set the visibility ceiling of one environment's routes.
    EnvironmentVisibility {
        /// Environment name, as in `[spec.environments.<name>]`.
        environment: String,
        /// The strictest visibility its routes get; `public` restricts nothing.
        visibility: Visibility,
    },
    /// Set what happens when one environment's rollout fails.
    EnvironmentRollback {
        /// Environment name, as in `[spec.environments.<name>]`.
        environment: String,
        /// `automatic` or `manual`; `None` returns to the default, automatic
        /// for promoted environments and manual otherwise.
        rollback: Option<Rollback>,
    },
}

crate::names::string_enum! {
    #[serde]
    /// The part of an application's configuration a typed edit changed, as
    /// its history records it.
    pub enum EditedField {
        /// The display name.
        Name => "name",
        /// The repository manifest connection.
        Repository => "repository",
        /// The set of services.
        Services => "services",
        /// One service's settings.
        Service => "service",
        /// The volumes.
        Volumes => "volumes",
        /// The routes.
        Routes => "routes",
        /// The before-rollout jobs.
        Jobs => "jobs",
        /// The exports.
        Exports => "exports",
        /// The imports.
        Imports => "imports",
        /// The generated secrets.
        Secrets => "secrets",
        /// The variables.
        Variables => "variables",
        /// The environments' settings.
        Environments => "environments",
    }
}

impl ApplicationEdit {
    /// The part of the configuration this edit changes.
    #[must_use]
    pub const fn field(&self) -> EditedField {
        match self {
            Self::Name(_) => EditedField::Name,
            Self::Repository(_)
            | Self::RepositoryUrl(_)
            | Self::RepositoryPath(_)
            | Self::RepositorySync(_) => EditedField::Repository,
            Self::AddService(_) | Self::RemoveService(_) => EditedField::Services,
            Self::Service { .. } => EditedField::Service,
            Self::AddVolume(_) | Self::Volumes(_) | Self::RemoveVolume(_) => EditedField::Volumes,
            Self::Routes(_) => EditedField::Routes,
            Self::Jobs(_) => EditedField::Jobs,
            Self::Exports(_) => EditedField::Exports,
            Self::Imports(_) => EditedField::Imports,
            Self::GeneratedSecrets(_) => EditedField::Secrets,
            Self::Variables(_) => EditedField::Variables,
            Self::EnvironmentVisibility { .. } | Self::EnvironmentRollback { .. } => {
                EditedField::Environments
            }
        }
    }
}

/// Every variable value of an application, replaced together.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Variables {
    /// Defaults, `[spec.variables]`.
    pub defaults: BTreeMap<String, Variable>,
    /// Values per environment name, `[spec.environments.<name>.variables]`.
    pub environments: BTreeMap<String, BTreeMap<String, Variable>>,
    /// Values for every preview, `[spec.previews.variables]`.
    pub previews: BTreeMap<String, Variable>,
}

impl Variables {
    /// Replaces the variable values of `spec`, keeping other environment
    /// configuration. Blocks declare environments, so a block left empty is
    /// kept, and only values add one.
    fn apply(self, spec: &mut crate::manifest::ApplicationSpec) {
        let Self {
            defaults,
            mut environments,
            previews,
        } = self;
        spec.variables = defaults;
        spec.previews.variables = previews;
        for (name, config) in &mut spec.environments {
            config.variables = environments.remove(name).unwrap_or_default();
        }
        for (name, variables) in environments {
            if !variables.is_empty() {
                spec.environments.entry(name).or_default().variables = variables;
            }
        }
    }

    /// The variable values of `manifest`.
    #[must_use]
    pub fn of(manifest: &ApplicationManifest) -> Self {
        Self {
            defaults: manifest.spec.variables.clone(),
            environments: manifest
                .spec
                .environments
                .iter()
                .map(|(name, config)| (name.clone(), config.variables.clone()))
                .collect(),
            previews: manifest.spec.previews.variables.clone(),
        }
    }
}

/// A change to one service. Nested edits require the corresponding source/check variant.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "field", content = "value", rename_all = "snake_case")]
pub enum ServiceEdit {
    /// Logical service name.
    Name(String),
    /// Switch source variant.
    Source(Source),
    /// Set an image source.
    Image(Template),
    /// Git repository URL.
    GitUrl(String),
    /// Git branch.
    GitBranch(String),
    /// Pinned Git commit, or none.
    GitCommit(Option<String>),
    /// Dockerfile path.
    Dockerfile(Template),
    /// Docker build context.
    Context(Template),
    /// Desired replicas.
    Replicas(Typed<u16>),
    /// Replace environment entries.
    Environment(BTreeMap<String, Template>),
    /// Set or remove one environment variable.
    EnvironmentEntry((String, Option<Template>)),
    /// Entrypoint elements.
    Command(Vec<Template>),
    /// Argument elements.
    Arguments(Vec<Template>),
    /// Replace mount declarations.
    Mounts(Vec<Mount>),
    /// Replace secret file references.
    Secrets(Vec<SecretMount>),
    /// Add or replace a mount at its target.
    Mount(Mount),
    /// Remove a mount by container target.
    RemoveMount(String),
    /// Replace file declarations.
    Files(Vec<ServiceFile>),
    /// Add or replace a file at its target.
    File(ServiceFile),
    /// Remove a file by container target.
    RemoveFile(String),
    /// Switch or clear health check.
    Healthcheck(Option<HealthCheck>),
    /// HTTP health port.
    HealthPort(Typed<u16>),
    /// HTTP health path.
    HealthPath(Template),
    /// Health command elements.
    HealthCommand(Vec<Template>),
    /// Health interval seconds.
    HealthInterval(Typed<u32>),
    /// Health timeout seconds.
    HealthTimeout(Typed<u32>),
    /// Health start period seconds.
    HealthStartPeriod(Typed<u32>),
    /// Consecutive health failures before unhealthy.
    HealthRetries(Typed<u32>),
    /// Replace resource limits.
    Resources(Option<ResourceLimits>),
    /// Set or clear CPU millicores.
    Cpu(Option<Typed<u32>>),
    /// Set or clear memory bytes.
    Memory(Option<Typed<u64>>),
    /// Source and replicas saved together.
    General(ServiceGeneral),
    /// Entrypoint and arguments saved together.
    Process(ServiceProcess),
    /// Replace the services that must be healthy before this one rolls out.
    DependsOn(Vec<String>),
    /// Replace the rollout settings; omitted fields use their defaults.
    Rollout(Rollout),
    /// Whether the service receives the daemon's telemetry.
    ReceivesTelemetry(bool),
    /// Replace the security settings; omitted fields are off.
    Security(ServiceSecurity),
}

/// A declaration identified by a key within its list: services and volumes
/// by name, mounts and files by container target.
trait Keyed: Sized {
    /// Name of the declaration in `NotFound` and `AlreadyExists` errors.
    const KIND: &'static str;

    /// The key that identifies it within its list.
    fn key(&self) -> &str;

    /// The item at `key`, failing with `NotFound` if absent.
    fn find<'a>(items: &'a mut [Self], key: &str) -> Result<&'a mut Self, EditError> {
        items
            .iter_mut()
            .find(|item| item.key() == key)
            .ok_or_else(|| EditError::NotFound {
                kind: Self::KIND,
                name: key.to_owned(),
            })
    }

    /// Appends `value`, failing with `AlreadyExists` if its key is taken.
    fn add(items: &mut Vec<Self>, value: Self) -> Result<(), EditError> {
        if items.iter().any(|item| item.key() == value.key()) {
            return Err(EditError::AlreadyExists {
                kind: Self::KIND,
                name: value.key().to_owned(),
            });
        }
        items.push(value);
        Ok(())
    }

    /// Replaces the item with `value`'s key, or appends `value`.
    fn set(items: &mut Vec<Self>, value: Self) {
        match items.iter_mut().find(|item| item.key() == value.key()) {
            Some(item) => *item = value,
            None => items.push(value),
        }
    }

    /// Removes the item at `key`, failing with `NotFound` if absent.
    fn remove(items: &mut Vec<Self>, key: &str) -> Result<(), EditError> {
        let index = items
            .iter()
            .position(|item| item.key() == key)
            .ok_or_else(|| EditError::NotFound {
                kind: Self::KIND,
                name: key.to_owned(),
            })?;
        items.remove(index);
        Ok(())
    }
}

// Implements `Keyed` from each declaration's kind and key field.
macro_rules! keyed {
    ($($type:ty => $kind:literal, $key:ident;)*) => {$(
        impl Keyed for $type {
            const KIND: &'static str = $kind;

            fn key(&self) -> &str {
                &self.$key
            }
        }
    )*};
}

keyed! {
    Service => "service", name;
    Volume => "volume", name;
    Mount => "mount", target;
    ServiceFile => "file", target;
}

/// Errors applying a structurally valid edit to the current saved configuration.
#[derive(Debug, thiserror::Error)]
pub enum EditError {
    /// The named service, volume, environment variable, mount, or file does not exist.
    #[error("{kind} {name:?} was not found")]
    NotFound {
        /// Resource kind.
        kind: &'static str,
        /// Requested resource name.
        name: String,
    },
    /// A new resource already has this name.
    #[error("{kind} {name:?} already exists")]
    AlreadyExists {
        /// Resource kind.
        kind: &'static str,
        /// Conflicting resource name.
        name: String,
    },
    /// A nested setting is unavailable for the currently selected variant.
    #[error("{0}")]
    Incompatible(&'static str),
}

impl ApplicationEdit {
    /// Whether this edit, on an application whose repository connection is
    /// `current`, lets sync deploy what no deployment chose: it turns sync
    /// on. That takes the permission to deploy.
    #[must_use]
    pub fn deploys_on_push(&self, current: Option<&RepositoryManifest>) -> bool {
        let off = !current.is_some_and(RepositoryManifest::syncs);
        match self {
            Self::Repository(changed) => {
                off && changed.as_ref().is_some_and(RepositoryManifest::syncs)
            }
            Self::RepositorySync(sync) => off && !sync.is_off(),
            _ => false,
        }
    }

    /// The environment this edit configures, if any.
    #[must_use]
    pub fn environment(&self) -> Option<&str> {
        match self {
            Self::EnvironmentVisibility { environment, .. }
            | Self::EnvironmentRollback { environment, .. } => Some(environment),
            _ => None,
        }
    }

    /// Edits input in memory. The caller must validate the resulting manifest before saving.
    ///
    /// Removing a service also drops its public routes, its jobs, and other
    /// services' dependencies on it, and renaming a service repoints them all,
    /// so references never dangle.
    /// Removing a volume leaves mounts in place for validation to reject.
    ///
    /// # Errors
    /// Rejects missing/duplicate resources and incompatible nested fields.
    pub fn apply(self, manifest: &mut ApplicationManifest) -> Result<(), EditError> {
        match self {
            Self::Name(name) => manifest.metadata.name = name,
            Self::Repository(Some(value)) => manifest.spec.manifest = Some(value),
            Self::Repository(None) => manifest.spec.disconnect(),
            Self::RepositoryUrl(value) => Self::repository(manifest)?.repository.url = value,
            Self::RepositoryPath(value) => Self::repository(manifest)?.path = value,
            Self::RepositorySync(value) => Self::repository(manifest)?.sync = value,
            Self::AddService(service) => Keyed::add(&mut manifest.spec.services, *service)?,
            Self::RemoveService(name) => {
                Keyed::remove(&mut manifest.spec.services, &name)?;
                Self::retarget_service(manifest, &name, None);
            }
            Self::Service { name, edit } => {
                let renamed_to = match &edit {
                    ServiceEdit::Name(value) => Some(value.clone()),
                    _ => None,
                };
                edit.apply(Keyed::find(&mut manifest.spec.services, &name)?)?;
                if let Some(new_name) = renamed_to {
                    Self::retarget_service(manifest, &name, Some(&new_name));
                }
            }
            Self::Volumes(volumes) => manifest.spec.volumes = volumes,
            Self::Routes(routes) => manifest.spec.routes = routes,
            Self::Jobs(jobs) => manifest.spec.jobs = jobs,
            Self::GeneratedSecrets(secrets) => manifest.spec.secrets = secrets,
            Self::Exports(exports) => manifest.spec.exports = exports,
            Self::Imports(imports) => manifest.spec.imports = imports,
            Self::AddVolume(volume) => Keyed::add(&mut manifest.spec.volumes, volume)?,
            Self::RemoveVolume(name) => Keyed::remove(&mut manifest.spec.volumes, &name)?,
            Self::Variables(variables) => variables.apply(&mut manifest.spec),
            Self::EnvironmentVisibility {
                environment,
                visibility,
            } => manifest
                .spec
                .set_environment_visibility(&environment, visibility),
            Self::EnvironmentRollback {
                environment,
                rollback,
            } => manifest
                .spec
                .set_environment_rollback(&environment, rollback),
        }
        Ok(())
    }

    /// Returns the connected manifest repository, or `Incompatible` when none is set.
    fn repository(
        manifest: &mut ApplicationManifest,
    ) -> Result<&mut RepositoryManifest, EditError> {
        manifest
            .spec
            .manifest
            .as_mut()
            .ok_or(EditError::Incompatible(
                "connect a manifest repository before editing its settings",
            ))
    }

    /// Repoints routes, jobs, exports, and dependencies naming service `old`
    /// to `new`, or drops them when `new` is `None` because the service was
    /// removed. Renaming an exported service renames its link alias.
    fn retarget_service(manifest: &mut ApplicationManifest, old: &str, new: Option<&String>) {
        // Repoints one reference; returns whether to keep what holds it.
        let retarget = |service: &mut String| {
            service.as_str() != old || new.inspect(|new| service.clone_from(new)).is_some()
        };
        manifest
            .spec
            .routes
            .retain_mut(|route| route.service.as_mut().is_none_or(retarget));
        manifest
            .spec
            .jobs
            .retain_mut(|job| retarget(&mut job.service));
        manifest
            .spec
            .exports
            .retain_mut(|export| retarget(&mut export.service));
        for service in &mut manifest.spec.services {
            service.depends_on.retain_mut(retarget);
        }
    }
}

impl ServiceEdit {
    /// Applies this edit to one service in memory. Nested Git, build, and health
    /// settings require the matching variant to already be selected; clearing the
    /// last resource limit removes the `resources` block entirely.
    fn apply(self, service: &mut Service) -> Result<(), EditError> {
        match self {
            Self::Name(value) => service.name = value,
            Self::Source(value) => service.source = value,
            Self::Image(image) => service.source = Source::Image { image },
            Self::GitUrl(value) => Self::git(service)?.url = value,
            Self::GitBranch(value) => Self::git(service)?.branch = value,
            Self::GitCommit(value) => Self::git(service)?.commit = value,
            Self::Dockerfile(value) => {
                let Build::Docker { dockerfile, .. } = Self::build(service)?;
                *dockerfile = value;
            }
            Self::Context(value) => {
                let Build::Docker { context, .. } = Self::build(service)?;
                *context = value;
            }
            Self::Replicas(value) => service.replicas = value,
            Self::Environment(value) => service.environment = value,
            Self::EnvironmentEntry((key, value)) => match value {
                Some(value) => {
                    service.environment.insert(key, value);
                }
                None => {
                    service
                        .environment
                        .remove(&key)
                        .ok_or(EditError::NotFound {
                            kind: "environment variable",
                            name: key,
                        })?;
                }
            },
            Self::Command(value) => service.command = value,
            Self::Arguments(value) => service.arguments = value,
            Self::Mounts(value) => service.mounts = value,
            Self::Secrets(value) => service.secrets = value,
            Self::Mount(value) => Keyed::set(&mut service.mounts, value),
            Self::RemoveMount(target) => Keyed::remove(&mut service.mounts, &target)?,
            Self::Files(value) => service.files = value,
            Self::File(value) => Keyed::set(&mut service.files, value),
            Self::RemoveFile(target) => Keyed::remove(&mut service.files, &target)?,
            Self::Healthcheck(value) => service.healthcheck = value,
            Self::HealthPort(_)
            | Self::HealthPath(_)
            | Self::HealthCommand(_)
            | Self::HealthInterval(_)
            | Self::HealthTimeout(_)
            | Self::HealthStartPeriod(_)
            | Self::HealthRetries(_) => self.apply_health(Self::health(service)?)?,
            Self::Resources(value) => service.resources = value,
            Self::Cpu(value) => {
                Self::resources(service).cpu_millis = value;
                Self::clear_empty_resources(service);
            }
            Self::Memory(value) => {
                Self::resources(service).memory_bytes = value;
                Self::clear_empty_resources(service);
            }
            Self::General(value) => {
                service.source = value.source;
                service.replicas = value.replicas;
            }
            Self::Process(value) => {
                service.command = value.command;
                service.arguments = value.arguments;
            }
            Self::DependsOn(value) => service.depends_on = value,
            Self::Rollout(value) => service.rollout = value,
            Self::ReceivesTelemetry(value) => service.receives_telemetry = value,
            Self::Security(value) => service.security = value,
        }
        Ok(())
    }

    /// Applies a health check field edit to `check`, which must be the kind
    /// of check the field belongs to. `apply` passes only health field edits.
    fn apply_health(self, check: &mut HealthCheck) -> Result<(), EditError> {
        match (self, check) {
            (Self::HealthPort(value), HealthCheck::Http { port, .. }) => *port = value,
            (Self::HealthPath(value), HealthCheck::Http { path, .. }) => *path = value,
            (Self::HealthCommand(value), HealthCheck::Command { command, .. }) => {
                *command = value;
            }
            (Self::HealthInterval(value), check) => *check.timing_mut().interval_seconds = value,
            (Self::HealthTimeout(value), check) => *check.timing_mut().timeout_seconds = value,
            (Self::HealthStartPeriod(value), check) => {
                *check.timing_mut().start_period_seconds = value;
            }
            (Self::HealthRetries(value), check) => *check.timing_mut().retries = value,
            (Self::HealthPort(_), _) => {
                return Err(EditError::Incompatible(
                    "port requires an HTTP health check",
                ));
            }
            (Self::HealthPath(_), _) => {
                return Err(EditError::Incompatible(
                    "path requires an HTTP health check",
                ));
            }
            (Self::HealthCommand(_), _) => {
                return Err(EditError::Incompatible(
                    "command requires a command health check",
                ));
            }
            _ => {
                return Err(EditError::Incompatible(
                    "only health check fields edit a health check",
                ));
            }
        }
        Ok(())
    }

    /// Returns the resource limits, inserting an empty block if none exist.
    fn resources(service: &mut Service) -> &mut ResourceLimits {
        service.resources.get_or_insert(ResourceLimits {
            cpu_millis: None,
            memory_bytes: None,
        })
    }

    /// Returns the Git repository of a Git source; image sources are incompatible.
    fn git(service: &mut Service) -> Result<&mut GitRepository, EditError> {
        match &mut service.source {
            Source::Git {
                repository: SourceRepository::Git(repository),
                ..
            } => Ok(repository),
            Source::Git { .. } => Err(EditError::Incompatible(
                "edit the manifest repository to change a \"self\" source",
            )),
            Source::Image { .. } => Err(EditError::Incompatible(
                "select a Git source before editing Git settings",
            )),
        }
    }

    /// Returns the build settings of a Git source; image sources are incompatible.
    fn build(service: &mut Service) -> Result<&mut Build, EditError> {
        match &mut service.source {
            Source::Git { build, .. } => Ok(build),
            Source::Image { .. } => Err(EditError::Incompatible(
                "select a Git source before editing build settings",
            )),
        }
    }

    /// Returns the configured health check, failing when none is set.
    fn health(service: &mut Service) -> Result<&mut HealthCheck, EditError> {
        service.healthcheck.as_mut().ok_or(EditError::Incompatible(
            "configure a health check before editing its settings",
        ))
    }

    /// Drops `resources` once neither CPU nor memory is limited, since validation
    /// rejects an empty limits block.
    fn clear_empty_resources(service: &mut Service) {
        if service
            .resources
            .as_ref()
            .is_some_and(|r| r.cpu_millis.is_none() && r.memory_bytes.is_none())
        {
            service.resources = None;
        }
    }
}

/// The table of field endpoints: every HTTP endpoint that submits one
/// [`ApplicationEdit`], listed once. The daemon builds its handlers, routes
/// and `OpenAPI` entries from it, and `piqueld-client` its per-field methods
/// and `Client::edit_application`, so adding a field takes one entry here and
/// its edit variant.
///
/// `field_endpoints!(callback)` expands to `callback! { entries }`. Each
/// entry reads:
///
/// ```text
/// /// Optional docs, the operation's `OpenAPI` description.
/// operation_id: method "/api/v1/applications/{id}/..." (param = "param", ...) [body] => { edit };
/// ```
///
/// - The path parameters follow the application `{id}`, which every
///   endpoint takes first, in path order.
/// - `[body]` is `[Type { value }]` for a `{ "value": ... }` body binding
///   `value`, `[name: Type]` for a whole body binding `name`, or `[]` for
///   none.
/// - `edit` uses only the path parameters and body bindings, so it reads both
///   as an expression, which the daemon builds from a request, and as a
///   pattern, which the client matches to choose the request. The client's
///   match is exhaustive, so every edit has an endpoint, and an edit two
///   entries could match goes to the first: removals precede the entries
///   that set the same thing.
///
/// The edit and body types are named unqualified: callers import them from
/// this module and [`crate::manifest`].
#[macro_export]
macro_rules! field_endpoints {
    ($callback:ident) => {
        $callback! {
            set_application_name: put "/api/v1/applications/{id}/name" () [StringValue { value }] => { ApplicationEdit::Name(value) };
            set_application_volumes: put "/api/v1/applications/{id}/volumes" () [VolumesValue { value }] => { ApplicationEdit::Volumes(value) };
            add_application_volume: post "/api/v1/applications/{id}/volumes" () [volume: Volume] => { ApplicationEdit::AddVolume(volume) };
            /// Removes a named volume from the saved configuration.
            ///
            /// Pass `deploy=true` to also deploy the result (202 instead of 200).
            remove_application_volume: delete "/api/v1/applications/{id}/volumes/{volume}" (volume = "volume") [] => { ApplicationEdit::RemoveVolume(volume) };
            add_application_service: post "/api/v1/applications/{id}/services" () [service: Box<Service>] => { ApplicationEdit::AddService(service) };
            /// Removes a service from the saved configuration.
            ///
            /// Pass `deploy=true` to also deploy the result (202 instead of 200).
            remove_application_service: delete "/api/v1/applications/{id}/services/{service}" (service = "service") [] => { ApplicationEdit::RemoveService(service) };
            set_application_routes: put "/api/v1/applications/{id}/routes" () [RoutesValue { value }] => { ApplicationEdit::Routes(value) };
            set_application_jobs: put "/api/v1/applications/{id}/jobs" () [JobsValue { value }] => { ApplicationEdit::Jobs(value) };
            set_application_exports: put "/api/v1/applications/{id}/exports" () [ExportsValue { value }] => { ApplicationEdit::Exports(value) };
            set_application_imports: put "/api/v1/applications/{id}/imports" () [ImportsValue { value }] => { ApplicationEdit::Imports(value) };
            set_application_generated_secrets: put "/api/v1/applications/{id}/generated-secrets" () [GeneratedSecretsValue { value }] => { ApplicationEdit::GeneratedSecrets(value) };
            set_application_variables: put "/api/v1/applications/{id}/variables" () [VariablesValue { value }] => { ApplicationEdit::Variables(value) };
            /// Sets an environment's visibility ceiling.
            ///
            /// `{environment}` is the environment's name, as in
            /// `[spec.environments.<name>]`, not its ID. Every route of the environment
            /// is at most this visible; `public` restricts nothing. Same query
            /// options, idempotency, and 200/202 responses as the other editing
            /// endpoints.
            set_environment_visibility: put "/api/v1/applications/{id}/environments/{environment}/visibility" (environment = "environment") [VisibilityValue { value }] => { ApplicationEdit::EnvironmentVisibility { environment, visibility: value } };
            /// Sets what happens when an environment's rollout fails.
            ///
            /// `{environment}` is the environment's name, as in
            /// `[spec.environments.<name>]`, not its ID. `automatic` restores the
            /// environment's last successful deployment when a service update fails
            /// within its monitor window; `manual` leaves it degraded. `null` returns
            /// to the default: automatic for promoted environments, manual otherwise.
            /// Same query options, idempotency, and 200/202 responses as the other
            /// editing endpoints.
            set_environment_rollback: put "/api/v1/applications/{id}/environments/{environment}/rollback" (environment = "environment") [RollbackValue { value }] => { ApplicationEdit::EnvironmentRollback { environment, rollback: value } };
            /// Disconnects the manifest repository.
            ///
            /// The saved configuration becomes directly editable. Same query options,
            /// idempotency, and 200/202 responses as the other editing endpoints.
            disconnect_manifest_repository: delete "/api/v1/applications/{id}/repository" () [] => { ApplicationEdit::Repository(None) };
            set_manifest_repository: put "/api/v1/applications/{id}/repository" () [RepositoryValue { value }] => { ApplicationEdit::Repository(value) };
            set_manifest_repository_url: put "/api/v1/applications/{id}/repository/url" () [StringValue { value }] => { ApplicationEdit::RepositoryUrl(value) };
            set_manifest_repository_path: put "/api/v1/applications/{id}/repository/path" () [StringValue { value }] => { ApplicationEdit::RepositoryPath(value) };
            set_manifest_repository_sync: put "/api/v1/applications/{id}/repository/sync" () [RepositorySyncValue { value }] => { ApplicationEdit::RepositorySync(value) };
            set_service_name: put "/api/v1/applications/{id}/services/{service}/name" (service = "service") [StringValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::Name(value) } };
            set_service_source: put "/api/v1/applications/{id}/services/{service}/source" (service = "service") [SourceValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::Source(value) } };
            set_service_image: put "/api/v1/applications/{id}/services/{service}/source/image" (service = "service") [TemplateValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::Image(value) } };
            set_service_git_url: put "/api/v1/applications/{id}/services/{service}/source/git/url" (service = "service") [StringValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::GitUrl(value) } };
            set_service_git_branch: put "/api/v1/applications/{id}/services/{service}/source/git/branch" (service = "service") [StringValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::GitBranch(value) } };
            set_service_git_commit: put "/api/v1/applications/{id}/services/{service}/source/git/commit" (service = "service") [OptionalStringValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::GitCommit(value) } };
            set_service_dockerfile: put "/api/v1/applications/{id}/services/{service}/source/git/dockerfile" (service = "service") [TemplateValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::Dockerfile(value) } };
            set_service_context: put "/api/v1/applications/{id}/services/{service}/source/git/context" (service = "service") [TemplateValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::Context(value) } };
            set_service_replicas: put "/api/v1/applications/{id}/services/{service}/replicas" (service = "service") [ReplicasValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::Replicas(value) } };
            set_service_environment: put "/api/v1/applications/{id}/services/{service}/environment" (service = "service") [EnvironmentValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::Environment(value) } };
            /// Removes an environment variable from a service.
            ///
            /// Pass `deploy=true` to also deploy the result (202 instead of 200).
            remove_service_environment_entry: delete "/api/v1/applications/{id}/services/{service}/environment/{key}" (service = "service", key = "key") [] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::EnvironmentEntry((key, None)) } };
            set_service_environment_entry: put "/api/v1/applications/{id}/services/{service}/environment/{key}" (service = "service", key = "key") [TemplateValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::EnvironmentEntry((key, Some(value))) } };
            set_service_command: put "/api/v1/applications/{id}/services/{service}/command" (service = "service") [TemplatesValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::Command(value) } };
            set_service_arguments: put "/api/v1/applications/{id}/services/{service}/arguments" (service = "service") [TemplatesValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::Arguments(value) } };
            set_service_process: put "/api/v1/applications/{id}/services/{service}/process" (service = "service") [process: ServiceProcess] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::Process(process) } };
            set_service_general: put "/api/v1/applications/{id}/services/{service}/general" (service = "service") [general: ServiceGeneral] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::General(general) } };
            set_service_mounts: put "/api/v1/applications/{id}/services/{service}/mounts" (service = "service") [MountsValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::Mounts(value) } };
            set_service_mount: put "/api/v1/applications/{id}/services/{service}/mount" (service = "service") [mount: Mount] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::Mount(mount) } };
            remove_service_mount: delete "/api/v1/applications/{id}/services/{service}/mount" (service = "service") [StringValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::RemoveMount(value) } };
            set_service_files: put "/api/v1/applications/{id}/services/{service}/files" (service = "service") [FilesValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::Files(value) } };
            set_service_file: put "/api/v1/applications/{id}/services/{service}/file" (service = "service") [file: ServiceFile] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::File(file) } };
            remove_service_file: delete "/api/v1/applications/{id}/services/{service}/file" (service = "service") [StringValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::RemoveFile(value) } };
            set_service_secrets: put "/api/v1/applications/{id}/services/{service}/secrets" (service = "service") [SecretsValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::Secrets(value) } };
            set_service_depends_on: put "/api/v1/applications/{id}/services/{service}/depends-on" (service = "service") [StringsValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::DependsOn(value) } };
            set_service_rollout: put "/api/v1/applications/{id}/services/{service}/rollout" (service = "service") [RolloutValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::Rollout(value) } };
            set_service_receives_telemetry: put "/api/v1/applications/{id}/services/{service}/receives-telemetry" (service = "service") [BoolValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::ReceivesTelemetry(value) } };
            set_service_security: put "/api/v1/applications/{id}/services/{service}/security" (service = "service") [SecurityValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::Security(value) } };
            set_service_healthcheck: put "/api/v1/applications/{id}/services/{service}/healthcheck" (service = "service") [HealthValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::Healthcheck(value) } };
            set_service_health_port: put "/api/v1/applications/{id}/services/{service}/healthcheck/port" (service = "service") [ReplicasValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::HealthPort(value) } };
            set_service_health_path: put "/api/v1/applications/{id}/services/{service}/healthcheck/path" (service = "service") [TemplateValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::HealthPath(value) } };
            set_service_health_command: put "/api/v1/applications/{id}/services/{service}/healthcheck/command" (service = "service") [TemplatesValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::HealthCommand(value) } };
            set_service_health_interval: put "/api/v1/applications/{id}/services/{service}/healthcheck/interval" (service = "service") [SecondsValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::HealthInterval(value) } };
            set_service_health_timeout: put "/api/v1/applications/{id}/services/{service}/healthcheck/timeout" (service = "service") [SecondsValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::HealthTimeout(value) } };
            set_service_health_start_period: put "/api/v1/applications/{id}/services/{service}/healthcheck/start-period" (service = "service") [SecondsValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::HealthStartPeriod(value) } };
            set_service_health_retries: put "/api/v1/applications/{id}/services/{service}/healthcheck/retries" (service = "service") [CountValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::HealthRetries(value) } };
            set_service_resources: put "/api/v1/applications/{id}/services/{service}/resources" (service = "service") [ResourcesValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::Resources(value) } };
            set_service_cpu: put "/api/v1/applications/{id}/services/{service}/resources/cpu" (service = "service") [CpuValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::Cpu(value) } };
            set_service_memory: put "/api/v1/applications/{id}/services/{service}/resources/memory" (service = "service") [MemoryValue { value }] => { ApplicationEdit::Service { name: service, edit: ServiceEdit::Memory(value) } };
        }
    };
}

/// Revision protection and deployment policy for a field edit.
// Reserved for `edit_endpoint!`: the API contract test that classifies
// write routes for the audit trail recognises typed edits by these options.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, utoipa::IntoParams)]
#[serde(default, deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct EditOptions {
    /// Required inspected generation unless force is set.
    pub expected_generation: Option<u64>,
    /// Explicitly bypass the revision check.
    pub force: bool,
    /// Deploy the changed configuration to the application's only environment
    /// or, when its manifest declares environments, every declared one not
    /// promoted from another; omitted/false saves only.
    pub deploy: bool,
}

#[cfg(test)]
mod tests {
    use super::{ApplicationEdit, ServiceEdit};
    use crate::manifest::{
        ApplicationManifest, ApplicationSpec, Job, JobRun, Metadata, Route, Service, Source,
    };

    #[test]
    fn service_edits_keep_routes_jobs_and_dependencies_attached_to_existing_services() {
        let service = |name: &str, depends_on: &[&str]| Service {
            name: name.into(),
            source: Source::Image {
                image: "nginx:alpine".into(),
            },
            replicas: 1.into(),
            environment: std::collections::BTreeMap::default(),
            command: vec![],
            arguments: vec![],
            mounts: vec![],
            secrets: vec![],
            files: vec![],
            healthcheck: None,
            resources: None,
            depends_on: depends_on.iter().map(|&name| name.into()).collect(),
            rollout: crate::manifest::Rollout::default(),
            security: crate::manifest::ServiceSecurity::default(),
            receives_telemetry: false,
        };
        let mut manifest = ApplicationManifest {
            api_version: crate::manifest::APPLICATION_API_VERSION.into(),
            kind: crate::manifest::APPLICATION_KIND.into(),
            metadata: Metadata { name: "app".into() },
            spec: ApplicationSpec {
                services: vec![service("web", &[]), service("worker", &["web"])],
                ..ApplicationSpec::default()
            },
        };
        ApplicationEdit::Routes(vec![Route::service(
            "app.example.com".into(),
            crate::manifest::Visibility::Public,
            "web".into(),
            3000,
        )])
        .apply(&mut manifest)
        .unwrap();
        ApplicationEdit::Jobs(vec![Job {
            name: "migrate".into(),
            service: "web".into(),
            command: vec!["migrate".into()],
            environment: std::collections::BTreeMap::new(),
            secrets: Vec::new(),
            run: JobRun::BeforeRollout,
            timeout_seconds: 300,
            volume_access: crate::manifest::JobVolumeAccess::ReadOnly,
            stop_service: false,
        }])
        .apply(&mut manifest)
        .unwrap();
        ApplicationEdit::Service {
            name: "web".into(),
            edit: ServiceEdit::Name("frontend".into()),
        }
        .apply(&mut manifest)
        .unwrap();
        assert_eq!(manifest.spec.routes[0].service.as_deref(), Some("frontend"));
        assert_eq!(manifest.spec.jobs[0].service, "frontend");
        assert_eq!(manifest.spec.services[1].depends_on, ["frontend"]);
        manifest.clone().validate().unwrap();
        ApplicationEdit::RemoveService("frontend".into())
            .apply(&mut manifest)
            .unwrap();
        assert_eq!(manifest.spec.routes, [] as [Route; 0]);
        assert_eq!(manifest.spec.jobs, [] as [Job; 0]);
        assert_eq!(manifest.spec.services[0].depends_on, [] as [String; 0]);
        manifest.validate().unwrap();
    }
}
