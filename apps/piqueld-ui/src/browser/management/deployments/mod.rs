//! Deployment and promotion actions, plan dialogs, history, and snapshot
//! inspection.
mod actions;
mod history;
mod page;
mod plan;
mod snapshot;

pub(super) use actions::{DeploymentActions, EnvironmentAction, PromoteAction};
pub(super) use history::DeploymentHistory;
pub(super) use page::DeploymentPage;
pub(super) use plan::change_row;

/// Deployments the history asks for per page.
const PAGE: u16 = 20;
