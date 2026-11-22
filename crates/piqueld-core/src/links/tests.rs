use super::*;
use crate::EnvironmentName;

fn party(id: &str, application: &str, name: &str) -> LinkParty {
    LinkParty {
        environment: EnvironmentId::parse(format!("{id}-0000000")).unwrap(),
        application_id: ApplicationId::parse(format!("app-{application}")).unwrap(),
        application: ApplicationName::parse(application).unwrap(),
        name: name.into(),
        preview: false,
        exports: Vec::new(),
        imports: Vec::new(),
        bindings: Bindings::new(),
    }
}

/// Binds every party's names as deploying each now would.
fn deployed(mut parties: Vec<LinkParty>) -> Vec<LinkParty> {
    let applications = parties
        .iter()
        .map(|party| (party.application.clone(), party.application_id.clone()))
        .collect::<Bindings>();
    for party in &mut parties {
        party.bindings = bind(
            &party.exports,
            &party.imports,
            &party.bindings,
            &applications,
        );
    }
    parties
}

fn export(service: &str, applications: &[&str], previews: bool) -> ValidatedExport {
    ValidatedExport {
        service: ServiceName::parse(service).unwrap(),
        applications: applications
            .iter()
            .map(|application| ApplicationName::parse(*application).unwrap())
            .collect(),
        previews,
    }
}

fn import(application: &str, service: &str, environment: Option<&str>) -> ValidatedImport {
    ValidatedImport {
        application: ApplicationName::parse(application).unwrap(),
        service: ServiceName::parse(service).unwrap(),
        environment: environment.map(|name| EnvironmentName::parse(name).unwrap()),
    }
}

#[test]
fn imports_connect_to_the_same_named_environment_that_exports_to_them() {
    let mut auth_staging = party("a", "auth", "staging");
    auth_staging.exports = vec![export("api", &["notes"], false)];
    let mut auth_production = party("b", "auth", "production");
    auth_production.exports = vec![export("api", &["notes"], false)];
    let mut notes = party("c", "notes", "staging");
    notes.imports = vec![import("auth", "api", None)];
    let mut other = party("d", "other", "staging");
    other.imports = vec![import("auth", "api", None)];
    let parties = deployed(vec![auth_staging, auth_production, notes, other]);

    let links = links(&parties);
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].importer, parties[2].environment);
    assert_eq!(links[0].exporter, parties[0].environment);
    assert_eq!(links[0].exporter_application.as_str(), "auth");

    let report = report(&parties[3].environment, &parties, &Scope::All);
    assert_eq!(report.imports[0].state, ImportState::Unavailable);
    assert_eq!(report.imports[0].alias, "api.auth");
    let report = super::report(&parties[0].environment, &parties, &Scope::All);
    assert_eq!(report.importers.len(), 1);
    assert_eq!(report.importers[0].application.as_str(), "notes");
    // Readers who cannot see notes do not learn it imports anything.
    let hidden = Scope::one(parties[0].application_id.clone());
    let report = super::report(&parties[0].environment, &parties, &hidden);
    assert_eq!(report.importers, Vec::new());
}

#[test]
fn imports_name_another_environment_and_previews_need_the_exporters_consent() {
    let mut auth = party("a", "auth", "staging");
    auth.exports = vec![export("api", &["notes"], false)];
    let mut preview = party("b", "notes", "notes-feat-1a2b3c");
    preview.preview = true;
    preview.imports = vec![import("auth", "api", Some("staging"))];
    let mut missing = party("c", "notes", "production");
    missing.imports = vec![import("auth", "api", None)];
    let mut parties = deployed(vec![auth, preview, missing]);

    assert_eq!(links(&parties), Vec::new());
    // Not exporting to previews and not existing look the same.
    for party in &parties[1..] {
        assert_eq!(
            report(&party.environment, &parties, &Scope::All).imports[0].state,
            ImportState::Unavailable
        );
    }

    parties[0].exports[0].previews = true;
    let links = links(&parties);
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].importer, parties[1].environment);
}

#[test]
fn links_group_the_services_one_environment_imports_from_another() {
    let mut auth = party("a", "auth", "staging");
    auth.exports = vec![
        export("api", &["notes"], false),
        export("events", &["notes"], false),
        export("admin", &["other"], false),
    ];
    let mut notes = party("b", "notes", "staging");
    notes.imports = vec![
        import("auth", "api", None),
        import("auth", "events", None),
        import("auth", "admin", None),
    ];
    let links = links(&deployed(vec![auth, notes]));
    assert_eq!(links.len(), 1);
    assert_eq!(
        links[0]
            .services
            .iter()
            .map(ServiceName::as_str)
            .collect::<Vec<_>>(),
        ["api", "events"]
    );
}

#[test]
fn grants_follow_the_application_bound_when_deployed_not_its_name() {
    let mut auth = party("a", "auth", "staging");
    auth.exports = vec![export("api", &["notes"], false)];
    let mut notes = party("b", "notes", "staging");
    notes.imports = vec![import("auth", "api", None)];
    let mut parties = deployed(vec![auth, notes]);
    assert_eq!(links(&parties).len(), 1);

    // notes is renamed, and another application takes its name: notes
    // keeps its link, the newcomer gets none, even after auth redeploys.
    parties[1].application = ApplicationName::parse("journal").unwrap();
    let mut impostor = party("c", "notes", "staging");
    impostor.application_id = ApplicationId::parse("app-impostor").unwrap();
    impostor.imports = vec![import("auth", "api", None)];
    parties.push(impostor);
    let parties = deployed(parties);
    let links = links(&parties);
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].importer, parties[1].environment);
    assert_eq!(
        report(&parties[2].environment, &parties, &Scope::All).imports[0].state,
        ImportState::Unavailable
    );

    // Renaming auth keeps notes' link, which reports auth's new name.
    let mut parties = parties;
    parties[0].application = ApplicationName::parse("identity").unwrap();
    let report = report(&parties[1].environment, &parties, &Scope::All);
    assert_eq!(report.imports[0].state, ImportState::Connected);
    assert_eq!(report.imports[0].alias, "api.identity");
}

#[test]
fn unbound_names_match_the_application_holding_them() {
    let mut auth = party("a", "auth", "staging");
    auth.exports = vec![export("api", &["notes"], false)];
    // auth deploys before notes exists, so its grant stays unbound.
    let auth = deployed(vec![auth]).remove(0);
    assert_eq!(auth.bindings, Bindings::new());
    let mut notes = party("b", "notes", "staging");
    notes.imports = vec![import("auth", "api", None)];
    let notes = deployed(vec![auth.clone(), notes]).remove(1);
    assert_eq!(links(&[auth, notes]).len(), 1);
}

#[test]
fn exporters_link_to_at_most_the_oldest_importers() {
    let mut auth = party("auth", "auth", "staging");
    auth.exports = vec![export("api", &["notes"], false)];
    let mut parties = vec![auth];
    for index in 0..=MAX_IMPORTERS {
        let mut notes = party(&format!("notes{index}"), "notes", "staging");
        notes.imports = vec![import("auth", "api", None)];
        parties.push(notes);
    }
    let parties = deployed(parties);
    let links = links(&parties);
    assert_eq!(links.len(), MAX_IMPORTERS);
    assert!(
        links
            .iter()
            .all(|link| link.importer != parties[MAX_IMPORTERS + 1].environment)
    );
    assert_eq!(
        report(
            &parties[MAX_IMPORTERS + 1].environment,
            &parties,
            &Scope::All
        )
        .imports[0]
            .state,
        ImportState::Full
    );
}
