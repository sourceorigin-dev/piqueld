use super::*;
use crate::manifest::parse_template_toml;
use crate::{ApplicationId, EnvironmentId, PromotedFrom};

const CONNECTION: &str = "[spec.manifest]\npath='app.toml'\n[spec.manifest.repository]\nurl='https://example.com/notes.git'\nbranch='main'\n";

fn spec(body: &str) -> Result<ApplicationSpec, ValidationErrors> {
    parse_template_toml(&format!(
        "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='notes'\n{body}"
    ))
    .map(|template| {
        template
            .normalize(ApplicationId::parse("app-notes").expect("the application id is valid"))
            .spec()
            .clone()
    })
}

fn codes(body: &str) -> Vec<(String, String)> {
    spec(body)
        .expect_err("the spec is rejected")
        .0
        .into_iter()
        .map(|error| (error.path, error.code))
        .collect()
}

fn name(name: &str) -> EnvironmentName {
    EnvironmentName::parse(name).expect("the environment name is valid")
}

fn branch(branch: &str, commit: Option<&str>) -> DeclaredSource {
    DeclaredSource::Branch(
        TrackedBranch::new(branch.into(), commit.map(Into::into))
            .expect("the tracked branch is valid"),
    )
}

fn environment(id: &str, source: EnvironmentSource, sync: bool) -> EnvironmentView {
    EnvironmentView {
        id: EnvironmentId::parse(format!("env-0000-{id}")).expect("the environment id is valid"),
        application_id: ApplicationId::parse("app-notes").expect("the application id is valid"),
        name: name(id),
        source,
        kind: crate::EnvironmentKind::Environment,
        sync,
        synced: None,
        telemetry: false,
        run_state: crate::api::RunState::Running,
        resolved_generation: None,
        delete_intent: false,
        created_at_ms: 0,
        updated_at_ms: 0,
    }
}

#[test]
fn declarations_resolve_sources_with_promotion_sources_first() {
    let spec = spec(&format!(
        "{CONNECTION}[spec.environments.production]\npromote_from='staging'\n[spec.environments.staging]\nsync=true\n[spec.environments.canary]\nbranch='next'\ncommit='{}'\n",
        "a".repeat(40)
    ))
    .expect("the spec is built");
    let declarations = spec
        .declared_environments()
        .expect("the environments declare");
    assert_eq!(
        declarations,
        [
            Declaration {
                name: name("canary"),
                source: branch("next", Some(&"a".repeat(40))),
                sync: false,
                manages_source: true,
            },
            Declaration {
                name: name("staging"),
                source: branch("main", None),
                sync: true,
                manages_source: true,
            },
            Declaration {
                name: name("production"),
                source: DeclaredSource::Promoted {
                    from: name("staging")
                },
                sync: false,
                manages_source: true,
            },
        ]
    );
    let saved =
        self::spec("[spec.environments.staging.variables]\nx=1\n").expect("the spec is built");
    assert_eq!(
        saved
            .declared_environments()
            .expect("the environments declare")[0]
            .source,
        DeclaredSource::Saved
    );
}

#[test]
fn the_staging_and_production_example_declares_both() {
    let example = parse_template_toml(include_str!(
        "../../../../../examples/staging-production.toml"
    ))
    .expect("the template toml parses")
    .normalize(ApplicationId::parse("app-notes").expect("the application id is valid"));
    assert_eq!(
        example
            .spec()
            .declared_environments()
            .expect("the environments declare")
            .into_iter()
            .map(|declaration| (
                declaration.name.to_string(),
                declaration.source.to_string(),
                declaration.sync
            ))
            .collect::<Vec<_>>(),
        [
            ("staging".into(), "branch main".into(), true),
            (
                "production".into(),
                "releases promoted from staging".into(),
                false
            ),
        ]
    );
}

/// Disconnecting the repository returns declared environments to the
/// saved manifest instead of failing on fields that need one.
#[test]
fn disconnecting_drops_what_needs_a_repository() {
    let mut manifest = parse_template_toml(include_str!(
        "../../../../../examples/staging-production.toml"
    ))
    .expect("the template toml parses")
    .normalize(ApplicationId::parse("app-notes").expect("the application id is valid"))
    .to_manifest();
    manifest
        .spec
        .environments
        .get_mut("staging")
        .expect("the entry exists")
        .branch = Some("next".into());
    crate::edit::ApplicationEdit::Repository(None)
        .apply(&mut manifest)
        .expect("the change applies");
    let sources = manifest
        .validate_template()
        .expect("the template validates")
        .spec()
        .declared_environments()
        .expect("the environments declare")
        .into_iter()
        .map(|declaration| (declaration.source.to_string(), declaration.sync))
        .collect::<Vec<_>>();
    assert_eq!(
        sources,
        [
            ("the saved manifest".into(), false),
            ("releases promoted from staging".into(), false),
        ]
    );
}

