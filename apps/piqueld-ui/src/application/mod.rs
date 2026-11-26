//! Application editor logic: the page and tab a route opens, what the editor
//! derives from the saved application, and the link rows of the Networking tab.

use piqueld_client::{
    ApplicationManifest, ApplicationView, DeploymentOrigin, EnvironmentSource, EnvironmentView,
    Export, Import, Reference, Segment, Service, TrackedBranch, Typed, VariableValue,
    access::{AppPermission, Grants},
    sync::short_commit,
};

/// Tabs of the application page.
pub const APPLICATION_TABS: [&str; 7] = [
    "Overview",
    "Services",
    "Networking",
    "Configuration",
    "Releases",
    "Events",
    "Settings",
];

/// Tabs of environment and preview pages.
pub const ENVIRONMENT_TABS: [&str; 7] = [
    "Overview",
    "Deployments",
    "Logs",
    "Traffic",
    "Terminal",
    "Events",
    "Settings",
];

/// Tabs of a deployment's page.
pub const DEPLOYMENT_TABS: [&str; 4] = ["Timeline", "Details", "Snapshot", "Attempts"];

/// Which page of an application the route shows.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Page {
    /// Shared configuration, environments, and application-wide history.
    Application,
    /// One service's shared configuration.
    Service(String),
    /// One environment's runtime, deployments, logs, traffic, terminal, history, and settings.
    Environment(String),
    /// One preview's branch, runtime, deployments, logs, traffic, terminal, history, and settings.
    Preview(String),
    /// One deployment of an environment or preview: its progress, steps,
    /// details, snapshot, and attempts.
    Deployment {
        /// The environment or preview it deploys.
        environment: String,
        /// Its operation's ID.
        operation: String,
    },
}

impl Page {
    /// Tabs of the page; service pages have their own section tabs.
    #[must_use]
    pub const fn tabs(&self) -> &'static [&'static str] {
        match self {
            Self::Environment(_) | Self::Preview(_) => &ENVIRONMENT_TABS,
            Self::Application | Self::Service(_) => &APPLICATION_TABS,
            Self::Deployment { .. } => &DEPLOYMENT_TABS,
        }
    }

    /// The accessible name of the page's tab list.
    #[must_use]
    pub const fn tabs_label(&self) -> &'static str {
        match self {
            Self::Environment(_) | Self::Preview(_) => "Environment sections",
            Self::Application | Self::Service(_) => "Application sections",
            Self::Deployment { .. } => "Deployment sections",
        }
    }

    /// The initial tab, reading query parameters with `query`: `?release=`
    /// opens releases, and `?tab=` names a tab by its `slug`. Tabs the page
    /// lacks open its first.
    #[must_use]
    pub fn initial_tab(&self, query: impl Fn(&str) -> Option<String>) -> &'static str {
        let requested = if query("release").is_some() {
            Some("releases".to_owned())
        } else {
            query("tab")
        };
        named_tab(self.tabs(), requested.as_deref())
    }
}

/// The tab of `tabs` whose `slug` is `requested`, else the first.
#[must_use]
pub fn named_tab(tabs: &'static [&'static str], requested: Option<&str>) -> &'static str {
    tabs.iter()
        .find(|tab| requested == Some(slug(tab).as_str()))
        .unwrap_or(&tabs[0])
}

/// `text` as an address or element ID fragment: lowercase, with runs of
/// other characters as one hyphen, so "Command & arguments" is
/// `command-arguments`.
#[must_use]
pub fn slug(text: &str) -> String {
    let mut slug = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_end_matches('-').to_owned()
}

/// What the application editor derives from the saved application.
pub trait ApplicationViewExt {
    /// Name of one of the application's environments or previews; its ID
    /// once it is gone.
    fn environment_name(&self, id: &str) -> String;

    /// Names of the application's environments, without its previews.
    fn environment_names(&self) -> Vec<String>;

    /// Where an environment deploys from, naming a promotion source:
    /// "the saved manifest", "branch main", or "promoted from staging".
    fn describe_source(&self, source: &EnvironmentSource) -> String;

