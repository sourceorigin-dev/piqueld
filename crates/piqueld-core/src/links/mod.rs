//! Resolves private links between environments from what each one deploys.
//!
//! An import of environment `I` connects to the environment `E` of the
//! imported application named like `I` (or as the import says) once `E`
//! exports the service to `I`'s application, and to previews when `I` is one.
//! Both sides are read from their active targets, so a link appears or goes
//! away when either side deploys, and never from a manifest only saved.
//!
//! Manifests name applications, but deploying binds each name to the
//! application holding it ([`bind`]): links then follow that application,
//! renamed or not, and another one taking a freed name gains nothing.

use crate::access::Scope;
use crate::manifest::{ValidatedExport, ValidatedImport};
use crate::{ApplicationId, ApplicationName, EnvironmentId, ServiceName};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use utoipa::ToSchema;

/// Most environments that may link to one exporting environment. Each link
/// attaches the exported services to one more network, so the oldest
/// importers keep their links and newer ones wait for a free slot.
pub const MAX_IMPORTERS: usize = 64;

/// The application each name an environment's exports and imports use is
/// bound to.
pub type Bindings = BTreeMap<ApplicationName, ApplicationId>;

/// Binds each application `exports` and `imports` name: to the application
/// `previous` bound it to, while that one still exists, else to the one
/// holding the name in `applications`. A name no application holds stays
/// unbound, and matches whichever application holds it until the
/// environment deploys again.
#[must_use]
pub fn bind(
    exports: &[ValidatedExport],
    imports: &[ValidatedImport],
    previous: &Bindings,
    applications: &Bindings,
) -> Bindings {
    exports
        .iter()
        .flat_map(|export| &export.applications)
        .chain(imports.iter().map(|import| &import.application))
        .filter_map(|name| {
            let id = previous
                .get(name)
                .filter(|id| applications.values().any(|existing| existing == *id))
                .or_else(|| applications.get(name))?;
            Some((name.clone(), id.clone()))
        })
        .collect()
}

/// One environment's identity and the links its active target declares.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinkParty {
    /// Environment identity.
    pub environment: EnvironmentId,
    /// Its application's identity.
    pub application_id: ApplicationId,
    /// Current name of its application.
    pub application: ApplicationName,
    /// Environment name, or a preview's slug.
    pub name: String,
    /// Whether the environment is a preview.
    pub preview: bool,
    /// Services it lets other applications reach.
    pub exports: Vec<ValidatedExport>,
    /// Services of other applications it reaches.
    pub imports: Vec<ValidatedImport>,
    /// The applications its exports and imports name, as deployed.
    pub bindings: Bindings,
}

/// A link between two environments: the `services` of `exporter` that every
/// service of `importer` reaches as `<service>.<exporter_application>`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Link {
    /// Environment whose services reach the exported ones.
    pub importer: EnvironmentId,
    /// Environment whose services are reached.
    pub exporter: EnvironmentId,
    /// Current name of the exporter's application, which aliases carry.
    pub exporter_application: ApplicationName,
    /// Exported services the importer imports.
    pub services: BTreeSet<ServiceName>,
}

/// Whether an import connects, and why not.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ImportState {
    /// The service is reachable.
    Connected,
    /// No deployed environment of that name exports the service to this
    /// application, or to previews. Whether the environment exists is not
    /// told apart, since the importer may not be allowed to see it.
    Unavailable,
    /// The environment exports the service, but already links to
    /// [`MAX_IMPORTERS`] older environments.
    Full,
}

/// One import of an environment and whether it connects.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ImportStatus {
    /// Application exporting the service.
    pub application: ApplicationName,
    /// Imported service.
    pub service: ServiceName,
    /// Environment of the application the import reaches.
    pub environment: String,
    /// Name the service answers to: `<service>.<application>`.
    pub alias: String,
    /// Whether it connects.
    pub state: ImportState,
}

/// An environment that reaches one of this environment's exported services.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Importer {
    /// Importing application.
    pub application: ApplicationName,
    /// Importing environment, or a preview's slug.
    pub environment: String,
    /// Exported service it reaches.
    pub service: ServiceName,
}

/// The links an environment takes part in, as deployed.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct LinkReport {
    /// Each import of the environment and whether it connects.
    pub imports: Vec<ImportStatus>,
    /// Environments that reach its exported services.
    pub importers: Vec<Importer>,
}

