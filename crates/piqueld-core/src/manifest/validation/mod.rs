//! Strict decoding and aggregate semantic validation of manifest inputs.

use super::dependencies::StartupOrder;
use super::variables::Template;
use super::{
    APPLICATION_API_VERSION, APPLICATION_KIND, ApplicationManifest, ApplicationSpec, Job,
    JobVolumeAccess, SecretGenerator, ServiceFile, Source, ValidatedApplication, ValidatedTemplate,
};
use crate::{ApiErrorCode, resource::valid_logical_name};
use std::collections::{BTreeMap, BTreeSet};

/// Most routes a manifest may declare; checked before any per-item validation.
const MAX_ROUTES: usize = 64;
/// Most services an application may declare.
const MAX_SERVICES: usize = 64;
/// Most volumes an application may declare.
const MAX_VOLUMES: usize = 64;
/// Most entries one variable map (environment or build arguments) may hold.
const MAX_VARIABLE_ENTRIES: usize = 256;
/// Variable keys and build targets share the common 255-byte identifier bound so one entry
/// cannot dominate a manifest, a container environment list, or a build command.
const MAX_IDENTIFIER_BYTES: usize = 255;
/// Longest value of a variable map entry, in bytes.
const MAX_VARIABLE_VALUE_BYTES: usize = 65_536;
/// Most elements of a command, entrypoint or argument list.
const MAX_PROCESS_ELEMENTS: usize = 128;
/// Longest single element of such a list, in bytes.
const MAX_PROCESS_ELEMENT_BYTES: usize = 4_096;
/// Most CPU a service may limit itself to, in millicores.
pub(super) const MAX_CPU_MILLIS: u32 = 1_048_576;
/// Accepted replica counts of a service.
pub(super) const REPLICAS: std::ops::RangeInclusive<u16> = 1..=100;
/// Most generated secrets an application may declare.
const MAX_GENERATED_SECRETS: usize = 64;
/// Accepted raw byte counts for random generated secrets, before encoding.
const RANDOM_SECRET_BYTES: std::ops::RangeInclusive<u16> = 16..=512;
/// Accepted RSA modulus sizes for generated private keys.
const RSA_SECRET_BITS: [u16; 3] = [2048, 3072, 4096];

/// Parses and validates a strict TOML manifest whose values may reference
/// variables, without performing I/O.
///
/// # Errors
/// Returns validation errors, or one decode error naming the rejected input
/// and its location.
pub fn parse_template_toml(input: &str) -> Result<ValidatedTemplate, ValidationErrors> {
    ApplicationManifest::decode_toml(input)?.validate_template()
}

/// Parses and validates a strict JSON manifest whose values may reference
/// variables, without performing I/O.
///
/// # Errors
/// Returns validation errors, or one decode error naming the rejected input
/// and its location.
pub(super) fn parse_template_json(input: &str) -> Result<ValidatedTemplate, ValidationErrors> {
    ApplicationManifest::decode_json(input)?.validate_template()
}

/// Parses and validates strict TOML that references no variables.
///
/// # Errors
/// Returns validation errors, including `variable_unresolved` for each
/// reference, or one decode error naming the rejected input and its location.
pub fn parse_toml(input: &str) -> Result<ValidatedApplication, ValidationErrors> {
    parse_template_toml(input)?.into_literal()
}

/// Parses and validates strict JSON that references no variables.
///
/// # Errors
/// Returns validation errors, including `variable_unresolved` for each
/// reference, or one decode error naming the rejected input and its location.
pub fn parse_json(input: &str) -> Result<ValidatedApplication, ValidationErrors> {
    parse_template_json(input)?.into_literal()
}