    /// Where a deployment came from: "Built", "Promoted from staging
    /// (deployment …)", "Release redeployed", "Restored from deployment …",
    /// "Rolled back from deployment … to deployment …", or "Local snapshot
    /// 0123456789ab on 89abcdef0123".
    fn describe_origin(&self, origin: &DeploymentOrigin) -> String;

    /// What a deployment of `origin` is called in a list: "Deployment" for
    /// one built from its source, else what brought it, such as
    /// "Promotion from staging" or "Automatic rollback".
    fn deployment_title(&self, origin: &DeploymentOrigin) -> String;

    /// Whether `grants` hold `permission` on the application, or in
    /// `environment`, if that is one of its previews, the previews permission
    /// that also allows it there (see `AppPermission::on_previews`).
    fn allows(&self, grants: &Grants, permission: AppPermission, environment: Option<&str>)
    -> bool;

    /// Whether a repository-backed application at revision `generation`,
    /// last updated at `updated_at_ms`, holds a manifest fetched after this
    /// view's: same revision, updated later.
    fn fetched_since(&self, generation: u64, updated_at_ms: i64) -> bool;

    /// Whether `environment` deploys the saved manifest next: it follows the
    /// branch the repository connection names, and the application read a
    /// manifest from it, which it deploys until a push changes the manifest
    /// there. Previews always deploy their own branch's.
    fn follows_connection(&self, environment: &EnvironmentView) -> bool;

    /// IDs of the promoted environments a release can be deployed into:
    /// those not being deleted.
    fn release_targets(&self) -> Vec<String>;

    /// How many replicas `service` runs, for its row in the service list:
    /// "3 replicas", or, when a variable sets the count, its value in the
    /// sole environment, or in each ("replicas: production 3, staging 1"),
    /// or the variable's name while no environment gives it a value.
    fn replica_summary(&self, service: &Service) -> String;
}

impl ApplicationViewExt for ApplicationView {
    fn environment_name(&self, id: &str) -> String {
        self.deployable(id)
            .map_or_else(|| id.to_owned(), |environment| environment.name.to_string())
    }

    fn environment_names(&self) -> Vec<String> {
        self.environments
            .iter()
            .map(|environment| environment.name.to_string())
            .collect()
    }

    fn describe_source(&self, source: &EnvironmentSource) -> String {
        match source.promoted_from() {
            Some(from) => format!("promoted from {}", self.environment_name(from.as_str())),
            None => source.to_string(),
        }
    }

    fn deployment_title(&self, origin: &DeploymentOrigin) -> String {
        match origin {
            DeploymentOrigin::Build | DeploymentOrigin::Local(_) => "Deployment".into(),
            DeploymentOrigin::Promotion { environment, .. } => {
                format!(
                    "Promotion from {}",
                    self.environment_name(environment.as_str())
                )
            }
            DeploymentOrigin::Release => "Release redeployed".into(),
            DeploymentOrigin::Restore {
                rolled_back: None, ..
            } => "Restore".into(),
            DeploymentOrigin::Restore {
                rolled_back: Some(_),
                ..
            } => "Automatic rollback".into(),
        }
    }

    fn describe_origin(&self, origin: &DeploymentOrigin) -> String {
        match origin {
            DeploymentOrigin::Build => "Built".into(),
            DeploymentOrigin::Promotion {
                environment,
                deployment,
            } => format!(
                "Promoted from {} (deployment {deployment})",
                self.environment_name(environment.as_str())
            ),
            DeploymentOrigin::Release => "Release redeployed".into(),
            DeploymentOrigin::Restore {
                deployment,
                rolled_back: None,
            } => format!("Restored from deployment {deployment}"),
            DeploymentOrigin::Restore {
                deployment,
                rolled_back: Some(failed),
            } => format!("Rolled back from deployment {failed} to deployment {deployment}"),
            DeploymentOrigin::Local(snapshot) => match &snapshot.base {
                Some(base) => format!(
                    "Local snapshot {} on {}",
                    short_commit(&snapshot.commit),
                    short_commit(base)
                ),
                None => format!(
                    "Local snapshot {}, uploaded complete",
                    short_commit(&snapshot.commit)
                ),
            },
        }
    }

