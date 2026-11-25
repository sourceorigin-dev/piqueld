use super::*;
use piqueld_client::{
    ApplicationId, ApplicationManifest, EnvironmentKind, EnvironmentName, Metadata, PromotedFrom,
    RunState,
};

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

fn environment(id: &str, name: &str, source: EnvironmentSource) -> EnvironmentView {
    EnvironmentView {
        id: EnvironmentId::parse(id).unwrap(),
        application_id: ApplicationId::parse("app-00000001").unwrap(),
        name: EnvironmentName::parse(name).unwrap(),
        source,
        kind: EnvironmentKind::Environment,
        sync: false,
        synced: None,
        telemetry: false,
        run_state: RunState::Running,
        resolved_generation: None,
        delete_intent: false,
        created_at_ms: 0,
        updated_at_ms: 0,
    }
}

/// An application at generation 3 with `environments`.
fn application(environments: Vec<EnvironmentView>) -> ApplicationView {
    let id = ApplicationId::parse("app-00000001").unwrap();
    let application = ApplicationManifest {
        api_version: "piqueld.dev/v1alpha1".into(),
        kind: "Application".into(),
        metadata: Metadata {
            name: "notes".into(),
        },
        spec: ApplicationSpec::default(),
    }
    .validate_template()
    .unwrap()
    .normalize(id);
    ApplicationView {
        spec_hash: application.spec_hash(),
        application,
        generation: 3,
        delete_intent: false,
        created_at_ms: 0,
        updated_at_ms: 0,
        environments,
        previews: Vec::new(),
        drift: Vec::new(),
        sync_check: None,
        repository: None,
    }
}

fn names(saved: &ApplicationView) -> Vec<&str> {
    saved
        .environments
        .iter()
        .map(|env| env.name.as_str())
        .collect()
}

fn branch(branch: &str, commit: Option<&str>) -> TrackedBranch {
    TrackedBranch::new(branch.into(), commit.map(Into::into)).unwrap()
}

fn promoted(from: &str) -> EnvironmentSource {
    EnvironmentSource::Promoted(PromotedFrom {
        environment: EnvironmentId::parse(from).unwrap(),
    })
}

/// The form showing `branch` and `commit`, building its own source.
fn building(branch: &str, commit: &str) -> SourceDraft {
    SourceDraft {
        branch: BranchDraft {
            branch: branch.into(),
            commit: commit.into(),
        },
        promote_from: String::new(),
    }
}

#[test]
fn a_renamed_environment_moves_to_its_name_order_and_advances_the_revision() {
    let mut saved = application(vec![
        environment("env-00000001", "alpha", EnvironmentSource::Saved),
        environment("env-00000002", "beta", EnvironmentSource::Saved),
    ]);
    let renamed = environment("env-00000001", "zeta", EnvironmentSource::Saved);
    let edit = EnvironmentEdit::Rename {
        id: "env-00000001".into(),
        name: "zeta".into(),
    };
    edit.apply(&mut saved, Some(renamed));
    assert_eq!(names(&saved), ["beta", "zeta"]);
    assert_eq!(saved.generation, 4);
}

#[test]
fn unguarded_changes_keep_the_revision_later_edits_expect() {
    let mut saved = application(vec![environment(
        "env-00000001",
        "alpha",
        EnvironmentSource::Saved,
    )]);
    let mut changed = saved.environments[0].clone();
    changed.sync = true;
    let id = String::from("env-00000001");
    EnvironmentEdit::Sync {
        id: id.clone(),
        enabled: true,
    }
    .apply(&mut saved, Some(changed.clone()));
    changed.telemetry = true;
    EnvironmentEdit::Telemetry {
        id: id.clone(),
        granted: true,
    }
    .apply(&mut saved, Some(changed.clone()));
    EnvironmentEdit::RetryDeletion(id).apply(&mut saved, None);
    assert_eq!(saved.generation, 3);
    assert_eq!(saved.environments, [changed]);
}

#[test]
fn an_accepted_deletion_marks_only_that_environment() {
    let mut saved = application(vec![
        environment("env-00000001", "alpha", EnvironmentSource::Saved),
        environment("env-00000002", "beta", EnvironmentSource::Saved),
    ]);
    EnvironmentEdit::Delete("env-00000002".into()).apply(&mut saved, None);
    let deleting: Vec<_> = saved
        .environments
        .iter()
        .map(|env| env.delete_intent)
        .collect();
    assert_eq!(deleting, [false, true]);
    assert_eq!(saved.generation, 4);
}

#[test]
fn a_blank_commit_follows_the_branch_head_and_whitespace_is_ignored() {
    assert_eq!(
        building(" main ", "  ").branch.tracked(),
        Ok(branch("main", None))
    );
    assert_eq!(
        building("main", &format!(" {COMMIT}\n")).branch.tracked(),
        Ok(branch("main", Some(COMMIT)))
    );
    building("main", "abc")
        .branch
        .tracked()
        .expect_err("`tracked` should be rejected");
    building("", "")
        .branch
        .tracked()
        .expect_err("`tracked` should be rejected");
}

#[test]
fn a_branch_form_changes_only_when_it_names_another_branch_or_commit() {
    let saved = EnvironmentSource::Branch(branch("release", None));
    let change = |draft: SourceDraft| draft.change("env-00000001".into(), Some(&saved), true);
    assert_eq!(change(building(" release", "")), None);
    assert_eq!(
        change(building("release", COMMIT)),
        Some(Ok(EnvironmentEdit::Branch {
            id: "env-00000001".into(),
            branch: branch("release", Some(COMMIT)),
        }))
    );
    assert!(matches!(change(building("release", "abc")), Some(Err(_))));
}

#[test]
fn a_promoted_environment_changes_its_source_or_builds_again() {
    let saved = promoted("env-00000002");
    let id = || String::from("env-00000001");
    let promoting = |from: &str| SourceDraft {
        promote_from: from.into(),
        ..building("main", "")
    };
    assert_eq!(
        promoting("env-00000002").change(id(), Some(&saved), true),
        None
    );
    assert_eq!(
        promoting("env-00000003").change(id(), Some(&saved), false),
        Some(Ok(EnvironmentEdit::Source {
            id: id(),
            promote_from: Some("env-00000003".into()),
        }))
    );
    assert_eq!(
        building("main", "").change(id(), Some(&saved), true),
        Some(Ok(EnvironmentEdit::Branch {
            id: id(),
            branch: branch("main", None),
        }))
    );
    assert_eq!(
        building("", "").change(id(), Some(&saved), false),
        Some(Ok(EnvironmentEdit::Source {
            id: id(),
            promote_from: None,
        }))
    );
}

#[test]
fn an_untouched_form_reports_no_change_so_it_never_blocks_deploys() {
    let default = BranchDraft {
        branch: "main".into(),
        commit: String::new(),
    };
    let sources = [
        (
            EnvironmentSource::Branch(branch("release", Some(COMMIT))),
            true,
        ),
        (promoted("env-00000002"), true),
        (EnvironmentSource::Saved, false),
        (promoted("env-00000002"), false),
    ];
    for (source, repository) in sources {
        let draft = SourceDraft::saved(Some(&source), default.clone());
        assert_eq!(
            draft.change("env-00000001".into(), Some(&source), repository),
            None,
            "{source}"
        );
    }
}
