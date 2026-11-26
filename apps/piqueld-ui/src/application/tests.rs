use super::*;
use piqueld_client::{
    ApplicationId, ApplicationSpec, EnvironmentId, EnvironmentKind, EnvironmentName, GitRepository,
    LocalSnapshot, Metadata, PromotedFrom, RepositoryManifest, RunState,
    access::{Permission, Scope},
};
use std::collections::BTreeMap;

const APP: &str = "app-notes-01";

fn environment(id: &str, name: &str, source: EnvironmentSource) -> EnvironmentView {
    EnvironmentView {
        id: EnvironmentId::parse(id).expect("the environment id parses"),
        application_id: ApplicationId::parse(APP).expect("the application id parses"),
        name: EnvironmentName::parse(name).expect("the environment name parses"),
        source,
        kind: EnvironmentKind::Environment,
        sync: false,
        synced: None,
        telemetry: false,
        run_state: RunState::Running,
        resolved_generation: None,
        delete_intent: false,
        created_at_ms: 1,
        updated_at_ms: 1,
    }
}

fn preview(id: &str, branch: &str) -> EnvironmentView {
    let mut preview = environment(id, id, EnvironmentSource::Saved);
    preview.kind = EnvironmentKind::Preview(Box::new(piqueld_client::Preview {
        branch: branch.parse().expect("the branch name parses"),
        slot: None,
        slug: id.parse().expect("the slug parses"),
    }));
    preview
}

fn promoted(id: &str, name: &str, from: &str) -> EnvironmentView {
    environment(
        id,
        name,
        EnvironmentSource::Promoted(PromotedFrom {
            environment: EnvironmentId::parse(from).expect("the environment id parses"),
        }),
    )
}

fn branch(name: &str) -> EnvironmentSource {
    EnvironmentSource::Branch(TrackedBranch::new(name.into(), None).expect("the branch is valid"))
}

/// Application `notes` at revision 3, updated at 10, reading its
/// manifest from branch `main` when `connected`.
fn view(connected: bool, environments: Vec<EnvironmentView>) -> ApplicationView {
    let spec = ApplicationSpec {
        manifest: connected.then(|| RepositoryManifest {
            repository: GitRepository {
                url: "https://example.com/notes.git".into(),
                branch: "main".into(),
                commit: None,
            },
            path: "piqueld.toml".into(),
            sync: piqueld_client::sync::RepositorySync::default(),
        }),
        ..ApplicationSpec::default()
    };
    let application = ApplicationManifest {
        api_version: "piqueld.dev/v1alpha1".into(),
        kind: "Application".into(),
        metadata: Metadata {
            name: "notes".into(),
        },
        spec,
    }
    .validate_template()
    .expect("the template is valid")
    .normalize(ApplicationId::parse(APP).expect("the application id parses"));
    ApplicationView {
        spec_hash: application.spec_hash(),
        application,
        generation: 3,
        delete_intent: false,
        created_at_ms: 1,
        updated_at_ms: 10,
        environments,
        previews: Vec::new(),
        drift: Vec::new(),
        sync_check: None,
        repository: None,
    }
}

