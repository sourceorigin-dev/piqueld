//! Validation of each service and job, and the variables, secret mounts, resources and health checks they declare.

use super::super::variables::{Template, Typed};
use super::super::{
    FileSource, HealthCheck, Job, ResourceLimits, SecretMount, Service, ServiceFile,
    ServiceSecurity, Source, ValidatedRollout,
};
use super::{
    MAX_CPU_MILLIS, REPLICAS, ValidationError, VariableMap, lacks_executable, safe_key_echo,
    valid_image_reference, valid_repository_path, validate_name, validate_process_arguments,
};
use crate::ApiErrorCode;
use std::collections::BTreeSet;

const MAX_MOUNTS_PER_SERVICE: usize = 32;
/// Secret mounts a service, or a job in addition to its service, may declare.
const MAX_SECRET_MOUNTS: usize = 64;
const MAX_HEALTHCHECK_INTERVAL_SECONDS: u32 = 3_600;
const MAX_HEALTHCHECK_START_PERIOD_SECONDS: u32 = 3_600;
/// Accepted consecutive failures before a container is unhealthy.
const HEALTHCHECK_RETRIES: std::ops::RangeInclusive<u32> = 1..=100;

impl Service {
    /// Validates the service at `base`: name, replicas, source, secrets, files,
    /// environment, mounts, process arguments, health check, resource limits,
    /// and that nothing mounts at a read-only root's `/tmp`.
    ///
    /// `volume_names` holds declared volumes for mount references, and
    /// `manifest_backed` allows `"self"` Git sources and repository files. Secret
    /// targets must be unique under `/run/secrets/` and must not collide with
    /// mount targets; file targets collide with neither.
    pub(super) fn validate(
        &self,
        base: &str,
        volume_names: &BTreeSet<String>,
        manifest_backed: bool,
        errors: &mut Vec<ValidationError>,
    ) {
        validate_name(&self.name, &format!("{base}.name"), errors);
        if self
            .replicas
            .literal()
            .is_some_and(|replicas| !REPLICAS.contains(replicas))
        {
            errors.push(ValidationError::new(
                ApiErrorCode::ReplicasOutOfRange,
                format!("{base}.replicas"),
                format!(
                    "replicas must be between {} and {}",
                    REPLICAS.start(),
                    REPLICAS.end()
                ),
            ));
        }
        if let Source::Image { image } = &self.source
            && image
                .as_literal()
                .is_some_and(|image| !valid_image_reference(&image))
        {
            errors.push(ValidationError::new(
                ApiErrorCode::ImageInvalid,
                format!("{base}.source.image"),
                "image must be a valid registry reference without credentials or a URL scheme",
            ));
        }
        self.source.validate_git(base, manifest_backed, errors);
        let mut targets = self
            .mounts
            .iter()
            .map(|m| m.target.as_str())
            .collect::<BTreeSet<_>>();
        SecretMount::validate_all(&self.secrets, base, &mut targets, errors);
        self.validate_files(base, manifest_backed, &mut targets, errors);
        VariableMap::ENVIRONMENT.validate(&self.environment, base, errors);
        self.validate_mounts(base, volume_names, errors);
        validate_process_arguments(
            &self.command,
            &format!("{base}.command"),
            ApiErrorCode::ProcessCommandExcessive,
            errors,
        );
        validate_process_arguments(
            &self.arguments,
            &format!("{base}.arguments"),
            ApiErrorCode::ProcessArgumentsExcessive,
            errors,
        );
        if self
            .command
            .first()
            .and_then(Template::as_literal)
            .is_some_and(|value| value.trim().is_empty())
        {
            errors.push(ValidationError::new(
                ApiErrorCode::ProcessCommandInvalid,
                format!("{base}.command[0]"),
                "an explicit container command must start with a non-empty executable",
            ));
        }
        if let Some(healthcheck) = &self.healthcheck {
            healthcheck.validate(&format!("{base}.healthcheck"), errors);
        }
        if let Some(resources) = &self.resources {
            resources.validate(base, errors);
        }
        self.rollout.validate(base, errors);
        if self.security.read_only_root
            && self
                .mounts
                .iter()
                .map(|mount| &mount.target)
                .chain(self.files.iter().map(|file| &file.target))
                .any(|target| target == ServiceSecurity::TMPFS_TARGET)
        {
            errors.push(ValidationError::new(
                ApiErrorCode::ReadOnlyRootTmpMount,
                format!("{base}.security.read_only_root"),
                "a read-only root keeps /tmp writable in memory, so nothing may mount at /tmp",
            ));
        }
    }

