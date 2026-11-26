// Generated from the OpenAPI document. Run `just generate`; do not edit.

use progenitor_client::encode_path;

/// `POST /api/v1/applications/{id}/services`
#[must_use]
pub fn add_application_service(id: &str) -> String {
    format!("/api/v1/applications/{id}/services", id = encode_path(id))
}

/// `POST /api/v1/applications/{id}/volumes`
#[must_use]
pub fn add_application_volume(id: &str) -> String {
    format!("/api/v1/applications/{id}/volumes", id = encode_path(id))
}

/// `GET /api/v1/applications/{id}/secrets`
#[must_use]
pub fn application_secrets(id: &str) -> String {
    format!("/api/v1/applications/{id}/secrets", id = encode_path(id))
}

/// `POST /api/v1/applications/apply`
#[must_use]
pub fn apply_application() -> String {
    String::from("/api/v1/applications/apply")
}

/// `POST /api/v1/auth/device/approve`
#[must_use]
pub fn auth_device_approve() -> String {
    String::from("/api/v1/auth/device/approve")
}

/// `POST /api/v1/auth/device/confirm`
#[must_use]
pub fn auth_device_confirm() -> String {
    String::from("/api/v1/auth/device/confirm")
}

/// `POST /api/v1/auth/device/inspect`
#[must_use]
pub fn auth_device_inspect() -> String {
    String::from("/api/v1/auth/device/inspect")
}

/// `POST /api/v1/auth/device/poll`
#[must_use]
pub fn auth_device_poll() -> String {
    String::from("/api/v1/auth/device/poll")
}

/// `POST /api/v1/auth/device/start`
#[must_use]
pub fn auth_device_start() -> String {
    String::from("/api/v1/auth/device/start")
}

/// `GET /api/v1/auth/directory`
#[must_use]
pub fn auth_directory() -> String {
    String::from("/api/v1/auth/directory")
}

/// `POST /api/v1/auth/login/finish`
#[must_use]
pub fn auth_login_finish() -> String {
    String::from("/api/v1/auth/login/finish")
}

/// `POST /api/v1/auth/login/start`
#[must_use]
pub fn auth_login_start() -> String {
    String::from("/api/v1/auth/login/start")
}

/// `POST /api/v1/auth/logout`
#[must_use]
pub fn auth_logout() -> String {
    String::from("/api/v1/auth/logout")
}

/// `POST /api/v1/auth/manage`
#[must_use]
pub fn auth_manage() -> String {
    String::from("/api/v1/auth/manage")
}

/// `GET /api/v1/auth/me`
#[must_use]
pub fn auth_me() -> String {
    String::from("/api/v1/auth/me")
}

/// `POST /api/v1/auth/login/operator`
#[must_use]
pub fn auth_operator_sign_in() -> String {
    String::from("/api/v1/auth/login/operator")
}

/// `POST /api/v1/auth/reauthenticate/finish`
#[must_use]
pub fn auth_reauthenticate_finish() -> String {
    String::from("/api/v1/auth/reauthenticate/finish")
}

/// `POST /api/v1/auth/reauthenticate/start`
#[must_use]
pub fn auth_reauthenticate_start() -> String {
    String::from("/api/v1/auth/reauthenticate/start")
}

/// `POST /api/v1/auth/recovery`
#[must_use]
pub fn auth_recover_admin() -> String {
    String::from("/api/v1/auth/recovery")
}

/// `POST /api/v1/auth/register/finish`
#[must_use]
pub fn auth_registration_finish() -> String {
    String::from("/api/v1/auth/register/finish")
}

/// `POST /api/v1/auth/register/start`
#[must_use]
pub fn auth_registration_start() -> String {
    String::from("/api/v1/auth/register/start")
}

/// `GET /api/v1/auth/setup-link`
#[must_use]
pub fn auth_setup_link() -> String {
    String::from("/api/v1/auth/setup-link")
}