    fn allows(
        &self,
        grants: &Grants,
        permission: AppPermission,
        environment: Option<&str>,
    ) -> bool {
        let holds = |permission| {
            grants
                .require_app(permission, self.application.id())
                .is_ok()
        };
        let preview = environment.is_some_and(|id| {
            self.previews
                .iter()
                .any(|preview| preview.id.as_str() == id)
        });
        holds(permission) || (preview && permission.on_previews().is_some_and(holds))
    }

    fn fetched_since(&self, generation: u64, updated_at_ms: i64) -> bool {
        self.application.spec().manifest.is_some()
            && generation == self.generation
            && updated_at_ms > self.updated_at_ms
    }

    fn follows_connection(&self, environment: &EnvironmentView) -> bool {
        self.application
            .spec()
            .manifest
            .as_ref()
            .is_some_and(|connection| {
                environment.preview().is_none()
                    && self.repository.is_some()
                    && environment.source.branch()
                        == TrackedBranch::of(&connection.repository).ok().as_ref()
            })
    }

    fn replica_summary(&self, service: &Service) -> String {
        let count = |value: &dyn std::fmt::Display, one: bool| {
            format!("{value} replica{}", if one { "" } else { "s" })
        };
        let template = match &service.replicas {
            Typed::Literal(replicas) => return count(replicas, *replicas == 1),
            Typed::Template(template) => template,
        };
        let variable = match template.segments().as_deref() {
            Ok([Segment::Reference(Reference::Variable(name))]) => name.clone(),
            _ => return format!("{template} replicas"),
        };
        let values: Vec<(String, VariableValue)> = self
            .environments
            .iter()
            .filter_map(|environment| {
                let value = self
                    .application
                    .values(&environment.target())
                    .remove(&variable)??;
                Some((environment.name.to_string(), value))
            })
            .collect();
        match values.as_slice() {
            [] => format!("replicas from vars.{variable}"),
            [(_, value)] if self.environments.len() == 1 => {
                count(value, *value == VariableValue::Integer(1))
            }
            values => format!(
                "replicas: {}",
                values
                    .iter()
                    .map(|(environment, value)| format!("{environment} {value}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    fn release_targets(&self) -> Vec<String> {
        self.environments
            .iter()
            .filter(|env| env.source.promoted_from().is_some() && !env.delete_intent)
            .map(|env| env.id.to_string())
            .collect()
    }
}

/// One editable export. Applications are typed as one comma-separated list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExportDraft {
    /// The exported service.
    pub service: String,
    /// Applications allowed to reach it, separated by commas.
    pub applications: String,
    /// Whether previews may reach it too.
    pub previews: bool,
}

impl ExportDraft {
    /// The manifest's exports as editable rows.
    #[must_use]
    pub fn rows(manifest: &ApplicationManifest) -> Vec<Self> {
        manifest
            .spec
            .exports
            .iter()
            .map(|export| Self {
                service: export.service.clone(),
                applications: export.applications.join(", "),
                previews: export.previews,
            })
            .collect()
    }

    /// The export input, skipping blank application names; validation
    /// reports empty or invalid ones.
    #[must_use]
    pub fn export(self) -> Export {
        Export {
            service: self.service,
            applications: self
                .applications
                .split(',')
                .map(str::trim)
                .filter(|application| !application.is_empty())
                .map(str::to_owned)
                .collect(),
            previews: self.previews,
        }
    }
}

/// One editable import. An empty environment reaches the one named like the
/// importing environment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportDraft {
    /// The application exporting the service.
    pub application: String,
    /// The imported service.
    pub service: String,
    /// The exporting environment; empty for the same-named one.
    pub environment: String,
}

impl ImportDraft {
    /// The manifest's imports as editable rows.
    #[must_use]
    pub fn rows(manifest: &ApplicationManifest) -> Vec<Self> {
        manifest
            .spec
            .imports
            .iter()
            .map(|import| Self {
                application: import.application.clone(),
                service: import.service.clone(),
                environment: import
                    .environment
                    .as_ref()
                    .map_or_default(ToString::to_string),
            })
            .collect()
    }

    /// The import input.
    #[must_use]
    pub fn import(self) -> Import {
        Import {
            application: self.application,
            service: self.service,
            environment: Some(self.environment)
                .filter(|environment| !environment.is_empty())
                .map(Into::into),
        }
    }
}

#[cfg(test)]
mod tests;