/// Redacts map keys and unknown components from a serde decode path.
///
/// Keeps the longest prefix made of known manifest field names with numeric
/// indices, ending at the first map field, so user-supplied map keys (e.g.
/// environment names) never leak, even when they match a field name.
///
/// ```text
/// "spec.services[0].replicas"           -> "spec.services[0].replicas"
/// "spec.services[0].environment.KEY"    -> "spec.services[0].environment"
/// "spec.services[0].environment.image"  -> "spec.services[0].environment"
/// "bogus.field"                         -> "$"
/// ```
#[must_use]
pub fn safe_decode_path(path: &str) -> String {
    const FIELDS: &[&str] = &[
        "api_version",
        "kind",
        "metadata",
        "name",
        "spec",
        "manifest",
        "services",
        "volumes",
        "source",
        "replicas",
        "environment",
        "command",
        "arguments",
        "mounts",
        "secrets",
        "healthcheck",
        "resources",
        "type",
        "image",
        "repository",
        "url",
        "branch",
        "commit",
        "build",
        "dockerfile",
        "context",
        "args",
        "volume",
        "target",
        "read_only",
        "port",
        "routes",
        "jobs",
        "run",
        "hostname",
        "service",
        "path",
        "interval_seconds",
        "timeout_seconds",
        "start_period_seconds",
        "retries",
        "cpu_millis",
        "memory_bytes",
        "generate",
        "bytes",
        "encoding",
        "bits",
        "depends_on",
        "rollout",
        "order",
        "monitor_seconds",
        "rollback",
        "variables",
        "environments",
        "exports",
        "imports",
        "application",
        "applications",
        "previews",
        "files",
        "content",
        "mode",
        "redirect",
        "to",
        "status",
        "preserve_path",
        "visibility",
        "promote_from",
        "sync",
        "volume_access",
        "stop_service",
        "receives_telemetry",
        "security",
        "user",
        "read_only_root",
        "drop_capabilities",
        "no_new_privileges",
    ];
    const MAPS: &[&str] = &["args", "environment", "environments", "variables"];
    let mut safe = Vec::new();
    for component in path.split('.') {
        let field_end = component.find('[').unwrap_or(component.len());
        let (field, indices) = component.split_at(field_end);
        if !FIELDS.contains(&field) || !valid_path_indices(indices) {
            break;
        }
        safe.push(component);
        if MAPS.contains(&field) {
            break;
        }
    }
    if safe.is_empty() {
        "$".into()
    } else {
        safe.join(".")
    }
}

/// Whether `value` is zero or more `[<digits>]` groups, e.g. `""` or `"[0][12]"`.
fn valid_path_indices(mut value: &str) -> bool {
    while !value.is_empty() {
        let Some(after_open) = value.strip_prefix('[') else {
            return false;
        };
        let Some(close) = after_open.find(']') else {
            return false;
        };
        if close == 0
            || !after_open[..close]
                .bytes()
                .all(|byte| byte.is_ascii_digit())
        {
            return false;
        }
        value = &after_open[close + 1..];
    }
    true
}

/// Explains the mutually exclusive route destinations.
pub(super) const ROUTE_TARGET_MESSAGE: &str =
    "route must set either both `service` and `port`, or `redirect`";

impl super::Redirect {
    /// Checks a literal destination URL and the status code of the redirect
    /// of the route at `path`.
    fn validate(&self, path: &str, errors: &mut Vec<ValidationError>) {
        if let Some(Err(source)) = self.to.as_literal().map(super::RedirectUrl::parse) {
            errors.push(ValidationError::new(
                ApiErrorCode::RouteRedirectInvalid,
                format!("{path}.redirect.to"),
                source.to_string(),
            ));
        }
        if let Err(source) = super::RedirectStatus::try_from(self.status) {
            errors.push(ValidationError::new(
                ApiErrorCode::RouteRedirectStatusInvalid,
                format!("{path}.redirect.status"),
                source.to_string(),
            ));
        }
    }
}

/// Whether a redirect from `hostname` to `to` reaches the route itself: its
/// own hostname, or a name its wildcard matches that no exact route in
/// `exact` claims.
fn redirect_loops(hostname: &str, to: &str, exact: &BTreeSet<String>) -> bool {
    let (Ok(hostname), Ok(to)) = (
        super::RouteHostname::parse(hostname),
        super::RedirectUrl::parse(to),
    ) else {
        return false;
    };
    hostname.as_str() == to.hostname()
        || super::Hostname::parse(to.hostname())
            .is_ok_and(|to| hostname.matches(&to) && !exact.contains(to.as_str()))
}