/// `POST /api/v1/auth/sign-in-link`
#[must_use]
pub fn auth_sign_in_link() -> String {
    String::from("/api/v1/auth/sign-in-link")
}

/// `GET /api/v1/auth/status`
#[must_use]
pub fn auth_status() -> String {
    String::from("/api/v1/auth/status")
}

/// `GET /api/v1/builds/{id}/logs`
#[must_use]
pub fn build_logs(id: i64) -> String {
    format!("/api/v1/builds/{id}/logs")
}

/// `POST /api/v1/system/builder/check`
#[must_use]
pub fn check_builder() -> String {
    String::from("/api/v1/system/builder/check")
}

/// `POST /api/v1/applications`
#[must_use]
pub fn create_application() -> String {
    String::from("/api/v1/applications")
}

/// `POST /api/v1/applications/{id}/environments`
#[must_use]
pub fn create_environment(id: &str) -> String {
    format!(
        "/api/v1/applications/{id}/environments",
        id = encode_path(id)
    )
}

/// `POST /api/v1/applications/{id}/previews`
#[must_use]
pub fn create_preview(id: &str) -> String {
    format!("/api/v1/applications/{id}/previews", id = encode_path(id))
}

/// `DELETE /api/v1/applications/{id}`
#[must_use]
pub fn delete_application(id: &str) -> String {
    format!("/api/v1/applications/{id}", id = encode_path(id))
}

/// `DELETE /api/v1/applications/{id}/secrets/{name}`
#[must_use]
pub fn delete_application_secret(id: &str, name: &str) -> String {
    format!(
        "/api/v1/applications/{id}/secrets/{name}",
        id = encode_path(id),
        name = encode_path(name)
    )
}

/// `DELETE /api/v1/environments/{id}`
#[must_use]
pub fn delete_environment(id: &str) -> String {
    format!("/api/v1/environments/{id}", id = encode_path(id))
}

/// `DELETE /api/v1/environments/{id}/secrets/{name}`
#[must_use]
pub fn delete_environment_secret(id: &str, name: &str) -> String {
    format!(
        "/api/v1/environments/{id}/secrets/{name}",
        id = encode_path(id),
        name = encode_path(name)
    )
}

/// `DELETE /api/v1/notifications/destinations/{name}`
#[must_use]
pub fn delete_notification_destination(name: &str) -> String {
    format!(
        "/api/v1/notifications/destinations/{name}",
        name = encode_path(name)
    )
}

/// `DELETE /api/v1/previews/{id}`
#[must_use]
pub fn delete_preview(id: &str) -> String {
    format!("/api/v1/previews/{id}", id = encode_path(id))
}

/// `POST /api/v1/environments/{id}/deploy`
#[must_use]
pub fn deploy_environment(id: &str) -> String {
    format!("/api/v1/environments/{id}/deploy", id = encode_path(id))
}

/// `POST /api/v1/applications/{id}/previews/local`
#[must_use]
pub fn deploy_local_preview(id: &str) -> String {
    format!(
        "/api/v1/applications/{id}/previews/local",
        id = encode_path(id)
    )
}

/// `POST /api/v1/previews/{id}/deploy`
#[must_use]
pub fn deploy_preview(id: &str) -> String {
    format!("/api/v1/previews/{id}/deploy", id = encode_path(id))
}

/// `GET /api/v1/analytics/deployments`
#[must_use]
pub fn deployment_analytics() -> String {
    String::from("/api/v1/analytics/deployments")
}

/// `DELETE /api/v1/applications/{id}/repository`
#[must_use]
pub fn disconnect_manifest_repository(id: &str) -> String {
    format!("/api/v1/applications/{id}/repository", id = encode_path(id))
}

/// `GET /api/v1/applications/{id}/manifest`
#[must_use]
pub fn download_application_manifest(id: &str) -> String {
    format!("/api/v1/applications/{id}/manifest", id = encode_path(id))
}

/// `GET /api/v1/environments/{id}/logs`
#[must_use]
pub fn environment_logs(id: &str) -> String {
    format!("/api/v1/environments/{id}/logs", id = encode_path(id))
}