    /// Checks file count, that each target is safe and collides with no other
    /// target in `targets` (mounts, secrets, and earlier files) or a reserved
    /// directory, that repository paths stay inside a repository the application
    /// has, and that inline contents fit one Swarm config.
    fn validate_files<'a>(
        &'a self,
        base: &str,
        manifest_backed: bool,
        targets: &mut BTreeSet<&'a str>,
        errors: &mut Vec<ValidationError>,
    ) {
        let files = &self.files;
        if files.len() > ServiceFile::MAX_PER_SERVICE {
            errors.push(ValidationError::new(
                ApiErrorCode::FileCountExcessive,
                format!("{base}.files"),
                format!(
                    "a service must mount at most {} files",
                    ServiceFile::MAX_PER_SERVICE
                ),
            ));
        }
        // Mount, secret, and file targets a file must not contain.
        let others = targets
            .iter()
            .copied()
            .chain(files.iter().map(|file| file.target.as_str()))
            .collect::<Vec<_>>();
        for (index, file) in files.iter().enumerate() {
            let path = format!("{base}.files[{index}]");
            validate_absolute_path(&file.target, &format!("{path}.target"), errors);
            if file.reserved_target()
                || others.iter().any(|other| file.contains(other))
                || !targets.insert(&file.target)
            {
                errors.push(ValidationError::new(
                    ApiErrorCode::FileTargetInvalid,
                    format!("{path}.target"),
                    format!(
                        "file targets must be unique among the service's mount, secret, and file targets, contain none of them, and stay clear of {}",
                        ServiceFile::RESERVED_DIRECTORIES.join(" and ")
                    ),
                ));
            }
            let (code, field, message) = match &file.source {
                FileSource::Path(_) if !manifest_backed => (
                    ApiErrorCode::FileRequiresRepository,
                    "path",
                    "only applications whose manifest comes from a repository can mount repository files; set content instead".to_owned(),
                ),
                FileSource::Path(relative) if !valid_repository_path(relative) => (
                    ApiErrorCode::RepositoryPathInvalid,
                    "path",
                    "file path must remain within the repository root".to_owned(),
                ),
                FileSource::Content(content) if content.is_empty() => (
                    ApiErrorCode::FileEmpty,
                    "content",
                    "file content cannot be empty".to_owned(),
                ),
                FileSource::Content(content) if content.len() > ServiceFile::MAX_BYTES => (
                    ApiErrorCode::FileTooLarge,
                    "content",
                    format!(
                        "file content must be at most {} bytes",
                        ServiceFile::MAX_BYTES
                    ),
                ),
                FileSource::Path(_) | FileSource::Content(_) => continue,
            };
            errors.push(ValidationError::new(
                code,
                format!("{path}.{field}"),
                message,
            ));
        }
    }

    /// Checks mount count, that each mount references a declared volume, and that
    /// targets are safe and unique.
    fn validate_mounts(
        &self,
        base: &str,
        volume_names: &BTreeSet<String>,
        errors: &mut Vec<ValidationError>,
    ) {
        let mounts = &self.mounts;
        let mut targets = BTreeSet::new();
        if mounts.len() > MAX_MOUNTS_PER_SERVICE {
            errors.push(ValidationError::new(
                ApiErrorCode::MountCountExcessive,
                format!("{base}.mounts"),
                format!("a service must declare at most {MAX_MOUNTS_PER_SERVICE} mounts"),
            ));
        }
        for (index, mount) in mounts.iter().enumerate() {
            let path = format!("{base}.mounts[{index}]");
            if !volume_names.contains(&mount.volume) {
                errors.push(ValidationError::new(
                    ApiErrorCode::MountVolumeMissing,
                    format!("{path}.volume"),
                    "mount references an undeclared volume",
                ));
            }
            validate_absolute_path(&mount.target, &format!("{path}.target"), errors);
            // HTTP health checks mount piqueld-probe there, and telemetry
            // receivers their socket directory.
            for (reserved, purpose) in [
                (piqueld_probe::CONTAINER_PATH, "health probe"),
                (crate::resource::TELEMETRY_TARGET, "telemetry socket"),
            ] {
                if mount.target == reserved || mount.target.starts_with(&format!("{reserved}/")) {
                    errors.push(ValidationError::new(
                        ApiErrorCode::MountTargetUnsafe,
                        format!("{path}.target"),
                        format!("mount target {reserved} is reserved for piqueld's {purpose}"),
                    ));
                }
            }
            if !targets.insert(mount.target.clone()) {
                errors.push(ValidationError::new(
                    ApiErrorCode::MountTargetDuplicate,
                    format!("{path}.target"),
                    "mount target is duplicated in this service",
                ));
            }
        }
    }
}