impl ApplicationManifest {
    /// Canonicalizes route hostnames in place (lowercase, trailing dot removed),
    /// then checks hostname syntax, uniqueness, the target service, the port,
    /// and that redirects do not loop.
    /// Hostnames that reference variables are checked once rendered. Names
    /// are literal, so they are checked here and stay the same everywhere.
    fn validate_routes(&mut self, errors: &mut Vec<ValidationError>) {
        let mut hostnames = BTreeSet::new();
        let mut names = BTreeSet::new();
        for (index, route) in self.spec.routes.iter_mut().enumerate() {
            if let Some(name) = &route.name {
                let path = format!("spec.routes[{index}].name");
                validate_name(name, &path, errors);
                if !names.insert(name.clone()) {
                    errors.push(ValidationError::new(
                        ApiErrorCode::RouteNameDuplicate,
                        &path,
                        "route name is already used in this application",
                    ));
                }
            }
            let hostname = route
                .hostname
                .as_literal()
                .map(|hostname| super::RouteHostname::canonicalize(&hostname));
            if let Some(hostname) = &hostname {
                route.hostname = Template::literal(hostname);
            }
            let path = format!("spec.routes[{index}]");
            if hostname
                .as_ref()
                .is_some_and(|hostname| super::RouteHostname::parse(hostname).is_err())
            {
                errors.push(ValidationError::new(
                    ApiErrorCode::RouteHostnameInvalid,
                    format!("{path}.hostname"),
                    "an exact public ASCII DNS hostname, or `*.` followed by one, is required",
                ));
            }
            if let Some(hostname) = hostname.clone()
                && !hostnames.insert(hostname)
            {
                errors.push(ValidationError::new(
                    ApiErrorCode::RouteHostnameDuplicate,
                    format!("{path}.hostname"),
                    "hostname is already used in this application",
                ));
            }
            match (&route.service, route.port, &route.redirect) {
                (Some(service), Some(port), None) => {
                    if !self.spec.services.iter().any(|s| &s.name == service) {
                        errors.push(ValidationError::new(
                            ApiErrorCode::RouteServiceMissing,
                            format!("{path}.service"),
                            "route must reference a service in this application",
                        ));
                    }
                    if port == 0 {
                        errors.push(ValidationError::new(
                            ApiErrorCode::RoutePortInvalid,
                            format!("{path}.port"),
                            "HTTP backend port must be 1..=65535",
                        ));
                    }
                }
                (None, None, Some(redirect)) => redirect.validate(&path, errors),
                _ => errors.push(ValidationError::new(
                    ApiErrorCode::RouteTargetInvalid,
                    &path,
                    ROUTE_TARGET_MESSAGE,
                )),
            }
        }
        for (index, route) in self.spec.routes.iter().enumerate() {
            if let (Some(hostname), Some(to)) = (
                route.hostname.as_literal(),
                route.redirect.as_ref().and_then(|r| r.to.as_literal()),
            ) && redirect_loops(&hostname, &to, &hostnames)
            {
                errors.push(ValidationError::new(
                    ApiErrorCode::RouteRedirectLoop,
                    format!("spec.routes[{index}].redirect.to"),
                    "redirect must target a hostname the route does not serve",
                ));
            }
        }
    }

    /// Decodes strict TOML without validating it, so callers can adjust the
    /// manifest (e.g. replace `spec.manifest`) before
    /// [`Self::validate_template`].
    ///
    /// # Errors
    /// Returns one decode error naming the rejected input and its location.
    pub fn decode_toml(input: &str) -> Result<Self, ValidationErrors> {
        let decode = |path: &str, error: &toml::de::Error| {
            ValidationErrors::decode(path, crate::TomlDiagnostic::new(input, error).to_string())
        };
        let deserializer = toml::Deserializer::parse(input).map_err(|error| decode(".", &error))?;
        serde_path_to_error::deserialize(deserializer)
            .map_err(|error| decode(&error.path().to_string(), error.inner()))
    }

    /// Decodes strict JSON without validating it; see [`Self::decode_toml`].
    ///
    /// # Errors
    /// Returns one decode error naming the rejected input and its location.
    pub fn decode_json(input: &str) -> Result<Self, ValidationErrors> {
        let mut deserializer = serde_json::Deserializer::from_str(input);
        let manifest = serde_path_to_error::deserialize(&mut deserializer).map_err(|error| {
            ValidationErrors::decode(&error.path().to_string(), error.inner().to_string())
        })?;
        deserializer
            .end()
            .map_err(|error| ValidationErrors::decode("$", error.to_string()))?;
        Ok(manifest)
    }

