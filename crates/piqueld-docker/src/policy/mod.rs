//! The runtime policy an observed service spec must meet to count as piqueld's own.
use super::{
    BTreeSet, BollardDocker, HealthConfig, HealthProbe, INSTANCE_LABEL, MountTypeEnum,
    NANO_CPUS_PER_MILLICORE, RESTART_DELAY, SERVICE_LABEL, ServiceSpec,
    ServiceSpecUpdateConfigFailureActionEnum, TaskSpec, TaskSpecContainerSpec,
    TaskSpecRestartPolicyConditionEnum, spec::ALL_CAPABILITIES,
};
use piqueld_core::manifest::{ContainerUser, ServiceSecurity};

/// The runtime policy emitted by the desired-service Docker specification builder.
///
/// Verifies exactly the fields piqueld authors: replication, update settings,
/// restart condition and delay, mounts, environment, network targets and
/// aliases, health checks and their probe config, file configs, resource
/// limits, security settings, and the placement pin to the local node. Other
/// fields are accepted only at known Engine defaults, so engine-defaulted
/// echo-back is not drift but unsupported non-default values are. Security
/// settings piqueld cannot express (privileged execution, added or single
/// dropped capabilities, sysctls, named users, groups, runtimes, log drivers)
/// are denied: their presence means out-of-band modification.
pub(super) struct ServiceRuntimePolicy;

impl ServiceRuntimePolicy {
    /// Returns whether an observed spec uses only settings piqueld authors, modulo
    /// known Engine defaults, including placement on `node_id`. Desired values are
    /// compared separately; `false` means out-of-policy drift that needs an update.
    pub(super) fn matches(spec: &ServiceSpec, node_id: &str) -> bool {
        let Some(task) = spec.task_template.as_ref() else {
            return false;
        };
        let Some(container) = task.container_spec.as_ref() else {
            return false;
        };
        Self::restart_policy(task)
            && Self::update_policy(spec)
            && Self::replicated_mode(spec)
            && Self::mounts(container)
            && Self::environment(container)
            && Self::networks(spec, task)
            && Self::health(container)
            && Self::resource_limits(task)
            && Self::no_unsupported_service_settings(spec)
            && Self::no_security_settings(task, container, node_id)
            && HealthProbe::matches(
                container,
                spec.labels
                    .as_ref()
                    .and_then(|labels| labels.get(INSTANCE_LABEL))
                    .map(String::as_str),
            )
    }

    /// Rejects security-sensitive drift outside the supported single-node policy.
    /// Placement must be exactly the constraint pinning tasks to `node_id`.
    fn no_security_settings(
        task: &TaskSpec,
        container: &TaskSpecContainerSpec,
        node_id: &str,
    ) -> bool {
        task.runtime
            .as_deref()
            .is_none_or(|runtime| runtime.is_empty() || runtime == "container")
            && task.log_driver.is_none()
            && task.plugin_spec.is_none()
            && task.network_attachment_spec.is_none()
            && task.placement.as_ref().is_some_and(|placement| {
                placement.constraints.as_ref()
                    == Some(&vec![BollardDocker::placement_constraint(node_id)])
                    && placement.preferences.as_ref().is_none_or(Vec::is_empty)
                    && placement.max_replicas.is_none_or(|value| value == 0)
                    && placement.platforms.as_ref().is_none_or(Vec::is_empty)
            })
            && task.force_update.is_none_or(|value| value == 0)
            && Self::security(container)
            && container.groups.as_ref().is_none_or(Vec::is_empty)
            && container
                .labels
                .as_ref()
                .is_none_or(std::collections::HashMap::is_empty)
            && container.hostname.as_deref().is_none_or(str::is_empty)
            && container.dir.as_deref().is_none_or(str::is_empty)
            && !container.tty.unwrap_or(false)
            && !container.open_stdin.unwrap_or(false)
            && container.stop_signal.as_deref().is_none_or(str::is_empty)
            && container
                .stop_grace_period
                .is_none_or(|value| value == 10_000_000_000)
            && container.hosts.as_ref().is_none_or(Vec::is_empty)
            && container.dns_config.as_ref().is_none_or(|dns| {
                dns.nameservers.as_ref().is_none_or(Vec::is_empty)
                    && dns.search.as_ref().is_none_or(Vec::is_empty)
                    && dns.options.as_ref().is_none_or(Vec::is_empty)
            })
            && container.secrets.as_ref().is_none_or(|items| {
                items.iter().all(|s| {
                    s.secret_id.is_some()
                        && s.secret_name.is_some()
                        && s.file.as_ref().is_some_and(|f| {
                            f.uid.as_deref() == Some("0")
                                && f.gid.as_deref() == Some("0")
                                && f.mode == Some(0o444)
                        })
                })
            })
            && BollardDocker::files_match_policy(container)
            && container.oom_score_adj.is_none_or(|value| value == 0)
            && container.isolation.is_none_or(|value| {
                matches!(
                    value,
                    bollard::models::TaskSpecContainerSpecIsolationEnum::EMPTY
                        | bollard::models::TaskSpecContainerSpecIsolationEnum::DEFAULT
                )
            })
            && !container.init.unwrap_or(false)
            && container
                .sysctls
                .as_ref()
                .is_none_or(std::collections::HashMap::is_empty)
            && container.capability_add.as_ref().is_none_or(Vec::is_empty)
            && container.ulimits.as_ref().is_none_or(Vec::is_empty)
    }