impl super::super::Rollout {
    /// Checks a literal rollout monitor window.
    pub(super) fn validate(&self, base: &str, errors: &mut Vec<ValidationError>) {
        if self
            .monitor_seconds
            .as_ref()
            .and_then(Typed::literal)
            .is_some_and(|seconds| !(1..=ValidatedRollout::MAX_MONITOR_SECONDS).contains(seconds))
        {
            errors.push(ValidationError::new(
                ApiErrorCode::RolloutMonitorInvalid,
                format!("{base}.rollout.monitor_seconds"),
                format!(
                    "rollout monitor must be between 1 and {} seconds",
                    ValidatedRollout::MAX_MONITOR_SECONDS
                ),
            ));
        }
    }
}

impl SecretMount {
    /// Checks the secret mounts of the service or job at `base`: literal names,
    /// their count, and targets that are unique file paths under
    /// `/run/secrets/`, also among `targets`, which collects them.
    fn validate_all<'a>(
        secrets: &'a [Self],
        base: &str,
        targets: &mut BTreeSet<&'a str>,
        errors: &mut Vec<ValidationError>,
    ) {
        for (index, secret) in secrets.iter().enumerate() {
            let path = format!("{base}.secrets[{index}]");
            // Names that reference variables are validated once rendered.
            if let Some(name) = secret.name.as_literal() {
                validate_name(&name, &format!("{path}.name"), errors);
            }
            validate_absolute_path(&secret.target, &format!("{path}.target"), errors);
            if !secret.target.starts_with("/run/secrets/") || !targets.insert(&secret.target) {
                errors.push(ValidationError::new(
                    ApiErrorCode::SecretTargetInvalid,
                    &path,
                    "secret targets must be unique file paths under /run/secrets",
                ));
            }
        }
        if secrets.len() > MAX_SECRET_MOUNTS {
            errors.push(ValidationError::new(
                ApiErrorCode::SecretsExcessive,
                base,
                format!("at most {MAX_SECRET_MOUNTS} secret file mounts per service or job"),
            ));
        }
    }
}

impl ResourceLimits {
    /// Checks optional limits: at least one limit set, CPU in `1..=MAX_CPU_MILLIS`,
    /// and memory nonzero and representable as Docker's `i64`.
    pub(super) fn validate(&self, base: &str, errors: &mut Vec<ValidationError>) {
        if self.cpu_millis.is_none() && self.memory_bytes.is_none() {
            errors.push(ValidationError::new(
                ApiErrorCode::ResourceLimitsEmpty,
                format!("{base}.resources"),
                "resource limits must configure CPU, memory, or both",
            ));
        }
        let cpu_millis = self.cpu_millis.as_ref().and_then(Typed::literal);
        let memory_bytes = self.memory_bytes.as_ref().and_then(Typed::literal);
        if cpu_millis == Some(&0) {
            errors.push(ValidationError::new(
                ApiErrorCode::CpuLimitInvalid,
                format!("{base}.resources.cpu_millis"),
                "CPU limit must be greater than zero",
            ));
        }
        if cpu_millis.is_some_and(|value| *value > MAX_CPU_MILLIS) {
            errors.push(ValidationError::new(
                ApiErrorCode::CpuLimitExcessive,
                format!("{base}.resources.cpu_millis"),
                format!("CPU limit must be at most {MAX_CPU_MILLIS} millicores"),
            ));
        }
        if memory_bytes == Some(&0)
            || memory_bytes.is_some_and(|value| i64::try_from(*value).is_err())
        {
            errors.push(ValidationError::new(
                ApiErrorCode::MemoryLimitInvalid,
                format!("{base}.resources.memory_bytes"),
                "memory limit must be greater than zero and fit the runtime value",
            ));
        }
    }
}