    /// Validates a manifest whose values may reference variables, as far as
    /// its literal values allow. Values that reference variables are checked
    /// again once rendered for an environment.
    ///
    /// Collects every independent error rather than stopping at the first,
    /// sorted by path then code: header, metadata and repository manifest
    /// source; the route, service and volume budgets (exceeding one returns
    /// early); then routes, names, services, volumes and variables. On
    /// success, canonicalizes literal hostnames and image registries.
    ///
    /// # Errors
    /// Returns all detected manifest validation errors.
    pub fn validate_template(mut self) -> Result<ValidatedTemplate, ValidationErrors> {
        let mut errors = Vec::new();
        self.validate_header(&mut errors);
        if let Some(manifest) = &self.spec.manifest {
            manifest
                .repository
                .validate("spec.manifest.repository", &mut errors);
            manifest.sync.validate("spec.manifest.sync", &mut errors);
            if Template::mentions_reference(&manifest.path) {
                errors.push(ValidationError::new(
                    ApiErrorCode::VariableNotAllowed,
                    "spec.manifest.path",
                    "the manifest path cannot reference variables",
                ));
            } else if !valid_repository_path(&manifest.path) {
                errors.push(ValidationError::new(
                    ApiErrorCode::RepositoryPathInvalid,
                    "spec.manifest.path",
                    "manifest path must remain within the repository root",
                ));
            }
        }
        if !self.spec.within_budgets(&mut errors) {
            return Err(ValidationErrors::sorted(errors));
        }
        self.validate_routes(&mut errors);
        unique_names(
            self.spec.services.iter().map(|service| &service.name),
            "spec.services",
            ApiErrorCode::ServiceNameDuplicate,
            &mut errors,
        );
        let volume_names = unique_names(
            self.spec.volumes.iter().map(|volume| &volume.name),
            "spec.volumes",
            ApiErrorCode::VolumeNameDuplicate,
            &mut errors,
        );
        for (index, service) in self.spec.services.iter().enumerate() {
            service.validate(
                &format!("spec.services[{index}]"),
                &volume_names,
                self.spec.manifest.is_some(),
                &mut errors,
            );
        }
        self.spec.validate_dependencies(&mut errors);
        self.spec.validate_telemetry_receiver(&mut errors);
        if self.spec.inline_file_bytes() > ServiceFile::MAX_TOTAL_BYTES {
            errors.push(ValidationError::new(
                ApiErrorCode::FileTotalExcessive,
                "spec.services",
                format!(
                    "inline file contents must total at most {} bytes",
                    ServiceFile::MAX_TOTAL_BYTES
                ),
            ));
        }
        self.spec.validate_volumes(&mut errors);
        self.spec.validate_generated_secrets(&mut errors);
        self.spec.validate_jobs(&mut errors);
        self.spec.validate_links(&mut errors);
        self.spec.check_variables(&mut errors);
        if let Err(invalid) = self.spec.declared_environments() {
            errors.extend(invalid.0);
        }
        if !errors.is_empty() {
            return Err(ValidationErrors::sorted(errors));
        }
        for service in &mut self.spec.services {
            if let Source::Image { image } = &mut service.source
                && let Some(literal) = image.as_literal()
                && let Some(parts) = ImageParts::parse(&literal)
            {
                *image = Template::literal(&parts.canonical());
            }
        }
        Ok(ValidatedTemplate::new(
            super::domain::ValidatedMetadata::from_input(&self.metadata)?,
            self.spec,
        ))
    }

    /// Validates a manifest that references no variables.
    ///
    /// # Errors
    /// Returns all detected manifest validation errors, including
    /// `variable_unresolved` for each reference.
    pub fn validate(self) -> Result<ValidatedApplication, ValidationErrors> {
        self.validate_template()?.into_literal()
    }

    /// Checks the API version, kind, and application name.
    fn validate_header(&self, errors: &mut Vec<ValidationError>) {
        if self.api_version != APPLICATION_API_VERSION {
            errors.push(ValidationError::new(
                ApiErrorCode::ApiVersionUnsupported,
                "api_version",
                "unsupported application API version",
            ));
        }
        if self.kind != APPLICATION_KIND {
            errors.push(ValidationError::new(
                ApiErrorCode::KindUnsupported,
                "kind",
                "resource kind must be Application",
            ));
        }
        validate_name(&self.metadata.name, "metadata.name", errors);
    }
}