/// Blocks that set no source, as every block did before blocks declared
/// sources, leave existing environments' sources and sync alone; one that
/// removes the source it set returns its environment to the default.
#[test]
fn only_declared_sources_change_existing_environments() {
    let staging = environment(
        "staging",
        EnvironmentSource::Branch(
            TrackedBranch::new("main".into(), None).expect("the tracked branch is valid"),
        ),
        true,
    );
    let production = environment(
        "production",
        EnvironmentSource::Promoted(PromotedFrom {
            environment: staging.id.clone(),
        }),
        false,
    );
    let environments = [production, staging];
    let legacy = spec(&format!(
        "{CONNECTION}[spec.environments.staging.variables]\nx=1\n[spec.environments.production]\nvisibility='private'\n"
    ))
    .expect("the spec is built");
    let declarations = legacy
        .declared_environments()
        .expect("the environments declare");
    assert_eq!(EnvironmentChange::between(&declarations, &environments), []);

    let declared = spec(&format!(
        "{CONNECTION}[spec.environments.staging]\nsync=true\n[spec.environments.production]\npromote_from='staging'\n"
    ))
    .expect("the spec is built");
    let removed = legacy
        .declared_environments_since(Some(&declared))
        .expect("the removed environments declare");
    assert_eq!(
        EnvironmentChange::between(&removed, &environments)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        [
            "production: deploys from branch main instead of releases promoted from staging",
            "staging: no longer follows pushes",
        ]
    );
}

#[test]
fn impossible_sources_fail_at_their_field() {
    let field = |path: &str, code: ApiErrorCode| (path.to_owned(), code.into());
    assert_eq!(
        codes("[spec.environments.staging]\nbranch='main'\nsync=true\n"),
        [
            field(
                "spec.environments.staging.branch",
                ApiErrorCode::ManifestRepositoryRequired
            ),
            field(
                "spec.environments.staging.sync",
                ApiErrorCode::ManifestRepositoryRequired
            ),
        ]
    );
    assert_eq!(
        codes(&format!(
            "{CONNECTION}[spec.environments.production]\npromote_from='staging'\nsync=true\n[spec.environments.staging]\n"
        )),
        [field(
            "spec.environments.production.promote_from",
            ApiErrorCode::EnvironmentSourceConflict
        )]
    );
    assert_eq!(
        codes(&format!(
            "{CONNECTION}[spec.environments.staging]\ncommit='{}'\nsync=true\n",
            "a".repeat(40)
        )),
        [field(
            "spec.environments.staging.sync",
            ApiErrorCode::EnvironmentSyncPinned
        )]
    );
    assert_eq!(
        codes(&format!(
            "{CONNECTION}[spec.environments.staging]\nbranch='a..b'\n"
        )),
        [field(
            "spec.environments.staging.branch",
            ApiErrorCode::GitBranchInvalid
        )]
    );
    assert_eq!(
        codes("[spec.environments.production]\npromote_from='qa'\n"),
        [field(
            "spec.environments.production.promote_from",
            ApiErrorCode::PromotionSourceInvalid
        )]
    );
    assert_eq!(
        codes(
            "[spec.environments.a]\npromote_from='b'\n[spec.environments.b]\npromote_from='c'\n[spec.environments.c]\npromote_from='a'\n"
        ),
        [field(
            "spec.environments.c.promote_from",
            ApiErrorCode::PromotionCycle
        )]
    );
    assert_eq!(
        codes("[spec.environments.a]\npromote_from='a'\n"),
        [field(
            "spec.environments.a.promote_from",
            ApiErrorCode::PromotionCycle
        )]
    );
}

#[test]
fn changes_create_resource_and_report_what_the_manifest_dropped() {
    let staging = environment("staging", EnvironmentSource::Saved, false);
    let production = environment(
        "production",
        EnvironmentSource::Branch(
            TrackedBranch::new("main".into(), None).expect("the tracked branch is valid"),
        ),
        false,
    );
    let old = environment(
        "old",
        EnvironmentSource::Promoted(PromotedFrom {
            environment: staging.id.clone(),
        }),
        false,
    );
    let declarations = [
        Declaration {
            name: name("staging"),
            source: branch("main", None),
            sync: true,
            manages_source: true,
        },
        Declaration {
            name: name("production"),
            source: DeclaredSource::Promoted {
                from: name("staging"),
            },
            sync: false,
            manages_source: true,
        },
        Declaration {
            name: name("qa"),
            source: branch("main", None),
            sync: false,
            manages_source: true,
        },
    ];
    let changes = EnvironmentChange::between(
        &declarations,
        &[old.clone(), production.clone(), staging.clone()],
    );
    assert_eq!(
        changes.iter().map(ToString::to_string).collect::<Vec<_>>(),
        [
            "staging: deploys from branch main instead of the saved manifest",
            "staging: follows pushes",
            "production: deploys from releases promoted from staging instead of branch main",
            "qa: created, deploys from branch main",
            "old: no longer declared; kept until deleted with `piquelctl env delete`",
        ]
    );
    assert_eq!(
        EnvironmentChange::between(&[], &[old, production, staging]),
        []
    );
}
