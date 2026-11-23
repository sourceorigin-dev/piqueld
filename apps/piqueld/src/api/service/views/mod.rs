//! Shared application and environment projections and bounded runtime diagnostics.
use piqueld_core::{
    ApiErrorCode, ObservedApplication,
    api::{DiagnosticView, EnvironmentStatusView, ObservedApplicationView, ObservedServiceView},
    resource::{Convergence, ObservedService, RuntimeMessage, TaskDiagnostic, TaskState},
};
use piqueld_store::StoredEnvironment;

/// Upper bound on diagnostics in one environment detail response.
const MAX_DETAIL_DIAGNOSTICS: usize = 24;
/// Upper bound on diagnostics reported for a single service.
const MAX_SERVICE_DIAGNOSTICS: usize = 8;

/// Joins each resolved (desired) service with its runtime observation.
///
/// A desired service missing from the runtime is reported as `Failed` with a
/// `service_missing` diagnostic only when `reconciled` is true (the application
/// is `Ready` and observation succeeded); otherwise it is still `Updating`.
pub(super) fn observed_view(
    stored: &StoredEnvironment,
    observed: &ObservedApplication,
    reconciled: bool,
) -> ObservedApplicationView {
    let services = stored
        .resolved
        .iter()
        .flat_map(|target| &target.services)
        .map(|desired| {
            let runtime = observed
                .services
                .iter()
                .find(|service| service.name == desired.name.as_str());
            let (image, observed_replicas, healthy_replicas, convergence, diagnostics) =
                match runtime {
                    Some(service) => (
                        Some(service.image.clone()),
                        service.replicas,
                        healthy_replicas(service),
                        service.convergence,
                        service_diagnostics(service),
                    ),
                    None if reconciled => (
                        None,
                        0,
                        0,
                        Convergence::Failed,
                        vec![DiagnosticView::new(
                            ApiErrorCode::ServiceMissing,
                            "the desired service was not found in the runtime observation",
                        )],
                    ),
                    None => (None, 0, 0, Convergence::Updating, Vec::new()),
                };
            ObservedServiceView {
                name: desired.logical_name.to_string(),
                image,
                desired_replicas: desired.replicas,
                observed_replicas,
                healthy_replicas,
                convergence,
                diagnostics,
            }
        })
        .collect();
    ObservedApplicationView {
        services,
        network_count: u32::try_from(observed.networks.len()).unwrap_or(u32::MAX),
        volume_count: u32::try_from(observed.volumes.len()).unwrap_or(u32::MAX),
    }
}

/// Counts desired, running tasks that are healthy. With a healthcheck the task
/// must report healthy; without one, it only must not report unhealthy.
fn healthy_replicas(service: &ObservedService) -> u16 {
    u16::try_from(
        service
            .tasks
            .iter()
            .filter(|task| {
                task.desired_running
                    && task.state == TaskState::Running
                    && if service.healthcheck_configured {
                        task.healthy == Some(true)
                    } else {
                        task.healthy != Some(false)
                    }
            })
            .count(),
    )
    .unwrap_or(u16::MAX)
}

/// Summarizes a service's problems: a convergence diagnostic when degraded or
/// failed, with Docker's reason ([`ObservedService::failure_reason`]), then
/// failures of currently desired tasks (historical tasks are ignored).
/// Capped at `MAX_SERVICE_DIAGNOSTICS`.
fn service_diagnostics(service: &ObservedService) -> Vec<DiagnosticView> {
    let mut diagnostics = Vec::new();
    if matches!(
        service.convergence,
        Convergence::Degraded | Convergence::Failed
    ) {
        let healthy = format!(
            "{} of {} observed replicas are healthy",
            healthy_replicas(service),
            service.replicas
        );
        diagnostics.push(DiagnosticView::new(
            ApiErrorCode::ServiceNotConverged,
            with_reason(healthy, service.failure_reason()),
        ));
    }
    diagnostics.extend(
        service
            .tasks
            .iter()
            .filter(|task| task.desired_running)
            .filter_map(|task| task.diagnostic.as_ref())
            .map(|diagnostic| {
                let (code, summary) = match diagnostic {
                    TaskDiagnostic::Failed { exit_code, .. } => (
                        ApiErrorCode::TaskFailed,
                        exit_code.map_or_else(
                            || "a desired task exited unsuccessfully".into(),
                            |code| format!("a desired task exited with status code {code}"),
                        ),
                    ),
                    TaskDiagnostic::Rejected { .. } => (
                        ApiErrorCode::TaskRejected,
                        "the runtime rejected a desired task before it started".into(),
                    ),
                };
                DiagnosticView::new(code, with_reason(summary, diagnostic.reason()))
            }),
    );
    diagnostics.truncate(MAX_SERVICE_DIAGNOSTICS);
    diagnostics
}

/// Appends Docker's `reason`, when it gave one, to a diagnostic's `summary`.
fn with_reason(summary: String, reason: Option<&RuntimeMessage>) -> String {
    match reason {
        Some(reason) => format!("{summary}: {reason}"),
        None => summary,
    }
}

/// Collects detail-page diagnostics in priority order: status message, latest
/// operation error, then per-service diagnostics. Capped at
/// `MAX_DETAIL_DIAGNOSTICS`.
pub(super) fn detail_diagnostics(
    status: &EnvironmentStatusView,
    observed: &ObservedApplicationView,
    operation: Option<&piqueld_core::Operation>,
) -> Vec<DiagnosticView> {
    let mut diagnostics = Vec::new();
    if let Some(message) = &status.message {
        diagnostics.push(DiagnosticView::new(
            ApiErrorCode::ApplicationStatus,
            message.clone(),
        ));
    }
    if let Some(operation) = operation
        && let (Some(code), Some(message)) = (&operation.error_code, &operation.error_message)
    {
        diagnostics.push(DiagnosticView {
            code: code.clone(),
            message: message.clone(),
        });
    }
    diagnostics.extend(
        observed
            .services
            .iter()
            .flat_map(|service| service.diagnostics.iter().cloned()),
    );
    diagnostics.truncate(MAX_DETAIL_DIAGNOSTICS);
    diagnostics
}

#[cfg(test)]
mod tests;