impl ApplicationSpec {
    /// Checks route, service, volume, secret, and per-service dependency counts;
    /// returns `false` when a budget is exceeded.
    fn within_budgets(&self, errors: &mut Vec<ValidationError>) -> bool {
        let mut within_budget = true;
        if self.routes.len() > MAX_ROUTES {
            errors.push(ValidationError::new(
                ApiErrorCode::RoutesLimit,
                "spec.routes",
                format!("at most {MAX_ROUTES} routes are allowed per application"),
            ));
            within_budget = false;
        }
        if self.services.len() > MAX_SERVICES {
            errors.push(ValidationError::new(
                ApiErrorCode::ServiceCountExcessive,
                "spec.services",
                format!("an application must declare at most {MAX_SERVICES} services"),
            ));
            within_budget = false;
        } else {
            for (index, service) in self.services.iter().enumerate() {
                if service.depends_on.len() > MAX_SERVICES {
                    errors.push(ValidationError::new(
                        ApiErrorCode::ServiceDependencyCountExcessive,
                        format!("spec.services[{index}].depends_on"),
                        format!("a service must list at most {MAX_SERVICES} dependencies"),
                    ));
                    within_budget = false;
                }
            }
        }
        if self.volumes.len() > MAX_VOLUMES {
            errors.push(ValidationError::new(
                ApiErrorCode::VolumeCountExcessive,
                "spec.volumes",
                format!("an application must declare at most {MAX_VOLUMES} volumes"),
            ));
            within_budget = false;
        }
        if self.secrets.len() > MAX_GENERATED_SECRETS {
            errors.push(ValidationError::new(
                ApiErrorCode::SecretsExcessive,
                "spec.secrets",
                format!("an application must declare at most {MAX_GENERATED_SECRETS} secrets"),
            ));
            within_budget = false;
        }
        if self.jobs.len() > Job::MAX_PER_APPLICATION {
            errors.push(ValidationError::new(
                ApiErrorCode::JobCountExcessive,
                "spec.jobs",
                format!(
                    "an application must declare at most {} jobs",
                    Job::MAX_PER_APPLICATION
                ),
            ));
            within_budget = false;
        }
        self.links_within_budget(errors) && within_budget
    }

    /// Dependencies must name other services once each and must not form a cycle.
    fn validate_dependencies(&self, errors: &mut Vec<ValidationError>) {
        let services = &self.services;
        let names = services
            .iter()
            .map(|service| service.name.as_str())
            .collect::<BTreeSet<_>>();
        for (index, service) in services.iter().enumerate() {
            let mut listed = BTreeSet::new();
            for (dependency_index, dependency) in service.depends_on.iter().enumerate() {
                let path = format!("spec.services[{index}].depends_on[{dependency_index}]");
                if !names.contains(dependency.as_str()) {
                    errors.push(ValidationError::new(
                        ApiErrorCode::ServiceDependencyMissing,
                        &path,
                        "dependency must name a service in this application",
                    ));
                } else if !listed.insert(dependency) {
                    errors.push(ValidationError::new(
                        ApiErrorCode::ServiceDependencyDuplicate,
                        &path,
                        "dependency is listed more than once",
                    ));
                }
            }
        }
        let (_, cyclic) = services.startup_order();
        let cyclic = cyclic
            .into_iter()
            .map(|service| service.name.as_str())
            .collect::<BTreeSet<_>>();
        for (index, service) in services.iter().enumerate() {
            if cyclic.contains(service.name.as_str()) {
                errors.push(ValidationError::new(
                    ApiErrorCode::ServiceDependencyCycle,
                    format!("spec.services[{index}].depends_on"),
                    "dependencies must not form or lead into a cycle",
                ));
            }
        }
    }