/// `GET /api/v1/environments/{id}/secrets`
#[must_use]
pub fn environment_secrets(id: &str) -> String {
    format!("/api/v1/environments/{id}/secrets", id = encode_path(id))
}

/// `GET /api/v1/environments/{id}/status`
#[must_use]
pub fn environment_status(id: &str) -> String {
    format!("/api/v1/environments/{id}/status", id = encode_path(id))
}

/// `GET /api/v1/environments/{id}/traffic`
#[must_use]
pub fn environment_traffic(id: &str) -> String {
    format!("/api/v1/environments/{id}/traffic", id = encode_path(id))
}

/// `GET /api/v1/environments/{id}/exec`
#[must_use]
pub fn exec_environment_command(id: &str) -> String {
    format!("/api/v1/environments/{id}/exec", id = encode_path(id))
}

/// `POST /api/v1/applications/{id}/webhook/secret`
#[must_use]
pub fn generate_webhook_secret(id: &str) -> String {
    format!(
        "/api/v1/applications/{id}/webhook/secret",
        id = encode_path(id)
    )
}

/// `GET /api/v1/applications/{id}`
#[must_use]
pub fn get_application(id: &str) -> String {
    format!("/api/v1/applications/{id}", id = encode_path(id))
}

/// `GET /api/v1/environments/{id}/deployments/{deployment}`
#[must_use]
pub fn get_deployment(id: &str, deployment: &str) -> String {
    format!(
        "/api/v1/environments/{id}/deployments/{deployment}",
        id = encode_path(id),
        deployment = encode_path(deployment)
    )
}

/// `GET /api/v1/diagnostics/{id}`
#[must_use]
pub fn get_diagnostic(id: &str) -> String {
    format!("/api/v1/diagnostics/{id}", id = encode_path(id))
}

/// `GET /api/v1/environments/{id}`
#[must_use]
pub fn get_environment(id: &str) -> String {
    format!("/api/v1/environments/{id}", id = encode_path(id))
}

/// `GET /api/v1/environments/{id}/detail`
#[must_use]
pub fn get_environment_detail(id: &str) -> String {
    format!("/api/v1/environments/{id}/detail", id = encode_path(id))
}

/// `GET /api/v1/operations/{id}`
#[must_use]
pub fn get_operation(id: &str) -> String {
    format!("/api/v1/operations/{id}", id = encode_path(id))
}

/// `GET /api/v1/operations/{id}/timeline`
#[must_use]
pub fn get_operation_timeline(id: &str) -> String {
    format!("/api/v1/operations/{id}/timeline", id = encode_path(id))
}

/// `GET /api/v1/previews/{id}`
#[must_use]
pub fn get_preview(id: &str) -> String {
    format!("/api/v1/previews/{id}", id = encode_path(id))
}

/// `GET /api/v1/applications/{id}/webhook`
#[must_use]
pub fn get_webhook(id: &str) -> String {
    format!("/api/v1/applications/{id}/webhook", id = encode_path(id))
}

/// `GET /api/v1/applications`
#[must_use]
pub fn list_applications() -> String {
    String::from("/api/v1/applications")
}

/// `GET /api/v1/audit`
#[must_use]
pub fn list_audit() -> String {
    String::from("/api/v1/audit")
}

/// `GET /api/v1/builds`
#[must_use]
pub fn list_builds() -> String {
    String::from("/api/v1/builds")
}

/// `GET /api/v1/environments/{id}/deployments/{deployment}/attempts`
#[must_use]
pub fn list_deployment_attempts(id: &str, deployment: &str) -> String {
    format!(
        "/api/v1/environments/{id}/deployments/{deployment}/attempts",
        id = encode_path(id),
        deployment = encode_path(deployment)
    )
}

/// `GET /api/v1/environments/{id}/deployments`
#[must_use]
pub fn list_deployments(id: &str) -> String {
    format!(
        "/api/v1/environments/{id}/deployments",
        id = encode_path(id)
    )
}

