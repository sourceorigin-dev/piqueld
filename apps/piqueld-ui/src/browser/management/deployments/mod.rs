//! Deployment and promotion actions, plan dialogs, history, a deployment's
//! page, and its snapshot.
mod actions;
mod history;
mod page;
mod plan;
mod snapshot;

pub(super) use actions::{DeploymentActions, EnvironmentAction, PromoteAction};
pub(super) use history::DeploymentHistory;
pub(super) use page::DeploymentPage;
pub(super) use plan::change_row;

use crate::browser::ui::key_values;
use leptos::prelude::*;
use std::fmt::Display;

/// Deployments the history asks for per page, and a deployment's page reads
/// to tell whether a newer one is listed.
const PAGE: u16 = 20;

/// Definition list of the values of `${{ vars.<name> }}` references.
fn variable_values<'a, V: Display + 'a>(
    variables: impl IntoIterator<Item = (&'a String, &'a V)>,
) -> AnyView {
    key_values(variables.into_iter().map(|(reference, value)| {
        (
            view! { <code>{reference.clone()}</code> },
            value.to_string(),
        )
    }))
}