    /// Accepts only security settings a service's `security` expresses,
    /// which the desired service compares: a numeric user (or a telemetry
    /// receiver's, the daemon's), a read-only root exactly when `/tmp` is
    /// an in-memory mount, all capabilities dropped or none, and no new
    /// privileges as the only privilege setting.
    fn security(container: &TaskSpecContainerSpec) -> bool {
        let tmpfs = container.mounts.iter().flatten().any(|mount| {
            mount.typ == Some(MountTypeEnum::TMPFS)
                && mount.target.as_deref() == Some(ServiceSecurity::TMPFS_TARGET)
        });
        container.user.as_deref().is_none_or(|user| {
            user.parse::<ContainerUser>().is_ok()
                || BollardDocker::observed_telemetry(container)
                    .is_some_and(|telemetry| !telemetry.source.is_empty())
        }) && container.read_only.unwrap_or(false) == tmpfs
            && container
                .capability_drop
                .as_deref()
                .is_none_or(|dropped| dropped.is_empty() || dropped == [ALL_CAPABILITIES])
            && container.privileges.as_ref().is_none_or(|privileges| {
                privileges.credential_spec.is_none()
                    && privileges.se_linux_context.is_none()
                    && privileges.seccomp.is_none()
                    && privileges.app_armor.is_none()
            })
    }

    /// Accepts only Docker's default rollback config, no service-level
    /// networks, and a VIP endpoint without published ports.
    fn no_unsupported_service_settings(spec: &ServiceSpec) -> bool {
        spec.rollback_config.as_ref().is_none_or(|rollback| {
            rollback.parallelism == Some(1)
                && rollback.delay.is_none_or(|value| value == 0)
                && rollback.failure_action
                    == Some(bollard::models::ServiceSpecRollbackConfigFailureActionEnum::PAUSE)
                && rollback.monitor == Some(5 * super::NANOSECONDS_PER_SECOND)
                && rollback.max_failure_ratio == Some(0.0)
                && rollback.order
                    == Some(bollard::models::ServiceSpecRollbackConfigOrderEnum::STOP_FIRST)
        }) && spec.networks.as_ref().is_none_or(Vec::is_empty)
            && spec.endpoint_spec.as_ref().is_none_or(|endpoint| {
                endpoint.mode.is_none_or(|mode| {
                    matches!(
                        mode,
                        bollard::models::EndpointSpecModeEnum::EMPTY
                            | bollard::models::EndpointSpecModeEnum::VIP
                    )
                }) && endpoint.ports.as_ref().is_none_or(Vec::is_empty)
            })
    }

    /// Requires the authored restart-on-any-exit policy with no attempt limit.
    fn restart_policy(task: &TaskSpec) -> bool {
        task.restart_policy.as_ref().is_some_and(|restart| {
            restart.condition == Some(TaskSpecRestartPolicyConditionEnum::ANY)
                && restart.delay == Some(RESTART_DELAY)
                && restart.max_attempts.is_none_or(|value| value == 0)
                && restart.window.is_none_or(|value| value == 0)
        })
    }

    /// Requires the authored one-at-a-time update policy that pauses on
    /// failure. Its order and monitor window are compared against the desired
    /// rollout separately.
    fn update_policy(spec: &ServiceSpec) -> bool {
        spec.update_config.as_ref().is_some_and(|update| {
            update.parallelism == Some(1)
                && update.delay.is_none_or(|value| value == 0)
                && update.failure_action == Some(ServiceSpecUpdateConfigFailureActionEnum::PAUSE)
                && update.max_failure_ratio == Some(0.0)
        })
    }

    /// Requires replicated mode and no other service mode.
    fn replicated_mode(spec: &ServiceSpec) -> bool {
        spec.mode.as_ref().is_some_and(|mode| {
            mode.replicated.is_some()
                && mode.global.is_none()
                && mode.replicated_job.is_none()
                && mode.global_job.is_none()
        })
    }