    /// At most one service of an application receives the daemon's
    /// telemetry, with at most one replica, since each environment has one
    /// telemetry socket and one collector listens on it. It runs as the
    /// daemon's user, so it sets none of its own.
    fn validate_telemetry_receiver(&self, errors: &mut Vec<ValidationError>) {
        let receivers = self
            .services
            .iter()
            .enumerate()
            .filter(|(_, service)| service.receives_telemetry);
        for (position, (index, service)) in receivers.enumerate() {
            if position > 0 {
                errors.push(ValidationError::new(
                    ApiErrorCode::TelemetryReceiverDuplicate,
                    format!("spec.services[{index}].receives_telemetry"),
                    "at most one service of an application may receive telemetry",
                ));
            }
            if service
                .replicas
                .literal()
                .is_some_and(|replicas| *replicas > 1)
            {
                errors.push(ValidationError::new(
                    ApiErrorCode::TelemetryReceiverReplicas,
                    format!("spec.services[{index}].replicas"),
                    "a service receiving telemetry runs at most one replica, since one collector listens on its socket",
                ));
            }
            if service.security.user.is_some() {
                errors.push(ValidationError::new(
                    ApiErrorCode::TelemetryReceiverUser,
                    format!("spec.services[{index}].security.user"),
                    "a service receiving telemetry runs as the daemon's user, so it can't set its own",
                ));
            }
        }
    }

    /// Checks every job: a unique valid name, a declared service, an order after
    /// the jobs of its service's startup dependencies, a command, the
    /// environment and secrets it adds, its volume access, and its timeout.
    fn validate_jobs(&self, errors: &mut Vec<ValidationError>) {
        let (jobs, services) = (&self.jobs, &self.services);
        unique_names(
            jobs.iter().map(|job| &job.name),
            "spec.jobs",
            ApiErrorCode::JobNameDuplicate,
            errors,
        );
        for (index, job) in jobs.iter().enumerate() {
            let base = format!("spec.jobs[{index}]");
            validate_name(&job.name, &format!("{base}.name"), errors);
            let service = services.iter().find(|service| service.name == job.service);
            if service.is_none() {
                errors.push(ValidationError::new(
                    ApiErrorCode::JobServiceMissing,
                    format!("{base}.service"),
                    "job must reference a service in this application",
                ));
            }
            let dependencies = services.dependencies_of(&job.service);
            for prerequisite in jobs[index..]
                .iter()
                .filter(|prerequisite| dependencies.contains(prerequisite.service.as_str()))
            {
                errors.push(ValidationError::new(
                    ApiErrorCode::JobDependencyOrderInvalid,
                    format!("{base}.service"),
                    format!(
                        "job {} for dependency service {} must run before job {}",
                        prerequisite.name, prerequisite.service, job.name
                    ),
                ));
            }
            if lacks_executable(&job.command) {
                errors.push(ValidationError::new(
                    ApiErrorCode::ProcessCommandInvalid,
                    format!("{base}.command"),
                    "a job command must start with a non-empty executable",
                ));
            }
            validate_process_arguments(
                &job.command,
                &format!("{base}.command"),
                ApiErrorCode::ProcessCommandExcessive,
                errors,
            );
            if let Some(service) = service {
                job.validate_additions(service, &base, errors);
                if job.volume_access == JobVolumeAccess::Exclusive {
                    job.validate_exclusive_access(service, services, &dependencies, &base, errors);
                }
            }
            if !(1..=Job::MAX_TIMEOUT_SECONDS).contains(&job.timeout_seconds) {
                errors.push(ValidationError::new(
                    ApiErrorCode::JobTimeoutInvalid,
                    format!("{base}.timeout_seconds"),
                    format!(
                        "job timeout must be between 1 and {} seconds",
                        Job::MAX_TIMEOUT_SECONDS
                    ),
                ));
            }
        }
    }

    /// Checks each declared volume name.
    fn validate_volumes(&self, errors: &mut Vec<ValidationError>) {
        for (index, volume) in self.volumes.iter().enumerate() {
            validate_name(&volume.name, &format!("spec.volumes[{index}].name"), errors);
        }
    }

