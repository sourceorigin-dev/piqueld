//! Every stable machine-readable code the API returns.

use crate::names::{compared_as_string, string_enum};

string_enum! {
    #[serde]
    /// A stable machine-readable code the API returns: the `code` of an
    /// error response, and of each validation error, compilation error, plan
    /// diagnostic and status diagnostic it reports. A code never changes, but
    /// new ones are added, so a client should expect codes it doesn't know.
    pub enum ApiErrorCode {
        /// 400: the request is invalid, such as a malformed `Idempotency-Key`,
        /// an event stream asked for newest-first or with a bad
        /// `Last-Event-ID`, or input the store refuses.
        InvalidRequest => "invalid_request",
        /// 400: a URL path parameter is malformed.
        PathInvalid => "path_invalid",
        /// 400: a query parameter is malformed, or contradicts another.
        QueryInvalid => "query_invalid",
        /// 400: a listing's page size or cursor is invalid.
        PaginationInvalid => "pagination_invalid",
        /// 400: a build output query is malformed.
        BuildQueryInvalid => "build_query_invalid",
        /// 400: the `force` query parameter is neither `true` nor `false`.
        ForceInvalid => "force_invalid",
        /// 400: a log query's tail or time window is out of range.
        LogsQueryInvalid => "logs_query_invalid",
        /// 400: a traffic query's window is out of range.
        TrafficQueryInvalid => "traffic_query_invalid",
        /// 400: a request header is malformed or repeated.
        HeaderInvalid => "header_invalid",
        /// 400: a TOML apply or plan request's `X-Expected-Generation` or
        /// `X-Expected-Application-Id` header is malformed or repeated.
        GenerationInvalid => "generation_invalid",
        /// 415: the body's `Content-Type` is not one the endpoint accepts.
        ContentTypeUnsupported => "content_type_unsupported",
        /// 400: the JSON body is malformed or does not match its schema.
        JsonMalformed => "json_malformed",
        /// 400: the TOML body is malformed or does not match the application
        /// schema; `details.errors` lists the decode errors.
        TomlMalformed => "toml_malformed",
        /// 413: the request body is larger than the API accepts.
        RequestBodyTooLarge => "request_body_too_large",
        /// 400: the request body could not be read.
        RequestBodyUnreadable => "request_body_unreadable",
        /// 408: the request body stopped arriving.
        RequestBodyTimeout => "request_body_timeout",
        /// 404: no API endpoint has this path.
        EndpointNotFound => "endpoint_not_found",
        /// 405: the endpoint does not take this method; `Allow` lists those
        /// it takes.
        MethodNotAllowed => "method_not_allowed",
        /// 404: the resource does not exist, or the caller may not see it.
        NotFound => "not_found",
        /// 409: the `Idempotency-Key` was already used for different input.
        RequestIdConflict => "request_id_conflict",
        /// 426: the exec endpoint was called without a WebSocket upgrade.
        UpgradeRequired => "upgrade_required",
        /// 400: the first exec WebSocket message is not an `ExecRequest`.
        ExecRequestMissing => "exec_request_missing",
        /// 408: no `ExecRequest` arrived within 30 seconds of connecting.
        ExecRequestTimeout => "exec_request_timeout",
        /// 400: an application ID is malformed.
        ApplicationIdInvalid => "application_id_invalid",
        /// 400: an environment ID is malformed.
        EnvironmentIdInvalid => "environment_id_invalid",
        /// 400: a preview slot is malformed.
        PreviewSlotInvalid => "preview_slot_invalid",
        /// 400: a notification destination name is malformed.
        DestinationNameInvalid => "destination_name_invalid",
        /// 400: an environment pins a commit without following a branch.
        RevisionInvalid => "revision_invalid",
        /// 400: a promoted environment also names a branch or commit.
        SourceInvalid => "source_invalid",
        /// 400: a release ID is malformed.
        ReleaseInvalid => "release_invalid",
        /// 400: a promotion names both a source deployment and a release.
        PromotionInvalid => "promotion_invalid",

        /// 401: the request carries no valid session or token.
        AuthenticationRequired => "authentication_required",
        /// 400: an authentication request is malformed or expired.
        AuthenticationInvalid => "authentication_invalid",
        /// 429: too many authentication requests are running.
        AuthenticationBusy => "authentication_busy",
        /// 500: an authentication operation failed in the daemon.
        AuthenticationFailed => "authentication_failed",
        /// 409: first-account setup is already completed.
        SetupCompleted => "setup_completed",
        /// 409: first-account setup is not completed yet.
        SetupPending => "setup_pending",
        /// 409: the account name or passkey is already registered.
        AccountConflict => "account_conflict",
        /// 409: the change would leave no account able to manage accounts.
        AccountLockout => "account_lockout",
        /// 403: the request's `Origin` is not the configured website.
        OriginMismatch => "origin_mismatch",
        /// 403: the request's authority or browser fetch metadata is not
        /// trusted.
        BrowserAccessDenied => "browser_access_denied",
        /// 403: a request to the gateway listener did not come through the
        /// gateway.
        UntrustedClient => "untrusted_client",
        /// 503: the tailnet peer could not be identified in time.
        TailnetUnavailable => "tailnet_unavailable",
        /// 401: a tailnet-bound token was used from elsewhere.
        TailnetBindingMismatch => "tailnet_binding_mismatch",
        /// 403: the caller lacks the permission named in
        /// `details.permission`.
        PermissionDenied => "permission_denied",
        /// 403: the account or grants include access the caller does not
        /// hold.
        PermissionExceeded => "permission_exceeded",
        /// 403: a credential with limited access, like an API token, tried to
        /// create a credential or change its account.
        CredentialScoped => "credential_scoped",
        /// 403: the action needs a recent passkey sign-in.
        ReauthenticationRequired => "reauthentication_required",

        /// 400: a change names neither a revision nor `force=true`.
        PreconditionRequired => "precondition_required",
        /// 409: the application changed since the caller read its
        /// generation; `details` has the expected and actual ones.
        GenerationConflict => "generation_conflict",
        /// 409: the application was replaced since the caller read it.
        IdentityConflict => "identity_conflict",
        /// 409: a repository-backed application can't be renamed while
        /// connected.
        RepositoryManaged => "repository_managed",
        /// 409: the application is busy with another operation.
        ApplicationBusy => "application_busy",
        /// 409: an application, environment or preview of that identity or
        /// name already exists.
        ApplicationNameCollision => "application_name_collision",
        /// 409: the application can't make the requested transition.
        ApplicationStateConflict => "application_state_conflict",
        /// 410: the requested event history was pruned.
        HistoryExpired => "history_expired",
        /// 409: the application does not have exactly one environment, so
        /// the request must name one; `details.environments` lists them.
        EnvironmentRequired => "environment_required",
        /// 409: deleting the application needs every environment named;
        /// `details.environments` lists them.
        EnvironmentConfirmationRequired => "environment_confirmation_required",
        /// 409: a sibling environment's configuration still names the
        /// environment being renamed.
        EnvironmentConfigured => "environment_configured",
        /// 409: the environment is promoted, so it doesn't build or follow a
        /// branch.
        EnvironmentPromoted => "environment_promoted",
        /// 409: the environment builds its own source, so it can't receive a
        /// promotion.
        EnvironmentNotPromoted => "environment_not_promoted",
        /// 409: other environments promote from this one;
        /// `details.environments` lists them.
        PromotionSourceInUse => "promotion_source_in_use",
        /// 409: the promotion source is not deployed and healthy.
        PromotionSourceNotReady => "promotion_source_not_ready",
        /// 409: the promotion source deployed again since it was checked.
        PromotionSourceChanged => "promotion_source_changed",
        /// 409: secrets a promotion needs are missing in the target;
        /// `details.secrets` lists them.
        SecretsUnavailable => "secrets_unavailable",
        /// 409: the deployment never succeeded, so it can't be restored.
        DeploymentNotRestorable => "deployment_not_restorable",
        /// 409: secret versions a restore needs are gone;
        /// `details.secrets` lists them.
        RestoreSecretsUnavailable => "restore_secrets_unavailable",
        /// 409: the hostname is reserved by another environment, another
        /// application's wildcard, or this installation.
        HostnameConflict => "hostname_conflict",
        /// 404: a field edit names an entry that does not exist.
        FieldResourceNotFound => "field_resource_not_found",
        /// 409: a field edit adds an entry that already exists.
        FieldResourceExists => "field_resource_exists",
        /// 409: a field edit does not fit the current configuration.
        FieldIncompatible => "field_incompatible",
        /// 422: the manifest failed validation; `details.errors` lists each
        /// error with its own code.
        ManifestValidationFailed => "manifest_validation_failed",
        /// 500: the saved configuration could not be rendered.
        ManifestSerializationFailed => "manifest_serialization_failed",
        /// 409: the runtime plan has blocking conflicts;
        /// `details.diagnostics` lists them.
        PlanBlocked => "plan_blocked",
        /// 502: the manifest repository could not be read.
        RepositoryUnavailable => "repository_unavailable",
        /// 409: the service has no running task.
        ServiceNotRunning => "service_not_running",
        /// 500: a local snapshot upload failed in the daemon.
        SnapshotUnavailable => "snapshot_unavailable",

        /// 409: the secret changed since the caller read it; `details` has
        /// the expected and actual generations.
        SecretGenerationConflict => "secret_generation_conflict",
        /// 409: stored secret values are no longer readable; supply new ones
        /// and deploy again.
        SecretUnavailable => "secret_unavailable",
        /// 409: secrets the configuration mounts are not set;
        /// `details.names` lists them.
        SecretMissing => "secret_missing",
        /// 409: the secret master key still works, so recovery is refused.
        SecretKeyUsable => "secret_key_usable",
        /// 503: secret storage is unavailable.
        SecretStorageUnavailable => "secret_storage_unavailable",
        /// 409: the secret is being deleted.
        SecretDeleting => "secret_deleting",
        /// 409: the secret's access changed since the caller read it.
        SecretAccessConflict => "secret_access_conflict",
        /// 409: the secret store is over its quota.
        SecretQuotaExceeded => "secret_quota_exceeded",
        /// 409: configuration or a deployment still references the secret.
        SecretReferenced => "secret_referenced",
        /// 409: the secret's access list does not include the environment.
        SecretAccessDenied => "secret_access_denied",
        /// 400: a secret change names no expected generation.
        SecretGenerationRequired => "secret_generation_required",
        /// 415: a secret value is not `application/octet-stream`.
        SecretContentType => "secret_content_type",
        /// 400: the secret body could not be read within the request limit.
        SecretBodyInvalid => "secret_body_invalid",
        /// 400: the secret value is empty or too large.
        SecretValueInvalid => "secret_value_invalid",

        /// 400: a notification destination needs a URL.
        DestinationUrlRequired => "destination_url_required",
        /// 400: a notification destination's URL is invalid.
        DestinationUrlInvalid => "destination_url_invalid",
        /// 400: a notification destination names an unknown application.
        DestinationApplicationUnknown => "destination_application_unknown",
        /// 409: the delivery can't be retried.
        DeliveryNotRetryable => "delivery_not_retryable",

        /// 503: the effective host configuration is unavailable.
        ConfigurationUnavailable => "configuration_unavailable",
        /// 503: too many requests like this one are running.
        ServerBusy => "server_busy",
        /// 404: no build machine is configured.
        BuilderNotConfigured => "builder_not_configured",
        /// 503: Docker Engine is unavailable.
        DockerUnavailable => "docker_unavailable",
        /// 502: a runtime request failed.
        RuntimeRequestFailed => "runtime_request_failed",
        /// 502: a Git source build failed.
        GitBuildFailed => "git_build_failed",
        /// 500: the application could not be compiled.
        ApplicationCompilationFailed => "application_compilation_failed",
        /// 503: control-plane storage is unavailable.
        StorageUnavailable => "storage_unavailable",
        /// 503: the database schema is incompatible.
        SchemaMismatch => "schema_mismatch",
        /// 500: stored application state is corrupt.
        StoredStateCorrupt => "stored_state_corrupt",
        /// Never sent by the daemon: the client reports it when an error
        /// response is not a readable `ErrorBody`.
        InvalidErrorResponse => "invalid_error_response",

        /// A desired service was not found in the runtime.
        ServiceMissing => "service_missing",
        /// Not every replica of a service is healthy.
        ServiceNotConverged => "service_not_converged",
        /// A desired task exited unsuccessfully.
        TaskFailed => "task_failed",
        /// The runtime rejected a desired task before it started.
        TaskRejected => "task_rejected",
        /// The environment's status message.
        ApplicationStatus => "application_status",
        /// A fetched manifest names another repository connection, which is
        /// ignored.
        ManifestConnectionIgnored => "manifest_connection_ignored",

        /// Manifest does not decode against the strict public schema.
        ManifestDecodeFailed => "manifest_decode_failed",
        /// Manifest `api_version` is not supported.
        ApiVersionUnsupported => "api_version_unsupported",
        /// Manifest `kind` is not supported.
        KindUnsupported => "kind_unsupported",
        /// Logical name violates the bounded naming rule.
        NameInvalid => "name_invalid",
        /// Application declares no service.
        ServiceRequired => "service_required",
        /// Service logical name is duplicated.
        ServiceNameDuplicate => "service_name_duplicate",
        /// Volume logical name is duplicated.
        VolumeNameDuplicate => "volume_name_duplicate",
        /// Service dependency does not name a service in the application.
        ServiceDependencyMissing => "service_dependency_missing",
        /// Service dependency is listed more than once.
        ServiceDependencyDuplicate => "service_dependency_duplicate",
        /// Service lists more dependencies than an application can have services.
        ServiceDependencyCountExcessive => "service_dependency_count_excessive",
        /// Service dependencies form or lead into a cycle.
        ServiceDependencyCycle => "service_dependency_cycle",
        /// Rollout monitor window is outside its supported range.
        RolloutMonitorInvalid => "rollout_monitor_invalid",
        /// An explicit start-first order lets two tasks share a writable volume.
        RolloutStartFirstWritableVolume => "rollout_start_first_writable_volume",
        /// A routed service declares no health check in an environment that rolls
        /// back failed rollouts automatically, so unless its image defines one, a
        /// rollout that doesn't serve isn't detected as failed.
        RollbackWithoutHealthCheck => "rollback_without_health_check",
        /// Replica count is outside its supported range.
        ReplicasOutOfRange => "replicas_out_of_range",
        /// Image reference is not a safe registry reference.
        ImageInvalid => "image_invalid",
        /// Git repository URL embeds credentials.
        GitCredentialsInline => "git_credentials_inline",
        /// Git repository URL is empty, oversized, or option-like.
        GitRepositoryInvalid => "git_repository_invalid",
        /// Git branch is not a valid branch name; also 400 for a request's
        /// branch.
        GitBranchInvalid => "git_branch_invalid",
        /// Git commit is not a full lowercase hexadecimal hash; also 400 for
        /// a request's pinned commit.
        GitCommitInvalid => "git_commit_invalid",
        /// Repository path is not relative or leaves the repository.
        RepositoryPathInvalid => "repository_path_invalid",
        /// Repository poll interval is outside its supported range.
        SyncIntervalInvalid => "sync_interval_invalid",
        /// Environment name violates its alphabet rule; also 400 for a
        /// request's environment name.
        EnvironmentNameInvalid => "environment_name_invalid",
        /// Environment value contains forbidden bytes.
        EnvironmentValueInvalid => "environment_value_invalid",
        /// Environment map exceeds its entry budget.
        EnvironmentCountExcessive => "environment_count_excessive",
        /// Environment value exceeds its byte budget.
        EnvironmentValueExcessive => "environment_value_excessive",
        /// Docker build argument name violates its alphabet rule.
        BuildArgNameInvalid => "build_arg_name_invalid",
        /// Docker build argument value contains forbidden bytes.
        BuildArgValueInvalid => "build_arg_value_invalid",
        /// Docker build argument map exceeds its entry budget.
        BuildArgCountExcessive => "build_arg_count_excessive",
        /// Docker build argument value exceeds its byte budget.
        BuildArgValueExcessive => "build_arg_value_excessive",
        /// Docker build arguments together exceed their byte budget.
        BuildArgTotalExcessive => "build_arg_total_excessive",
        /// Docker build target is not a valid stage name.
        BuildTargetInvalid => "build_target_invalid",
        /// Mount references an undeclared volume.
        MountVolumeMissing => "mount_volume_missing",
        /// Mount target is not a normalized absolute container path.
        MountTargetUnsafe => "mount_target_unsafe",
        /// Mount target is duplicated within one service.
        MountTargetDuplicate => "mount_target_duplicate",
        /// Service declares more mounts than allowed.
        MountCountExcessive => "mount_count_excessive",
        /// Resource limits configure neither CPU nor memory.
        ResourceLimitsEmpty => "resource_limits_empty",
        /// CPU limit is zero.
        CpuLimitInvalid => "cpu_limit_invalid",
        /// CPU limit exceeds its explicit budget.
        CpuLimitExcessive => "cpu_limit_excessive",
        /// Memory limit is zero or does not fit the runtime value.
        MemoryLimitInvalid => "memory_limit_invalid",
        /// Health-check port is outside 1-65535.
        PortInvalid => "port_invalid",
        /// HTTP health-check path is unsafe.
        HealthcheckPathInvalid => "healthcheck_path_invalid",
        /// Health-check command has no usable executable or contains NUL.
        HealthcheckCommandInvalid => "healthcheck_command_invalid",
        /// Health-check interval is not positive.
        HealthcheckIntervalInvalid => "healthcheck_interval_invalid",
        /// Health-check interval exceeds its explicit budget.
        HealthcheckIntervalExcessive => "healthcheck_interval_excessive",
        /// Health-check timeout is not positive or exceeds its interval.
        HealthcheckTimeoutInvalid => "healthcheck_timeout_invalid",
        /// Health-check start period exceeds its explicit budget.
        HealthcheckStartPeriodExcessive => "healthcheck_start_period_excessive",
        /// Health-check retries are not positive or exceed their explicit budget.
        HealthcheckRetriesInvalid => "healthcheck_retries_invalid",
        /// Explicit container command does not start with a non-empty executable.
        ProcessCommandInvalid => "process_command_invalid",
        /// Container command exceeds its element or byte budget.
        ProcessCommandExcessive => "process_command_excessive",
        /// Container arguments exceed their element or byte budget.
        ProcessArgumentsExcessive => "process_arguments_excessive",
        /// Container process argument contains NUL.
        ProcessArgumentInvalid => "process_argument_invalid",
        /// Application declares more services than allowed.
        ServiceCountExcessive => "service_count_excessive",
        /// Application declares more volumes than allowed.
        VolumeCountExcessive => "volume_count_excessive",
        /// Application declares more jobs than allowed.
        JobCountExcessive => "job_count_excessive",
        /// The daemon could not observe an environment's runtime; a status
        /// diagnostic.
        RuntimeUnavailable => "runtime_unavailable",
        /// Application declares more routes than allowed.
        RoutesLimit => "routes_limit",
        /// Route name is used by another route of the application.
        RouteNameDuplicate => "route_name_duplicate",
        /// Route hostname is not an exact public hostname or a wildcard of one.
        RouteHostnameInvalid => "route_hostname_invalid",
        /// Route hostname is used by another route of the application.
        RouteHostnameDuplicate => "route_hostname_duplicate",
        /// Route sets neither, or both, a service and port and a redirect.
        RouteTargetInvalid => "route_target_invalid",
        /// Route references an undeclared service.
        RouteServiceMissing => "route_service_missing",
        /// Route backend port is zero.
        RoutePortInvalid => "route_port_invalid",
        /// Redirect destination is not an absolute public `http(s)` URL.
        RouteRedirectInvalid => "route_redirect_invalid",
        /// Redirect status is not a supported redirect code.
        RouteRedirectStatusInvalid => "route_redirect_status_invalid",
        /// Redirect destination is a hostname the route serves itself.
        RouteRedirectLoop => "route_redirect_loop",
        /// Application declares more generated secrets, or a service more secret
        /// mounts, than allowed.
        SecretsExcessive => "secrets_excessive",
        /// Secret target is not a unique file path under `/run/secrets`.
        SecretTargetInvalid => "secret_target_invalid",
        /// Generated secret name is duplicated.
        SecretNameDuplicate => "secret_name_duplicate",
        /// Generated secret uses an unsupported byte count or key size.
        SecretGeneratorInvalid => "secret_generator_invalid",
        /// Job logical name is duplicated.
        JobNameDuplicate => "job_name_duplicate",
        /// Job references an undeclared service.
        JobServiceMissing => "job_service_missing",
        /// A job would start a dependency before that dependency's own jobs succeeded.
        JobDependencyOrderInvalid => "job_dependency_order_invalid",
        /// Job timeout is outside its supported range.
        JobTimeoutInvalid => "job_timeout_invalid",
        /// An exclusive job has no writable volume, or a startup dependency mounts one.
        JobVolumeAccessInvalid => "job_volume_access_invalid",
        /// A job sets an environment variable or mounts at a target its service already does.
        JobSettingConflict => "job_setting_conflict",
        /// A job stops its own service, or an exclusive job those sharing its volumes,
        /// until rollout.
        JobStopsServices => "job_stops_services",

        /// Template text has a `${{` without a well-formed `namespace.name` reference.
        TemplateInvalid => "template_invalid",
        /// Reference names a namespace that is reserved for a later feature.
        VariableReserved => "variable_reserved",
        /// Reference names an unknown namespace.
        VariableNamespaceUnknown => "variable_namespace_unknown",
        /// Reference names an unknown system variable.
        VariableUnknown => "variable_unknown",
        /// Reference names a variable declared nowhere in the manifest.
        VariableUndeclared => "variable_undeclared",
        /// Reference appears where values cannot reference variables.
        VariableNotAllowed => "variable_not_allowed",
        /// Reference names a system variable this deployment does not have.
        VariableUnavailable => "variable_unavailable",
        /// Declared variable has no value for the environment being rendered.
        VariableValueMissing => "variable_value_missing",
        /// Rendered value does not have the field's type.
        VariableTypeInvalid => "variable_type_invalid",
        /// Variable name violates its alphabet rule.
        VariableNameInvalid => "variable_name_invalid",
        /// Manifest declares more variables or environment blocks than allowed.
        VariableCountExcessive => "variable_count_excessive",
        /// Variable value exceeds its byte budget.
        VariableValueExcessive => "variable_value_excessive",
        /// 409: previews need an application whose manifest comes from a
        /// repository.
        PreviewRequiresRepository => "preview_requires_repository",
        /// 409: creating a preview would exceed a `previews` count limit;
        /// `details` is a `PreviewLimitReached`.
        PreviewLimitReached => "preview_limit_reached",
        /// 404: no daemon setting has this key.
        SettingUnknown => "setting_unknown",
        /// 409: the setting doesn't apply to this installation, which lacks
        /// what it needs, such as a build machine.
        SettingUnavailable => "setting_unavailable",
        /// 422: the value is not one the setting takes; the message says
        /// which it does.
        SettingInvalid => "setting_invalid",
        /// A preview service runs with a `previews` default limit it does not set.
        PreviewLimitsDefaulted => "preview_limits_defaulted",
        /// A preview service asks for more replicas than `previews.max_replicas`.
        PreviewReplicasCapped => "preview_replicas_capped",
        /// 413: a local snapshot bundle is larger than
        /// the `snapshots.max_upload_bytes` setting, or its tree than
        /// `[checkouts] max_bytes`.
        SnapshotTooLarge => "snapshot_too_large",
        /// 422: a local snapshot upload is not a valid Git bundle of one
        /// snapshot commit.
        SnapshotInvalid => "snapshot_invalid",
        /// 422: the base commit a thin snapshot bundle needs cannot be fetched
        /// from the application's repository; upload a complete bundle
        /// instead.
        SnapshotBaseUnavailable => "snapshot_base_unavailable",
        /// 408: a local snapshot upload stalled, or took too long to receive.
        SnapshotTimedOut => "snapshot_timed_out",
        /// Only environments of a repository-backed application follow a branch.
        ManifestRepositoryRequired => "manifest_repository_required",
        /// A promoted environment also sets a branch, commit, or sync.
        EnvironmentSourceConflict => "environment_source_conflict",
        /// An environment pinned to a commit cannot sync.
        EnvironmentSyncPinned => "environment_sync_pinned",
        /// A promotion source is not an environment that can be one; 422
        /// when a request names it.
        PromotionSourceInvalid => "promotion_source_invalid",
        /// An environment would, directly or through others, promote from
        /// itself; 422 when a request would make it.
        PromotionCycle => "promotion_cycle",
        /// The manifest declares the environment, so it owns its source and
        /// existence; 409 when a request changes either.
        EnvironmentDeclared => "environment_declared",
        /// An edit would declare an environment that doesn't exist; 422 from
        /// the edit.
        EnvironmentUnknown => "environment_unknown",
        /// Manifest that must be literal still references variables.
        VariableUnresolved => "variable_unresolved",
        /// A declared secret is also set in the application's secret store.
        SecretNameConflict => "secret_name_conflict",
        /// A file's target is a mount or secret target, another file's, or reserved.
        FileTargetInvalid => "file_target_invalid",
        /// A file reads the manifest's repository, but the application has none.
        FileRequiresRepository => "file_requires_repository",
        /// Inline file content is empty, which Swarm configs cannot be.
        FileEmpty => "file_empty",
        /// Inline file content exceeds the per-file budget.
        FileTooLarge => "file_too_large",
        /// A service mounts more files than allowed.
        FileCountExcessive => "file_count_excessive",
        /// An application's files together exceed their byte budget.
        FileTotalExcessive => "file_total_excessive",

        /// Service source has no immutable resolution yet.
        SourceUnresolved => "source_unresolved",
        /// A mounted secret has no pinned Docker secret version yet.
        SecretUnresolved => "secret_unresolved",
        /// Resolved source does not immutably match the requested service source.
        SourceResolutionMismatch => "source_resolution_mismatch",
        /// A repository file a service mounts was not captured with its manifest.
        FileUnresolved => "file_unresolved",
        /// An application declares too many exports or imports, or an export
        /// grants too many applications.
        LinkCountExcessive => "link_count_excessive",
        /// An export references a service the application does not declare.
        ExportServiceMissing => "export_service_missing",
        /// A service is exported, or an application granted, more than once.
        ExportDuplicate => "export_duplicate",
        /// An export grants no application.
        ExportApplicationsEmpty => "export_applications_empty",
        /// A service of another application is imported more than once.
        ImportDuplicate => "import_duplicate",
        /// An import does not connect: its environment is missing, does not export
        /// the service to the importer, or already links to the most importers.
        ImportUnconnected => "import_unconnected",
        /// More than one service of an application receives telemetry.
        TelemetryReceiverDuplicate => "telemetry_receiver_duplicate",
        /// A service receiving telemetry runs more than one replica.
        TelemetryReceiverReplicas => "telemetry_receiver_replicas",
        /// A service receiving telemetry sets its user, though it runs as the
        /// daemon's.
        TelemetryReceiverUser => "telemetry_receiver_user",
        /// A service with a read-only root mounts a volume or file at `/tmp`,
        /// where its writable in-memory directory goes.
        ReadOnlyRootTmpMount => "read_only_root_tmp_mount",
        /// A service receives telemetry, but its environment is not granted it, so
        /// it runs without the telemetry socket.
        TelemetryNotGranted => "telemetry_not_granted",
        /// A release's manifest renders a build input differently for another
        /// environment than for the release, so its images do not apply there.
        ReleaseIncompatible => "release_incompatible",

        /// Same-name runtime resource is not owned by this application.
        UnownedNameCollision => "unowned_name_collision",
        /// Immutable runtime configuration differs from desired state.
        ImmutableConfigurationDrift => "immutable_configuration_drift",
        /// Foreign or unowned resource is skipped by the plan.
        ForeignResourceIgnored => "foreign_resource_ignored",
        /// An owned service update failed in the runtime.
        ServiceUpdateFailed => "service_update_failed",
        /// Obsolete owned resources await removal until earlier changes converge.
        CleanupDeferred => "cleanup_deferred",
        /// A release image is missing and can't be pulled again, so the release
        /// can't be deployed: a blocking plan diagnostic, or 409 from the
        /// deployment.
        ImageUnavailable => "image_unavailable",
    }
}

compared_as_string!(ApiErrorCode);

#[cfg(test)]
mod tests {
    use super::ApiErrorCode;
    use std::collections::BTreeSet;

    /// Each code is sent as its own `snake_case` string and read back as
    /// itself, and no two share one.
    #[test]
    fn codes_serialize_to_distinct_snake_case_strings() {
        for code in ApiErrorCode::ALL {
            let value = code.as_str();
            assert!(
                value
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_'),
                "{value}"
            );
            assert_eq!(
                serde_json::to_value(code).expect("the value serializes"),
                value
            );
            assert_eq!(
                serde_json::from_value::<ApiErrorCode>(value.into())
                    .expect("the api error code deserializes"),
                code
            );
            assert_eq!(ApiErrorCode::parse(value), Some(code));
        }
        let values = ApiErrorCode::ALL.map(ApiErrorCode::as_str);
        assert_eq!(BTreeSet::from(values).len(), values.len());
    }
}