impl Job {
    /// Checks the environment variables and secrets the job adds to its
    /// `service`'s container, none of which may override the service's own.
    pub(super) fn validate_additions(
        &self,
        service: &Service,
        base: &str,
        errors: &mut Vec<ValidationError>,
    ) {
        VariableMap::ENVIRONMENT.validate(&self.environment, base, errors);
        for key in self.environment.keys() {
            if service.environment.contains_key(key) {
                errors.push(ValidationError::new(
                    ApiErrorCode::JobSettingConflict,
                    format!("{base}.environment.name"),
                    format!(
                        "environment key {} is already set by service {}",
                        safe_key_echo(key),
                        service.name
                    ),
                ));
            }
        }
        SecretMount::validate_all(&self.secrets, base, &mut BTreeSet::new(), errors);
        let service_targets = service
            .mounts
            .iter()
            .map(|mount| &mount.target)
            .chain(service.secrets.iter().map(|secret| &secret.target))
            .chain(service.files.iter().map(|file| &file.target))
            .collect::<BTreeSet<_>>();
        for (index, secret) in self.secrets.iter().enumerate() {
            if service_targets.contains(&secret.target) {
                errors.push(ValidationError::new(
                    ApiErrorCode::JobSettingConflict,
                    format!("{base}.secrets[{index}].target"),
                    format!(
                        "{} is already mounted by service {}",
                        secret.target, service.name
                    ),
                ));
            }
        }
    }

    /// An exclusive job needs a writable volume of its `service`, and none of
    /// its startup dependencies among `services` may mount one: they keep
    /// running while it holds them.
    pub(super) fn validate_exclusive_access(
        &self,
        service: &Service,
        services: &[Service],
        dependencies: &BTreeSet<&str>,
        base: &str,
        errors: &mut Vec<ValidationError>,
    ) {
        let volumes = service
            .mounts
            .iter()
            .filter(|mount| !mount.read_only)
            .map(|mount| mount.volume.as_str())
            .collect::<BTreeSet<_>>();
        if volumes.is_empty() {
            errors.push(ValidationError::new(
                ApiErrorCode::JobVolumeAccessInvalid,
                format!("{base}.volume_access"),
                format!(
                    "exclusive volume access requires service {} to mount a writable volume",
                    self.service
                ),
            ));
        }
        for (dependency, volume) in services
            .iter()
            .filter(|service| dependencies.contains(service.name.as_str()))
            .flat_map(|service| service.mounts.iter().map(move |mount| (service, mount)))
            .filter(|(_, mount)| volumes.contains(mount.volume.as_str()))
        {
            errors.push(ValidationError::new(
                ApiErrorCode::JobVolumeAccessInvalid,
                format!("{base}.volume_access"),
                format!(
                    "startup dependency {} mounts volume {}, which job {} needs exclusively",
                    dependency.name, volume.volume, self.name
                ),
            ));
        }
    }
}

impl HealthCheck {
    /// Validates a health check's probe, then its timing: interval in
    /// `1..=MAX_HEALTHCHECK_INTERVAL_SECONDS`, timeout in `1..=interval`,
    /// start period up to `MAX_HEALTHCHECK_START_PERIOD_SECONDS` and retries
    /// in `HEALTHCHECK_RETRIES`.
    ///
    /// HTTP paths must be normalized absolute paths without query or fragment.
    pub(super) fn validate(&self, path: &str, errors: &mut Vec<ValidationError>) {
        match self {
            HealthCheck::Http {
                port,
                path: request_path,
                ..
            } => {
                if port.literal() == Some(&0) {
                    errors.push(ValidationError::new(
                        ApiErrorCode::PortInvalid,
                        format!("{path}.port"),
                        "health-check port must be between 1 and 65535",
                    ));
                }
                let request_path = request_path.as_literal().unwrap_or_else(|| "/".into());
                let valid_path = request_path == "/"
                    || (request_path.starts_with('/')
                        && !request_path.ends_with('/')
                        && request_path
                            .split('/')
                            .skip(1)
                            .all(|part| !part.is_empty() && part != "." && part != ".."));
                if request_path.len() > 2048
                    || !valid_path
                    || request_path.contains(['\\', '?', '#'])
                    || request_path
                        .chars()
                        .any(|character| character.is_whitespace() || character.is_control())
                {
                    errors.push(ValidationError::new(
                        ApiErrorCode::HealthcheckPathInvalid,
                        format!("{path}.path"),
                        "HTTP health-check path must start with / and contain no whitespace",
                    ));
                }
            }
            HealthCheck::Command { command, .. } => {
                if lacks_executable(command)
                    || command
                        .iter()
                        .any(|value| value.as_literal().is_some_and(|value| value.contains('\0')))
                {
                    errors.push(ValidationError::new(
                        ApiErrorCode::HealthcheckCommandInvalid,
                        format!("{path}.command"),
                        "health-check command must contain at least one NUL-free argument",
                    ));
                }
                validate_process_arguments(
                    command,
                    &format!("{path}.command"),
                    ApiErrorCode::ProcessCommandExcessive,
                    errors,
                );
            }
        }
        self.validate_timing(path, errors);
    }

