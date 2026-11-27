//! What the overview and the application directory show: each application's
//! health and latest activity, the newest deployments, and preview limits.
use crate::state::ApplicationHealth;
use piqueld_client::{
    ApplicationSummary, EnvironmentStatusView, EnvironmentView, Operation, OperationKind,
    PreviewUsage,
};
use std::cmp::Reverse;

/// One application in the dashboard list, with its environments' statuses and
/// latest deployments, all from the application listing.
#[derive(Clone, Debug)]
pub struct ApplicationRow {
    /// The listed application.
    pub application: ApplicationSummary,
    /// Its environments, in name order.
    pub environments: Vec<EnvironmentRow>,
    /// The latest deployment of each environment that has one.
    pub deployments: Vec<Operation>,
}

impl From<ApplicationSummary> for ApplicationRow {
    fn from(application: ApplicationSummary) -> Self {
        let deployments = application
            .environments
            .iter()
            .filter_map(|listed| listed.latest_operation.clone())
            .filter(|operation| operation.kind != OperationKind::Delete)
            .collect();
        let environments = application
            .environments
            .iter()
            .map(|listed| EnvironmentRow {
                environment: listed.environment.clone(),
                status: listed.status.clone(),
            })
            .collect();
        Self {
            application,
            environments,
            deployments,
        }
    }
}

/// One environment of a listed application, with its status.
#[derive(Clone, Debug)]
pub struct EnvironmentRow {
    /// The listed environment.
    pub environment: EnvironmentView,
    /// Its status; absent before one is recorded.
    pub status: Option<EnvironmentStatusView>,
}

impl EnvironmentRow {
    /// Health from the environment's status, `Stopped` while it is; a missing
    /// status shows as pending.
    #[must_use]
    pub fn health(&self) -> ApplicationHealth {
        self.status
            .as_ref()
            .map_or(ApplicationHealth::Pending, |status| {
                ApplicationHealth::from_server_state(status.state)
            })
            .unless_stopped(self.environment.stopped())
    }
}

impl ApplicationRow {
    /// Health badge for a row: the least healthy of its environments, or not
    /// deployed without any.
    #[must_use]
    pub fn health(&self) -> ApplicationHealth {
        self.environments
            .iter()
            .map(EnvironmentRow::health)
            .max_by_key(|health| health.severity())
            .unwrap_or(ApplicationHealth::NotDeployed)
    }

    /// The directory row's subtitle: a requested deletion, else the first
    /// status message among its environments, else its generation.
    #[must_use]
    pub fn subtitle(&self) -> String {
        if self.application.delete_intent {
            return "Deletion requested".to_owned();
        }
        self.environments
            .iter()
            .find_map(|row| {
                let message = row.status.as_ref()?.message.as_ref()?;
                Some(format!("{}: {message}", row.environment.name))
            })
            .unwrap_or_else(|| format!("Generation {}", self.application.generation))
    }

    /// When its newest deployment was requested, in Unix milliseconds.
    #[must_use]
    pub fn last_deployed_ms(&self) -> Option<i64> {
        self.deployments
            .iter()
            .map(|operation| operation.created_at_ms)
            .max()
    }

    /// The `limit` newest deployments across `rows`, newest first, each with
    /// its application: at most one per environment, its latest. Deployments
    /// requested in the same millisecond come in descending operation ID
    /// order, so the list never reshuffles.
    #[must_use]
    pub fn recent_deployments(
        rows: &[Self],
        limit: usize,
    ) -> Vec<(&ApplicationSummary, &Operation)> {
        let mut deployments = rows
            .iter()
            .flat_map(|row| {
                row.deployments
                    .iter()
                    .map(move |deployment| (&row.application, deployment))
            })
            .collect::<Vec<_>>();
        deployments.sort_by_key(|(_, operation)| {
            Reverse((operation.created_at_ms, operation.id.as_str()))
        });
        deployments.truncate(limit);
        deployments
    }
}

/// How previews use the daemon's `previews` limits, across the whole
/// installation and each application, from least to most constrained.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PreviewLoad {
    /// Every count is below its limit.
    Within,
    /// Some count reached its limit, so new previews there are refused.
    AtLimit,
    /// Some count exceeds its limit, which was lowered after they were
    /// created; they keep running.
    OverLimit,
}

impl PreviewLoad {
    /// The load of `usage`: the worst of its total and per-application counts.
    #[must_use]
    pub fn of(usage: &PreviewUsage) -> Self {
        let limits = &usage.limits;
        std::iter::once((usage.total, limits.max_total))
            .chain(
                usage
                    .applications
                    .iter()
                    .map(|application| (application.previews, limits.max_per_application)),
            )
            .map(|(count, max)| match count.cmp(&max) {
                std::cmp::Ordering::Less => Self::Within,
                std::cmp::Ordering::Equal => Self::AtLimit,
                std::cmp::Ordering::Greater => Self::OverLimit,
            })
            .max()
            .unwrap_or(Self::Within)
    }

    /// The label of its status badge.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Within => "Within limits",
            Self::AtLimit => "At limit",
            Self::OverLimit => "Over limit",
        }
    }
}

#[cfg(test)]
mod tests;