    /// Checks generated secret declarations: unique valid names, random byte counts
    /// in `RANDOM_SECRET_BYTES`, and RSA sizes in `RSA_SECRET_BITS`. Invalid
    /// generator parameters are reported at `spec.secrets[i].generate.bytes` or
    /// `.bits` with `secret_generator_invalid`.
    fn validate_generated_secrets(&self, errors: &mut Vec<ValidationError>) {
        let secrets = &self.secrets;
        unique_names(
            secrets.iter().map(|secret| &secret.name),
            "spec.secrets",
            ApiErrorCode::SecretNameDuplicate,
            errors,
        );
        for (index, secret) in secrets.iter().enumerate() {
            let path = format!("spec.secrets[{index}]");
            validate_name(&secret.name, &format!("{path}.name"), errors);
            match secret.generate {
                SecretGenerator::Random { bytes, .. } if !RANDOM_SECRET_BYTES.contains(&bytes) => {
                    errors.push(ValidationError::new(
                        ApiErrorCode::SecretGeneratorInvalid,
                        format!("{path}.generate.bytes"),
                        format!(
                            "random secrets must use {}-{} bytes",
                            RANDOM_SECRET_BYTES.start(),
                            RANDOM_SECRET_BYTES.end()
                        ),
                    ));
                }
                SecretGenerator::Rsa { bits } if !RSA_SECRET_BITS.contains(&bits) => {
                    errors.push(ValidationError::new(
                        ApiErrorCode::SecretGeneratorInvalid,
                        format!("{path}.generate.bits"),
                        "RSA keys must use 2048, 3072, or 4096 bits",
                    ));
                }
                _ => {}
            }
        }
    }
}

/// Rules for a validated name/value map passed to a process, such as a
/// container environment or Docker build arguments.
struct VariableMap {
    /// Human-readable entry kind used in messages.
    noun: &'static str,
    /// Field name below the validated path.
    field: &'static str,
    /// Name piqueld sets itself for ingress, which users cannot declare.
    reserved: Option<&'static str>,
    /// Code reported for an invalid entry name.
    name_invalid: ApiErrorCode,
    /// Code reported for an invalid entry value.
    value_invalid: ApiErrorCode,
    /// Code reported when there are too many entries.
    count_excessive: ApiErrorCode,
    /// Code reported when a value is too long.
    value_excessive: ApiErrorCode,
}

impl VariableMap {
    /// Service or job environment, validated at `<base>.environment`.
    const ENVIRONMENT: Self = Self {
        noun: "environment",
        field: "environment",
        reserved: Some(crate::resource::INGRESS_PROXIES_ENV),
        name_invalid: ApiErrorCode::EnvironmentNameInvalid,
        value_invalid: ApiErrorCode::EnvironmentValueInvalid,
        count_excessive: ApiErrorCode::EnvironmentCountExcessive,
        value_excessive: ApiErrorCode::EnvironmentValueExcessive,
    };
    /// Docker build arguments, validated at `<build>.args`.
    const BUILD_ARGS: Self = Self {
        noun: "build argument",
        field: "args",
        reserved: None,
        name_invalid: ApiErrorCode::BuildArgNameInvalid,
        value_invalid: ApiErrorCode::BuildArgValueInvalid,
        count_excessive: ApiErrorCode::BuildArgCountExcessive,
        value_excessive: ApiErrorCode::BuildArgValueExcessive,
    };

    /// Appends errors at `base.field` when `values` has too many entries, a
    /// name is not a POSIX-style identifier or exceeds the identifier bound, or
    /// a literal value contains NUL or exceeds its byte budget. Values that
    /// reference variables are checked once rendered.
    fn validate(
        &self,
        values: &BTreeMap<String, Template>,
        base: &str,
        errors: &mut Vec<ValidationError>,
    ) {
        let Self { noun, field, .. } = self;
        if values.len() > MAX_VARIABLE_ENTRIES {
            errors.push(ValidationError::new(
                self.count_excessive,
                format!("{base}.{field}"),
                format!("at most {MAX_VARIABLE_ENTRIES} {noun} entries are allowed"),
            ));
        }
        for (key, value) in values {
            let key_echo = safe_key_echo(key);
            if Template::mentions_reference(key) {
                errors.push(ValidationError::new(
                    ApiErrorCode::VariableNotAllowed,
                    format!("{base}.{field}.name"),
                    format!("{noun} key {key_echo} cannot reference variables"),
                ));
            } else if !valid_env_name(key) || key.len() > MAX_IDENTIFIER_BYTES {
                errors.push(ValidationError::new(
                    self.name_invalid,
                    format!("{base}.{field}.name"),
                    format!(
                        "{noun} key {key_echo} is invalid: names must use letters, digits, and underscores, cannot start with a digit, and must be at most {MAX_IDENTIFIER_BYTES} bytes"
                    ),
                ));
            }
            if self.reserved == Some(key.as_str()) {
                errors.push(ValidationError::new(
                    self.name_invalid,
                    format!("{base}.{field}.name"),
                    format!("{noun} key {key_echo} is reserved for piqueld ingress"),
                ));
            }
            let Some(value) = value.as_literal() else {
                continue;
            };
            if value.contains('\0') {
                errors.push(ValidationError::new(
                    self.value_invalid,
                    format!("{base}.{field}.value"),
                    format!("{noun} value for key {key_echo} cannot contain NUL"),
                ));
            }
            if value.len() > MAX_VARIABLE_VALUE_BYTES {
                errors.push(ValidationError::new(
                    self.value_excessive,
                    format!("{base}.{field}.value"),
                    format!(
                        "{noun} value for key {key_echo} must be at most {MAX_VARIABLE_VALUE_BYTES} bytes"
                    ),
                ));
            }
        }
    }
}