    /// Requires every mount to be a plain named volume without extra
    /// options, except one writable bind mount at the telemetry target and
    /// one read-only root's in-memory `/tmp`, with a target Swarm passes on
    /// as written.
    fn mounts(container: &TaskSpecContainerSpec) -> bool {
        container.mounts.as_ref().is_none_or(|mounts| {
            let telemetry = |mount: &&bollard::models::Mount| {
                mount.typ == Some(MountTypeEnum::BIND)
                    && mount.target.as_deref() == Some(piqueld_core::resource::TELEMETRY_TARGET)
                    && !mount.read_only.unwrap_or(false)
            };
            let tmpfs = |mount: &&bollard::models::Mount| {
                mount.typ == Some(MountTypeEnum::TMPFS)
                    && mount.target.as_deref() == Some(ServiceSecurity::TMPFS_TARGET)
                    && mount.source.as_deref().is_none_or(str::is_empty)
                    && !mount.read_only.unwrap_or(false)
            };
            mounts.iter().filter(telemetry).count() <= 1
                && mounts.iter().filter(tmpfs).count() <= 1
                && mounts.iter().all(|mount| {
                    (tmpfs(&mount)
                        || (mount.typ == Some(MountTypeEnum::VOLUME) || telemetry(&mount))
                            && mount.source.as_ref().is_some_and(|value| !value.is_empty()))
                        && mount.target.as_ref().is_some_and(|value| {
                            !value.is_empty() && BollardDocker::is_swarm_literal(value)
                        })
                        && mount.consistency.as_deref().is_none_or(str::is_empty)
                        && mount.bind_options.is_none()
                        && mount.volume_options.is_none()
                        && mount.image_options.is_none()
                        && mount.tmpfs_options.is_none()
                })
        })
    }

    /// Requires well-formed `KEY=value` entries with unique, non-empty keys,
    /// whose values Swarm passes on as written.
    fn environment(container: &TaskSpecContainerSpec) -> bool {
        container.env.as_ref().is_none_or(|environment| {
            let mut keys = BTreeSet::new();
            environment.iter().all(|entry| {
                entry.split_once('=').is_some_and(|(key, value)| {
                    !key.is_empty() && keys.insert(key) && BollardDocker::is_swarm_literal(value)
                })
            })
        })
    }

    /// Requires unique, non-empty network targets without driver options,
    /// accepting only the service's logical name and its link aliases
    /// (`<logical name>.<application>`) as aliases. Which network carries
    /// each is compared against desired attachments during planning, because
    /// targets here are still Docker network IDs.
    fn networks(spec: &ServiceSpec, task: &TaskSpec) -> bool {
        let logical_name = spec
            .labels
            .as_ref()
            .and_then(|labels| labels.get(SERVICE_LABEL));
        task.networks.as_ref().is_none_or(|networks| {
            let mut targets = BTreeSet::new();
            networks.iter().all(|network| {
                network
                    .target
                    .as_ref()
                    .is_some_and(|value| !value.is_empty() && targets.insert(value.as_str()))
                    && network.aliases.as_ref().is_none_or(|aliases| {
                        aliases.iter().all(|alias| {
                            logical_name.is_some_and(|logical_name| {
                                alias == logical_name
                                    || alias
                                        .strip_prefix(logical_name.as_str())
                                        .and_then(|rest| rest.strip_prefix('.'))
                                        .is_some_and(piqueld_core::valid_logical_name)
                            })
                        })
                    })
                    && network
                        .driver_opts
                        .as_ref()
                        .is_none_or(std::collections::HashMap::is_empty)
            })
        })
    }

    /// Accepts no healthcheck or one that round-trips through observation.
    fn health(container: &TaskSpecContainerSpec) -> bool {
        container
            .health_check
            .as_ref()
            .is_none_or(Self::supported_health_config)
    }

    /// Accepts millicore-aligned CPU and non-negative memory limits, no PID
    /// limit, and only default (zero) reservations.
    fn resource_limits(task: &TaskSpec) -> bool {
        task.resources
            .as_ref()
            .and_then(|resources| resources.limits.as_ref())
            .is_none_or(|limits| {
                limits
                    .nano_cpus
                    .is_none_or(|value| value >= 0 && value % NANO_CPUS_PER_MILLICORE == 0)
                    && limits.memory_bytes.is_none_or(|value| value >= 0)
                    && limits.pids.is_none_or(|value| value == 0)
            })
            && task.resources.as_ref().is_none_or(|resources| {
                resources
                    .reservations
                    .as_ref()
                    .is_none_or(Self::default_reservations)
            })
    }

    /// Returns whether reservations are unset or zero.
    fn default_reservations(reservations: &bollard::models::ResourceObject) -> bool {
        reservations.nano_cpus.is_none_or(|value| value == 0)
            && reservations.memory_bytes.is_none_or(|value| value == 0)
            && reservations
                .generic_resources
                .as_ref()
                .is_none_or(Vec::is_empty)
    }

    /// Returns whether a health config uses positive timings, no start
    /// interval, and re-authors to the identical `test` vector, timings,
    /// retries and start period, where unset and zero are the same.
    ///
    /// The config is parsed with `observed_health` and rebuilt with
    /// `health_config`, so hand-written variants of the same check register as drift.
    fn supported_health_config(health: &HealthConfig) -> bool {
        if health.interval.is_none_or(|value| value <= 0)
            || health.timeout.is_none_or(|value| value <= 0)
            || health.start_interval.is_some_and(|value| value != 0)
        {
            return false;
        }
        BollardDocker::observed_health(health).is_some_and(|observed| {
            let canonical = BollardDocker::health_config(&observed);
            health.test == canonical.test
                && health.interval == canonical.interval
                && health.timeout == canonical.timeout
                && health.retries == canonical.retries
                && health.start_period.unwrap_or(0) == canonical.start_period.unwrap_or(0)
        })
    }
}

#[cfg(test)]
mod tests;