    /// Validates the timing part of [`Self::validate`].
    fn validate_timing(&self, path: &str, errors: &mut Vec<ValidationError>) {
        let super::super::HealthTiming {
            interval_seconds,
            timeout_seconds,
            start_period_seconds,
            retries,
        } = self.timing();
        if start_period_seconds
            .literal()
            .is_some_and(|&start_period| start_period > MAX_HEALTHCHECK_START_PERIOD_SECONDS)
        {
            errors.push(ValidationError::new(
                ApiErrorCode::HealthcheckStartPeriodExcessive,
                format!("{path}.start_period_seconds"),
                format!(
                    "health-check start period must be at most {MAX_HEALTHCHECK_START_PERIOD_SECONDS} seconds"
                ),
            ));
        }
        if retries
            .literal()
            .is_some_and(|retries| !HEALTHCHECK_RETRIES.contains(retries))
        {
            errors.push(ValidationError::new(
                ApiErrorCode::HealthcheckRetriesInvalid,
                format!("{path}.retries"),
                format!(
                    "health-check retries must be between {} and {}",
                    HEALTHCHECK_RETRIES.start(),
                    HEALTHCHECK_RETRIES.end()
                ),
            ));
        }
        let (Some(&interval), timeout) = (interval_seconds.literal(), timeout_seconds.literal())
        else {
            return;
        };
        if interval == 0 {
            errors.push(ValidationError::new(
                ApiErrorCode::HealthcheckIntervalInvalid,
                format!("{path}.interval_seconds"),
                "health-check interval must be greater than zero",
            ));
        }
        if interval > MAX_HEALTHCHECK_INTERVAL_SECONDS {
            errors.push(ValidationError::new(
            ApiErrorCode::HealthcheckIntervalExcessive,
            format!("{path}.interval_seconds"),
            format!(
                "health-check interval must be at most {MAX_HEALTHCHECK_INTERVAL_SECONDS} seconds"
            ),
        ));
        }
        if interval > 0 && timeout.is_some_and(|&timeout| timeout == 0 || timeout > interval) {
            errors.push(ValidationError::new(
                ApiErrorCode::HealthcheckTimeoutInvalid,
                format!("{path}.timeout_seconds"),
                "health-check timeout must be greater than zero and no longer than its interval",
            ));
        }
    }
}

/// Reports a container path unless it is absolute, normalized (no `.`, `..`, or
/// empty components), not `/` itself, and free of backslashes and controls.
fn validate_absolute_path(value: &str, path: &str, errors: &mut Vec<ValidationError>) {
    let components = value.split('/').skip(1);
    if Template::mentions_reference(value) {
        errors.push(ValidationError::new(
            ApiErrorCode::VariableNotAllowed,
            path,
            "container paths cannot reference variables",
        ));
    } else if !value.starts_with('/')
        || value == "/"
        || value.ends_with('/')
        || value.len() > 4096
        || value.contains('\\')
        || value.chars().any(char::is_control)
        || components
            .into_iter()
            .any(|part| part.is_empty() || part.len() > 255 || part == ".." || part == ".")
    {
        errors.push(ValidationError::new(
            ApiErrorCode::MountTargetUnsafe,
            path,
            "mount target must be an absolute, normalized container path below root",
        ));
    }
}