/// Quotes a user-supplied key for error messages, truncated to 64 characters with
/// control characters replaced by `U+FFFD`.
fn safe_key_echo(key: &str) -> String {
    let truncated: String = key.chars().take(64).collect();
    let sanitized = truncated
        .chars()
        .map(|character| {
            if character.is_control() {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect::<String>();
    format!("'{sanitized}'")
}

/// Reports duplicates at `{path}[index].name` and returns the set of distinct names.
fn unique_names<'a>(
    names: impl Iterator<Item = &'a String>,
    path: &str,
    duplicate_code: ApiErrorCode,
    errors: &mut Vec<ValidationError>,
) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for (index, name) in names.enumerate() {
        if !found.insert(name.clone()) {
            errors.push(ValidationError::new(
                duplicate_code,
                format!("{path}[{index}].name"),
                "name is duplicated",
            ));
        }
    }
    found
}

/// Reports `NAME_INVALID` unless `value` is a safe logical resource name.
pub(super) fn validate_name(value: &str, path: &str, errors: &mut Vec<ValidationError>) {
    if Template::mentions_reference(value) {
        errors.push(ValidationError::new(
            ApiErrorCode::VariableNotAllowed,
            path,
            "names cannot reference variables",
        ));
    } else if !valid_logical_name(value) {
        errors.push(ValidationError::new(
            ApiErrorCode::NameInvalid,
            path,
            "names must be 1-63 lowercase letters, digits, or hyphens, start with a letter, and end with a letter or digit",
        ));
    }
}

/// Whether an environment key or build argument name is a POSIX-style identifier
/// (`[A-Za-z_][A-Za-z0-9_]*`).
fn valid_env_name(value: &str) -> bool {
    !value.is_empty()
        && !value.as_bytes()[0].is_ascii_digit()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Whether `command` has no executable: it is empty, or its first element is
/// blank text. A first element that references variables is checked once
/// rendered.
fn lacks_executable(command: &[Template]) -> bool {
    command.first().is_none_or(|value| {
        value
            .as_literal()
            .is_some_and(|value| value.trim().is_empty())
    })
}

/// Bounds a command or argument list by element count and size, and rejects NUL
/// bytes. `excessive_code` distinguishes commands from arguments.
fn validate_process_arguments(
    values: &[Template],
    path: &str,
    excessive_code: ApiErrorCode,
    errors: &mut Vec<ValidationError>,
) {
    if values.len() > MAX_PROCESS_ELEMENTS {
        errors.push(ValidationError::new(
            excessive_code,
            path,
            format!("the list must contain at most {MAX_PROCESS_ELEMENTS} elements"),
        ));
    }
    for (index, value) in values.iter().enumerate() {
        let Some(value) = value.as_literal() else {
            continue;
        };
        if value.contains('\0') {
            errors.push(ValidationError::new(
                ApiErrorCode::ProcessArgumentInvalid,
                format!("{path}[{index}]"),
                "container process arguments cannot contain NUL",
            ));
        }
        if value.len() > MAX_PROCESS_ELEMENT_BYTES {
            errors.push(ValidationError::new(
                excessive_code,
                format!("{path}[{index}]"),
                format!("each element must be at most {MAX_PROCESS_ELEMENT_BYTES} bytes"),
            ));
        }
    }
}

mod errors;
mod images;
mod services;
mod sources;
pub use errors::*;
pub(crate) use images::*;
pub use sources::*;