#[test]
fn replica_summaries_resolve_variables_per_environment() {
    let manifest: ApplicationManifest = serde_json::from_value(serde_json::json!({
        "api_version": "piqueld.dev/v1alpha1",
        "kind": "Application",
        "metadata": { "name": "notes" },
        "spec": {
            "variables": { "web_replicas": 2 },
            "environments": {
                "production": { "variables": { "web_replicas": 3, "api_replicas": 4 } },
                "staging": {},
            },
            "services": [
                { "name": "web", "source": { "type": "image", "image": "nginx:1.27" }, "replicas": "${{ vars.web_replicas }}" },
                { "name": "api", "source": { "type": "image", "image": "api:1" }, "replicas": "${{ vars.api_replicas }}" },
                { "name": "db", "source": { "type": "image", "image": "postgres:16" }, "replicas": 1 },
            ],
        },
    }))
    .expect("the manifest parses");
    let application = manifest
        .validate_template()
        .expect("the template is valid")
        .normalize(ApplicationId::parse(APP).expect("the application id parses"));
    let api = application.spec().services[0].clone();
    let db = application.spec().services[1].clone();
    let web = application.spec().services[2].clone();
    let mut view = ApplicationView {
        spec_hash: application.spec_hash(),
        application,
        generation: 1,
        delete_intent: false,
        created_at_ms: 1,
        updated_at_ms: 1,
        environments: Vec::new(),
        previews: Vec::new(),
        drift: Vec::new(),
        sync_check: None,
        repository: None,
    };
    assert_eq!(view.replica_summary(&db), "1 replica");
    assert_eq!(
        view.replica_summary(&web),
        "replicas from vars.web_replicas"
    );
    view.environments = vec![environment(APP, "production", EnvironmentSource::Saved)];
    assert_eq!(view.replica_summary(&web), "3 replicas");
    view.environments.push(environment(
        "app-notes-02",
        "staging",
        EnvironmentSource::Saved,
    ));
    assert_eq!(
        view.replica_summary(&web),
        "replicas: production 3, staging 2"
    );
    assert_eq!(view.replica_summary(&api), "replicas: production 4");
}

fn grants(permission: AppPermission, application: &str) -> Grants {
    let mut grants = Grants::default();
    grants
        .grant(
            Permission::App(permission),
            &Scope::one(ApplicationId::parse(application).expect("the application id parses")),
        )
        .expect("the permission is granted");
    grants
}

#[test]
fn query_parameters_open_only_tabs_the_page_has() {
    let query = |pairs: &'static [(&'static str, &'static str)]| {
        let pairs = pairs.iter().copied().collect::<BTreeMap<_, _>>();
        move |key: &str| pairs.get(key).map(|value| (*value).to_owned())
    };
    let environment = Page::Environment("env".into());
    assert_eq!(environment.initial_tab(query(&[("tab", "logs")])), "Logs");
    assert_eq!(slug("Command & arguments"), "command-arguments");
    assert_eq!(
        environment.initial_tab(query(&[("release", "r1")])),
        "Overview"
    );
    assert_eq!(
        Page::Application.initial_tab(query(&[("tab", "logs")])),
        "Overview"
    );
    assert_eq!(
        Page::Application.initial_tab(query(&[("release", "r1"), ("tab", "jobs")])),
        "Releases"
    );
    let deployment = Page::Deployment {
        environment: "env".into(),
        operation: "op".into(),
    };
    assert_eq!(deployment.initial_tab(query(&[])), "Timeline");
    assert_eq!(
        deployment.initial_tab(query(&[("tab", "snapshot")])),
        "Snapshot"
    );
}

#[test]
fn sources_and_origins_name_environments_or_their_id_once_gone() {
    let saved = view(
        false,
        vec![
            environment("env-production-01", "production", EnvironmentSource::Saved),
            promoted("env-staging-01", "staging", "env-production-01"),
        ],
    );
    let promotion = |environment: &str| DeploymentOrigin::Promotion {
        environment: EnvironmentId::parse(environment).expect("the environment id parses"),
        deployment: "dep-1".into(),
    };
    assert_eq!(
        saved.describe_origin(&promotion("env-production-01")),
        "Promoted from production (deployment dep-1)"
    );
    assert_eq!(
        saved.describe_origin(&promotion("env-deleted-01")),
        "Promoted from env-deleted-01 (deployment dep-1)"
    );
    assert_eq!(
        saved.describe_source(&saved.environments[1].source),
        "promoted from production"
    );
    assert_eq!(
        saved.describe_origin(&DeploymentOrigin::Restore {
            deployment: "dep-good".into(),
            rolled_back: Some("dep-failed".into()),
        }),
        "Rolled back from deployment dep-failed to deployment dep-good"
    );
    let snapshot = |base: Option<&str>| {
        DeploymentOrigin::Local(LocalSnapshot {
            commit: "0123456789abcdef0123456789abcdef01234567".into(),
            base: base.map(str::to_owned),
        })
    };
    assert_eq!(
        saved.describe_origin(&snapshot(Some("89abcdef0123456789abcdef0123456789abcdef"))),
        "Local snapshot 0123456789ab on 89abcdef0123"
    );
    assert_eq!(
        saved.describe_origin(&snapshot(None)),
        "Local snapshot 0123456789ab, uploaded complete"
    );
}