impl LinkParty {
    /// The environment `import` reaches: the one it names, else the one named
    /// like this environment.
    fn target<'a>(&'a self, import: &'a ValidatedImport) -> &'a str {
        import
            .environment
            .as_ref()
            .map_or(self.name.as_str(), |environment| environment.as_str())
    }

    /// Whether `name`, as this environment's exports or imports use it, is
    /// `other`'s application: the one it bound the name to, or the one
    /// holding the name while it is unbound.
    fn names(&self, name: &ApplicationName, other: &Self) -> bool {
        self.bindings.get(name).map_or_else(
            || *name == other.application,
            |id| *id == other.application_id,
        )
    }

    /// Whether this environment exports `service` to `importer`'s application.
    fn exports_to(&self, service: &ServiceName, importer: &Self) -> bool {
        self.exports.iter().any(|export| {
            export.service == *service
                && export
                    .applications
                    .iter()
                    .any(|name| self.names(name, importer))
                && (!importer.preview || export.previews)
        })
    }

    /// The exporter `import` resolves to among `parties`, if one exports the
    /// service to this environment. Previews are never exporters.
    fn resolve<'a>(&self, import: &ValidatedImport, parties: &'a [Self]) -> Option<&'a Self> {
        parties.iter().find(|party| {
            !party.preview
                && self.names(&import.application, party)
                && party.name == self.target(import)
                && party.environment != self.environment
                && party.exports_to(&import.service, self)
        })
    }
}

/// Every link between `parties`, one per importer and exporter pair.
/// `parties` come oldest first: once an exporter links to [`MAX_IMPORTERS`]
/// environments, newer ones get no link to it.
#[must_use]
pub fn links(parties: &[LinkParty]) -> Vec<Link> {
    let mut links = BTreeMap::<(&EnvironmentId, &EnvironmentId), Link>::new();
    let mut importers = BTreeMap::<&EnvironmentId, usize>::new();
    for importer in parties {
        for import in &importer.imports {
            let Some(exporter) = importer.resolve(import, parties) else {
                continue;
            };
            let key = (&importer.environment, &exporter.environment);
            if !links.contains_key(&key) {
                let count = importers.entry(&exporter.environment).or_default();
                if *count == MAX_IMPORTERS {
                    continue;
                }
                *count += 1;
            }
            links
                .entry(key)
                .or_insert_with(|| Link {
                    importer: importer.environment.clone(),
                    exporter: exporter.environment.clone(),
                    exporter_application: exporter.application.clone(),
                    services: BTreeSet::new(),
                })
                .services
                .insert(import.service.clone());
        }
    }
    links.into_values().collect()
}

/// Reports the links of `environment` among `parties`: each of its imports,
/// and the environments importing its services whose applications are in
/// `readable`.
#[must_use]
pub fn report(environment: &EnvironmentId, parties: &[LinkParty], readable: &Scope) -> LinkReport {
    let Some(party) = parties
        .iter()
        .find(|party| &party.environment == environment)
    else {
        return LinkReport::default();
    };
    let links = links(parties);
    let imports = party
        .imports
        .iter()
        .map(|import| {
            let exporter = party.resolve(import, parties);
            let state = match exporter {
                None => ImportState::Unavailable,
                Some(exporter)
                    if links.iter().any(|link| {
                        link.importer == party.environment && link.exporter == exporter.environment
                    }) =>
                {
                    ImportState::Connected
                }
                Some(_) => ImportState::Full,
            };
            let application = exporter
                .map_or(&import.application, |exporter| &exporter.application)
                .clone();
            ImportStatus {
                alias: crate::manifest::links::alias(&import.service, &application),
                application,
                service: import.service.clone(),
                environment: party.target(import).to_owned(),
                state,
            }
        })
        .collect();
    let mut importers = links
        .iter()
        .filter(|link| &link.exporter == environment)
        .filter_map(|link| {
            let importer = parties
                .iter()
                .find(|party| party.environment == link.importer)?;
            readable
                .contains(&importer.application_id)
                .then_some((importer, link))
        })
        .flat_map(|(importer, link)| {
            link.services.iter().map(|service| Importer {
                application: importer.application.clone(),
                environment: importer.name.clone(),
                service: service.clone(),
            })
        })
        .collect::<Vec<_>>();
    importers.sort();
    LinkReport { imports, importers }
}

#[cfg(test)]
mod tests;