/// `GET /api/v1/events/kinds`
#[must_use]
pub fn list_event_kinds() -> String {
    String::from("/api/v1/events/kinds")
}

/// `GET /api/v1/events`
#[must_use]
pub fn list_events() -> String {
    String::from("/api/v1/events")
}

/// `GET /api/v1/applications/{id}/previews`
#[must_use]
pub fn list_previews(id: &str) -> String {
    format!("/api/v1/applications/{id}/previews", id = encode_path(id))
}

/// `GET /api/v1/applications/{id}/releases`
#[must_use]
pub fn list_releases(id: &str) -> String {
    format!("/api/v1/applications/{id}/releases", id = encode_path(id))
}

/// `GET /api/v1/notifications/deliveries`
#[must_use]
pub fn notification_deliveries() -> String {
    String::from("/api/v1/notifications/deliveries")
}

/// `GET /api/v1/notifications/destinations`
#[must_use]
pub fn notification_destinations() -> String {
    String::from("/api/v1/notifications/destinations")
}

/// `GET /api/v1/openapi.json`
#[must_use]
pub fn open_api_document() -> String {
    String::from("/api/v1/openapi.json")
}

/// `POST /api/v1/applications/plan`
#[must_use]
pub fn plan_application() -> String {
    String::from("/api/v1/applications/plan")
}

/// `POST /api/v1/environments/{id}/promote/plan`
#[must_use]
pub fn plan_promotion(id: &str) -> String {
    format!(
        "/api/v1/environments/{id}/promote/plan",
        id = encode_path(id)
    )
}

/// `POST /api/v1/environments/{id}/promote`
#[must_use]
pub fn promote_environment(id: &str) -> String {
    format!("/api/v1/environments/{id}/promote", id = encode_path(id))
}

/// `POST /api/v1/applications/{id}/previews/prune`
#[must_use]
pub fn prune_previews(id: &str) -> String {
    format!(
        "/api/v1/applications/{id}/previews/prune",
        id = encode_path(id)
    )
}

/// `PUT /api/v1/applications/{id}/secrets/{name}`
#[must_use]
pub fn put_application_secret(id: &str, name: &str) -> String {
    format!(
        "/api/v1/applications/{id}/secrets/{name}",
        id = encode_path(id),
        name = encode_path(name)
    )
}

/// `PUT /api/v1/notifications/destinations/{name}`
#[must_use]
pub fn put_notification_destination(name: &str) -> String {
    format!(
        "/api/v1/notifications/destinations/{name}",
        name = encode_path(name)
    )
}

/// `PUT /api/v1/system/settings/{key}`
#[must_use]
pub fn put_setting(key: &str) -> String {
    format!("/api/v1/system/settings/{key}", key = encode_path(key))
}

/// `POST /api/v1/environments/{id}/reconcile`
#[must_use]
pub fn reconcile_environment(id: &str) -> String {
    format!("/api/v1/environments/{id}/reconcile", id = encode_path(id))
}

/// `POST /api/v1/system/secrets/recover-key`
#[must_use]
pub fn recover_secret_key() -> String {
    String::from("/api/v1/system/secrets/recover-key")
}

/// `POST /api/v1/system/dns/refresh`
#[must_use]
pub fn refresh_dns() -> String {
    String::from("/api/v1/system/dns/refresh")
}

/// `POST /api/v1/environments/{id}/secrets/{name}/regenerate`
#[must_use]
pub fn regenerate_environment_secret(id: &str, name: &str) -> String {
    format!(
        "/api/v1/environments/{id}/secrets/{name}/regenerate",
        id = encode_path(id),
        name = encode_path(name)
    )
}

