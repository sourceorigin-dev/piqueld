use super::*;

#[test]
fn changes_show_configuration_differences_without_exposing_sensitive_values() {
    let input = r#"api_version="piqueld.dev/v1alpha1"
kind="Application"
[metadata]
name="example"
[[spec.services]]
name="web"
replicas=1
[spec.services.source]
type="image"
image="example:1"
[spec.services.environment]
TOKEN="old-secret"
"#;
    let id = crate::ApplicationId::parse("app-example-01").unwrap();
    let original = crate::parse_toml(input).unwrap().normalize(id);
    let mut changed = original.to_manifest();
    changed.spec.services[0].replicas = 3.into();
    changed.spec.services[0]
        .environment
        .insert("TOKEN".into(), "new-secret".into());
    let changed = changed.validate().unwrap().normalize(original.id().clone());
    let (original, changed) = (original.spec().to_input(), changed.spec().to_input());
    let differences = ManifestChange::between(Some(&original), &changed);
    assert_eq!(differences.len(), 2);
    assert!(
        differences
            .iter()
            .any(|change| change.field == "services.web.replicas"
                && change.before.as_deref() == Some("1")
                && change.after.as_deref() == Some("3"))
    );
    let json = serde_json::to_string(&differences).unwrap();
    assert!(!json.contains("old-secret"));
    assert!(!json.contains("new-secret"));
    assert!(json.contains("redacted"));
    assert!(ManifestChange::between(Some(&original), &original).is_empty());
}

/// Edits list environment sources too, which plans leave out.
#[test]
fn edits_list_secret_declarations_and_environment_sources() {
    let spec = |body: &str| {
        crate::manifest::parse_template_toml(&format!(
            "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='example'\n[spec.manifest]\npath='app.toml'\n[spec.manifest.repository]\nurl='https://example.com/app.git'\nbranch='main'\n[[spec.services]]\nname='web'\nsource={{type='image',image='example:1'}}\n{body}"
        ))
        .unwrap()
        .spec()
        .clone()
    };
    let read = spec("[spec.environments.production]\n");
    let saved = spec(
        "[[spec.secrets]]\nname='token'\n[spec.secrets.generate]\ntype='random'\nbytes=16\nencoding='hex'\n[spec.environments.production]\nbranch='release'\n",
    );
    let fields = ManifestChange::edits(&read, &saved)
        .into_iter()
        .map(|change| change.field)
        .collect::<Vec<_>>();
    assert_eq!(fields, ["environments.production.branch", "secrets.token"]);
    // Plans show a generated secret's declaration, never a value.
    let planned = ManifestChange::between(Some(&read), &saved);
    assert_eq!(
        planned
            .iter()
            .map(|change| (change.field.as_str(), change.after.as_deref()))
            .collect::<Vec<_>>(),
        [(
            "secrets.token",
            Some(r#"{"generate":{"bytes":16,"encoding":"hex","type":"random"}}"#)
        )]
    );
    let rollback = spec("[spec.environments.production]\nrollback='manual'\n");
    let changes = ManifestChange::edits(&read, &rollback);
    assert_eq!(
        changes
            .iter()
            .map(|change| (change.field.as_str(), change.after.as_deref()))
            .collect::<Vec<_>>(),
        [("environments.production.rollback", Some("manual"))]
    );
}

#[test]
fn changes_show_new_and_changed_jobs() {
    let input = r#"api_version="piqueld.dev/v1alpha1"
kind="Application"
[metadata]
name="example"
[[spec.services]]
name="web"
[spec.services.source]
type="image"
image="example:1"
"#;
    let id = crate::ApplicationId::parse("app-example-01").unwrap();
    let original = crate::parse_toml(input).unwrap().normalize(id.clone());
    let with_jobs = |jobs: &[(&str, &str)]| {
        let mut manifest = original.to_manifest();
        manifest.spec.jobs = jobs
            .iter()
            .map(|(name, command)| crate::manifest::Job {
                name: (*name).into(),
                service: "web".into(),
                command: vec![(*command).into()],
                environment: BTreeMap::new(),
                secrets: Vec::new(),
                run: crate::manifest::JobRun::BeforeRollout,
                timeout_seconds: 300,
                volume_access: crate::manifest::JobVolumeAccess::ReadOnly,
                stop_service: false,
            })
            .collect();
        manifest
            .validate()
            .unwrap()
            .normalize(id.clone())
            .spec()
            .to_input()
    };
    let original = original.spec().to_input();
    let fields = |changes: Vec<ManifestChange>| {
        changes
            .into_iter()
            .map(|change| (change.field, change.before.is_some()))
            .collect::<Vec<_>>()
    };
    let migrate = [("migrate", "migrate")];
    assert_eq!(
        fields(ManifestChange::between(
            Some(&original),
            &with_jobs(&migrate)
        )),
        [
            ("jobs.migrate.command".to_owned(), false),
            ("jobs.migrate.run".to_owned(), false),
            ("jobs.migrate.service".to_owned(), false),
            ("jobs.migrate.timeout_seconds".to_owned(), false),
            ("jobs.order".to_owned(), false),
        ]
    );
    assert_eq!(
        fields(ManifestChange::between(
            Some(&with_jobs(&migrate)),
            &with_jobs(&[("migrate", "upgrade")])
        )),
        [("jobs.migrate.command".to_owned(), true)]
    );
    assert_eq!(
        fields(ManifestChange::between(
            Some(&with_jobs(&[("migrate", "migrate"), ("seed", "seed")])),
            &with_jobs(&[("seed", "seed"), ("migrate", "migrate")])
        )),
        [("jobs.order".to_owned(), true)]
    );
}