#[test]
fn previews_permissions_allow_only_in_previews() {
    let mut saved = view(
        false,
        vec![environment(
            "env-production-01",
            "production",
            EnvironmentSource::Saved,
        )],
    );
    saved.previews.push(preview("feature-1a2b3c", "feature"));
    let previews = grants(AppPermission::PreviewsDeploy, APP);
    assert!(
        previews
            .require_app(AppPermission::LogsRead, saved.application.id())
            .is_err()
    );
    assert!(saved.allows(&previews, AppPermission::LogsRead, Some("feature-1a2b3c")));
    assert!(!saved.allows(
        &previews,
        AppPermission::LogsRead,
        Some("env-production-01")
    ));
    assert!(!saved.allows(&previews, AppPermission::LogsRead, None));
    assert!(!saved.allows(&previews, AppPermission::Exec, Some("feature-1a2b3c")));
    let elsewhere = grants(AppPermission::LogsRead, "app-other-01");
    assert!(!saved.allows(&elsewhere, AppPermission::LogsRead, None));
}

#[test]
fn only_a_later_fetch_of_the_same_revision_counts_as_fetched() {
    let connected = view(true, Vec::new());
    assert!(connected.fetched_since(3, 11));
    assert!(!connected.fetched_since(3, 10), "not updated since");
    assert!(
        !connected.fetched_since(4, 11),
        "a newer revision conflicts"
    );
    assert!(
        !view(false, Vec::new()).fetched_since(3, 11),
        "only repositories are fetched"
    );
}

#[test]
fn only_environments_on_the_connection_branch_deploy_the_saved_manifest() {
    let mut saved = view(true, Vec::new());
    let main = environment("env-production-01", "production", branch("main"));
    assert!(
        !saved.follows_connection(&main),
        "nothing was read from the repository yet"
    );
    saved.repository = Some(saved.application.clone());
    assert!(saved.follows_connection(&main));
    let other = environment("env-staging-01", "staging", branch("develop"));
    assert!(!saved.follows_connection(&other));
    let mut preview = preview("main-1a2b3c", "main");
    preview.source = branch("main");
    assert!(!saved.follows_connection(&preview));
    let mut unconnected = view(false, Vec::new());
    unconnected.repository = Some(unconnected.application.clone());
    assert!(!unconnected.follows_connection(&main));
}

#[test]
fn releases_deploy_only_into_live_promoted_environments() {
    let mut deleting = promoted("env-qa-01", "qa", "env-production-01");
    deleting.delete_intent = true;
    let saved = view(
        false,
        vec![
            environment("env-production-01", "production", EnvironmentSource::Saved),
            promoted("env-staging-01", "staging", "env-production-01"),
            deleting,
        ],
    );
    assert_eq!(saved.release_targets(), ["env-staging-01"]);
}

#[test]
fn export_rows_trim_names_and_drop_blank_ones() {
    let row = ExportDraft {
        service: "api".into(),
        applications: " notes ,, billing, ".into(),
        previews: true,
    };
    let export = row.export();
    assert_eq!(export.applications, ["notes", "billing"]);
    let mut manifest = view(false, Vec::new()).application.to_manifest();
    manifest.spec.exports.push(export);
    assert_eq!(
        ExportDraft::rows(&manifest),
        [ExportDraft {
            service: "api".into(),
            applications: "notes, billing".into(),
            previews: true,
        }]
    );
}

#[test]
fn imports_without_an_environment_reach_the_same_named_one() {
    let row = |environment: &str| ImportDraft {
        application: "auth".into(),
        service: "api".into(),
        environment: environment.into(),
    };
    assert_eq!(row("").import().environment, None);
    let import = row("production").import();
    assert_eq!(
        import
            .environment
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some("production")
    );
    let mut manifest = view(false, Vec::new()).application.to_manifest();
    manifest.spec.imports = vec![import, row("").import()];
    assert_eq!(ImportDraft::rows(&manifest), [row("production"), row("")]);
}