/// `DELETE /api/v1/applications/{id}/services/{service}`
#[must_use]
pub fn remove_application_service(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `DELETE /api/v1/applications/{id}/volumes/{volume}`
#[must_use]
pub fn remove_application_volume(id: &str, volume: &str) -> String {
    format!(
        "/api/v1/applications/{id}/volumes/{volume}",
        id = encode_path(id),
        volume = encode_path(volume)
    )
}

/// `DELETE /api/v1/applications/{id}/services/{service}/environment/{key}`
#[must_use]
pub fn remove_service_environment_entry(id: &str, service: &str, key: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/environment/{key}",
        id = encode_path(id),
        service = encode_path(service),
        key = encode_path(key)
    )
}

/// `DELETE /api/v1/applications/{id}/services/{service}/file`
#[must_use]
pub fn remove_service_file(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/file",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `DELETE /api/v1/applications/{id}/services/{service}/mount`
#[must_use]
pub fn remove_service_mount(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/mount",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `POST /api/v1/environments/{id}/rename`
#[must_use]
pub fn rename_environment(id: &str) -> String {
    format!("/api/v1/environments/{id}/rename", id = encode_path(id))
}

/// `DELETE /api/v1/system/settings/{key}`
#[must_use]
pub fn reset_setting(key: &str) -> String {
    format!("/api/v1/system/settings/{key}", key = encode_path(key))
}

/// `POST /api/v1/environments/{id}/restore`
#[must_use]
pub fn restore_deployment(id: &str) -> String {
    format!("/api/v1/environments/{id}/restore", id = encode_path(id))
}

/// `POST /api/v1/previews/{id}/restore`
#[must_use]
pub fn restore_preview_deployment(id: &str) -> String {
    format!("/api/v1/previews/{id}/restore", id = encode_path(id))
}

/// `POST /api/v1/notifications/deliveries/{id}/retry`
#[must_use]
pub fn retry_notification_delivery(id: &str) -> String {
    format!(
        "/api/v1/notifications/deliveries/{id}/retry",
        id = encode_path(id)
    )
}

/// `PUT /api/v1/applications/{id}/exports`
#[must_use]
pub fn set_application_exports(id: &str) -> String {
    format!("/api/v1/applications/{id}/exports", id = encode_path(id))
}

/// `PUT /api/v1/applications/{id}/generated-secrets`
#[must_use]
pub fn set_application_generated_secrets(id: &str) -> String {
    format!(
        "/api/v1/applications/{id}/generated-secrets",
        id = encode_path(id)
    )
}

/// `PUT /api/v1/applications/{id}/imports`
#[must_use]
pub fn set_application_imports(id: &str) -> String {
    format!("/api/v1/applications/{id}/imports", id = encode_path(id))
}

/// `PUT /api/v1/applications/{id}/jobs`
#[must_use]
pub fn set_application_jobs(id: &str) -> String {
    format!("/api/v1/applications/{id}/jobs", id = encode_path(id))
}

/// `PUT /api/v1/applications/{id}/name`
#[must_use]
pub fn set_application_name(id: &str) -> String {
    format!("/api/v1/applications/{id}/name", id = encode_path(id))
}

/// `PUT /api/v1/applications/{id}/routes`
#[must_use]
pub fn set_application_routes(id: &str) -> String {
    format!("/api/v1/applications/{id}/routes", id = encode_path(id))
}

/// `PUT /api/v1/applications/{id}/variables`
#[must_use]
pub fn set_application_variables(id: &str) -> String {
    format!("/api/v1/applications/{id}/variables", id = encode_path(id))
}

/// `PUT /api/v1/applications/{id}/volumes`
#[must_use]
pub fn set_application_volumes(id: &str) -> String {
    format!("/api/v1/applications/{id}/volumes", id = encode_path(id))
}

/// `PUT /api/v1/environments/{id}/branch`
#[must_use]
pub fn set_environment_branch(id: &str) -> String {
    format!("/api/v1/environments/{id}/branch", id = encode_path(id))
}

/// `PUT /api/v1/applications/{id}/environments/{environment}/rollback`
#[must_use]
pub fn set_environment_rollback(id: &str, environment: &str) -> String {
    format!(
        "/api/v1/applications/{id}/environments/{environment}/rollback",
        id = encode_path(id),
        environment = encode_path(environment)
    )
}

/// `PUT /api/v1/environments/{id}/source`
#[must_use]
pub fn set_environment_source(id: &str) -> String {
    format!("/api/v1/environments/{id}/source", id = encode_path(id))
}

/// `PUT /api/v1/environments/{id}/sync`
#[must_use]
pub fn set_environment_sync(id: &str) -> String {
    format!("/api/v1/environments/{id}/sync", id = encode_path(id))
}

/// `PUT /api/v1/environments/{id}/telemetry`
#[must_use]
pub fn set_environment_telemetry(id: &str) -> String {
    format!("/api/v1/environments/{id}/telemetry", id = encode_path(id))
}

/// `PUT /api/v1/applications/{id}/environments/{environment}/visibility`
#[must_use]
pub fn set_environment_visibility(id: &str, environment: &str) -> String {
    format!(
        "/api/v1/applications/{id}/environments/{environment}/visibility",
        id = encode_path(id),
        environment = encode_path(environment)
    )
}

/// `PUT /api/v1/applications/{id}/repository`
#[must_use]
pub fn set_manifest_repository(id: &str) -> String {
    format!("/api/v1/applications/{id}/repository", id = encode_path(id))
}

/// `PUT /api/v1/applications/{id}/repository/path`
#[must_use]
pub fn set_manifest_repository_path(id: &str) -> String {
    format!(
        "/api/v1/applications/{id}/repository/path",
        id = encode_path(id)
    )
}

/// `PUT /api/v1/applications/{id}/repository/sync`
#[must_use]
pub fn set_manifest_repository_sync(id: &str) -> String {
    format!(
        "/api/v1/applications/{id}/repository/sync",
        id = encode_path(id)
    )
}

/// `PUT /api/v1/applications/{id}/repository/url`
#[must_use]
pub fn set_manifest_repository_url(id: &str) -> String {
    format!(
        "/api/v1/applications/{id}/repository/url",
        id = encode_path(id)
    )
}

/// `PUT /api/v1/applications/{id}/secrets/{name}/access`
#[must_use]
pub fn set_secret_access(id: &str, name: &str) -> String {
    format!(
        "/api/v1/applications/{id}/secrets/{name}/access",
        id = encode_path(id),
        name = encode_path(name)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/arguments`
#[must_use]
pub fn set_service_arguments(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/arguments",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/command`
#[must_use]
pub fn set_service_command(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/command",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/source/git/context`
#[must_use]
pub fn set_service_context(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/source/git/context",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/resources/cpu`
#[must_use]
pub fn set_service_cpu(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/resources/cpu",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/depends-on`
#[must_use]
pub fn set_service_depends_on(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/depends-on",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/source/git/dockerfile`
#[must_use]
pub fn set_service_dockerfile(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/source/git/dockerfile",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/environment`
#[must_use]
pub fn set_service_environment(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/environment",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/environment/{key}`
#[must_use]
pub fn set_service_environment_entry(id: &str, service: &str, key: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/environment/{key}",
        id = encode_path(id),
        service = encode_path(service),
        key = encode_path(key)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/file`
#[must_use]
pub fn set_service_file(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/file",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/files`
#[must_use]
pub fn set_service_files(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/files",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/general`
#[must_use]
pub fn set_service_general(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/general",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/source/git/branch`
#[must_use]
pub fn set_service_git_branch(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/source/git/branch",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/source/git/commit`
#[must_use]
pub fn set_service_git_commit(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/source/git/commit",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/source/git/url`
#[must_use]
pub fn set_service_git_url(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/source/git/url",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/healthcheck/command`
#[must_use]
pub fn set_service_health_command(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/healthcheck/command",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/healthcheck/interval`
#[must_use]
pub fn set_service_health_interval(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/healthcheck/interval",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/healthcheck/path`
#[must_use]
pub fn set_service_health_path(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/healthcheck/path",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/healthcheck/port`
#[must_use]
pub fn set_service_health_port(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/healthcheck/port",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/healthcheck/retries`
#[must_use]
pub fn set_service_health_retries(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/healthcheck/retries",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/healthcheck/start-period`
#[must_use]
pub fn set_service_health_start_period(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/healthcheck/start-period",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/healthcheck/timeout`
#[must_use]
pub fn set_service_health_timeout(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/healthcheck/timeout",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/healthcheck`
#[must_use]
pub fn set_service_healthcheck(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/healthcheck",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/source/image`
#[must_use]
pub fn set_service_image(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/source/image",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/resources/memory`
#[must_use]
pub fn set_service_memory(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/resources/memory",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/mount`
#[must_use]
pub fn set_service_mount(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/mount",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/mounts`
#[must_use]
pub fn set_service_mounts(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/mounts",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/name`
#[must_use]
pub fn set_service_name(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/name",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/process`
#[must_use]
pub fn set_service_process(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/process",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/receives-telemetry`
#[must_use]
pub fn set_service_receives_telemetry(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/receives-telemetry",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/replicas`
#[must_use]
pub fn set_service_replicas(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/replicas",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/resources`
#[must_use]
pub fn set_service_resources(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/resources",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/rollout`
#[must_use]
pub fn set_service_rollout(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/rollout",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/secrets`
#[must_use]
pub fn set_service_secrets(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/secrets",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/security`
#[must_use]
pub fn set_service_security(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/security",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `PUT /api/v1/applications/{id}/services/{service}/source`
#[must_use]
pub fn set_service_source(id: &str, service: &str) -> String {
    format!(
        "/api/v1/applications/{id}/services/{service}/source",
        id = encode_path(id),
        service = encode_path(service)
    )
}

/// `POST /api/v1/environments/{id}/start`
#[must_use]
pub fn start_environment(id: &str) -> String {
    format!("/api/v1/environments/{id}/start", id = encode_path(id))
}

/// `POST /api/v1/environments/{id}/stop`
#[must_use]
pub fn stop_environment(id: &str) -> String {
    format!("/api/v1/environments/{id}/stop", id = encode_path(id))
}

/// `GET /api/v1/environments/{id}/logs/stream`
#[must_use]
pub fn stream_environment_logs(id: &str) -> String {
    format!(
        "/api/v1/environments/{id}/logs/stream",
        id = encode_path(id)
    )
}

/// `GET /api/v1/events/stream`
#[must_use]
pub fn stream_events() -> String {
    String::from("/api/v1/events/stream")
}

/// `GET /api/v1/operations/{id}/timeline/stream`
#[must_use]
pub fn stream_operation_timeline(id: &str) -> String {
    format!(
        "/api/v1/operations/{id}/timeline/stream",
        id = encode_path(id)
    )
}

/// `GET /api/v1/system/checks`
#[must_use]
pub fn system_checks() -> String {
    String::from("/api/v1/system/checks")
}

/// `GET /api/v1/system/configuration`
#[must_use]
pub fn system_configuration() -> String {
    String::from("/api/v1/system/configuration")
}

/// `GET /api/v1/system/readiness`
#[must_use]
pub fn system_readiness() -> String {
    String::from("/api/v1/system/readiness")
}

/// `GET /api/v1/system/resources`
#[must_use]
pub fn system_resources() -> String {
    String::from("/api/v1/system/resources")
}

/// `GET /api/v1/system/status`
#[must_use]
pub fn system_status() -> String {
    String::from("/api/v1/system/status")
}

/// `POST /api/v1/notifications/destinations/{name}/test`
#[must_use]
pub fn test_notification_destination(name: &str) -> String {
    format!(
        "/api/v1/notifications/destinations/{name}/test",
        name = encode_path(name)
    )
}

/// `GET /api/v1/audit/verify`
#[must_use]
pub fn verify_audit() -> String {
    String::from("/api/v1/audit/verify")
}
