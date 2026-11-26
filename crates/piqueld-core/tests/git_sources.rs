//! Git manifest validation and immutable resolution contracts.
use piqueld_core::edit::ApplicationEdit;
use piqueld_core::manifest::{
    GitRepository, SourceRepository, ValidatedBuild as Build, ValidatedSource as Source,
};
use piqueld_core::resource::{ResolvedSource, Sha256Digest};
use piqueld_core::{
    ApiErrorCode, ApplicationId, EnvironmentId, InstanceId, ResolutionSet, compile_application,
    parse_json,
};

fn manifest() -> serde_json::Value {
    serde_json::json!({
        "api_version": "piqueld.dev/v1alpha1", "kind": "Application", "metadata": {"name": "example"},
        "spec": {"services": [{"name": "web", "source": {"type": "git", "repository": {"url": "https://example.com/app.git", "branch": "main"}, "build": {"type": "docker", "dockerfile": "./Dockerfile"}}}]}
    })
}

#[test]
fn git_sources_validate_paths_and_explicit_build_backend() {
    let valid = manifest();
    let parsed = parse_json(&valid.to_string())
        .expect("the json parses")
        .normalize(ApplicationId::parse("example-id").expect("the application id is valid"));
    let Source::Git {
        build: Build::Docker { context, .. },
        ..
    } = &parsed.spec().services[0].source
    else {
        panic!("Git source expected")
    };
    assert_eq!(context, ".");
    for (field, value) in [
        ("dockerfile", "../Dockerfile"),
        ("context", "/etc"),
        ("dockerfile", ".git/config"),
        ("type", "auto"),
    ] {
        let mut invalid = valid.clone();
        invalid["spec"]["services"][0]["source"]["build"][field] = value.into();
        assert!(
            parse_json(&invalid.to_string()).is_err(),
            "{field}: {value}"
        );
    }
    for (field, value) in [
        ("branch", "--upload-pack=bad"),
        ("branch", "@"),
        ("branch", "main~1"),
        ("commit", "HEAD"),
        ("url", "--help"),
        ("url", "https://user:token@example.com/repo.git"),
        ("url", "HTTPS://token@example.com/repo.git"),
        ("url", "ssh://git:password@example.com/repo.git"),
    ] {
        let mut invalid = valid.clone();
        invalid["spec"]["services"][0]["source"]["repository"][field] = value.into();
        assert!(
            parse_json(&invalid.to_string()).is_err(),
            "{field}: {value}"
        );
    }
}

#[test]
fn git_resolution_retains_commit_and_local_image_and_rejects_mismatched_inputs() {
    let app = parse_json(&manifest().to_string())
        .expect("the json parses")
        .normalize(ApplicationId::parse("example-id").expect("the application id is valid"));
    let mut resolutions = ResolutionSet::default();
    let image_id = Sha256Digest::parse(format!("sha256:{}", "a".repeat(64)))
        .expect("the sha256 digest is valid");
    resolutions.sources.insert(
        piqueld_core::ServiceName::parse("web").expect("the service name is valid"),
        ResolvedSource::Git {
            requested: app.spec().services[0].source.clone(),
            commit: piqueld_core::GitCommit::parse("b".repeat(40)).expect("the commit is valid"),
            image_id: image_id.clone().into(),
        },
    );
    let instance = InstanceId::parse("test").expect("the instance id is valid");
    let environment = EnvironmentId::parse("example-id").expect("the environment id is valid");
    let resolved = compile_application(&app, &environment, instance.clone(), &resolutions)
        .expect("the application compiles");
    assert_eq!(resolved.services[0].image.as_str(), image_id.as_str());
    assert_eq!(resolved.reusable_resolutions(&app), resolutions);
    let mut changed = app.to_manifest();
    let piqueld_core::manifest::Source::Git {
        repository: SourceRepository::Git(repository),
        ..
    } = &mut changed.spec.services[0].source
    else {
        panic!("the service builds from Git")
    };
    repository.commit = Some("c".repeat(40));
    let changed = changed
        .validate()
        .expect("the manifest validates")
        .normalize(app.id().clone());
    compile_application(&changed, &environment, instance, &resolutions)
        .expect_err("`compile_application` should be rejected");
    assert!(resolved.reusable_resolutions(&changed).sources.is_empty());
}

#[test]
fn self_sources_build_from_the_manifest_revision() {
    let mut valid = manifest();
    valid["spec"]["services"][0]["source"]["repository"] = "self".into();
    let error = parse_json(&valid.to_string()).expect_err("the json is rejected");
    assert_eq!(error.0[0].code, "manifest_repository_required");
    valid["spec"]["manifest"] = serde_json::json!({
        "path": "app.json",
        "repository": {"url": "https://example.com/app.git", "branch": "main"},
    });
    let app = parse_json(&valid.to_string())
        .expect("the json parses")
        .normalize(ApplicationId::parse("example-id").expect("the application id is valid"));
    let manifest_repository = |app: &piqueld_core::NormalizedApplication| {
        app.spec()
            .manifest
            .as_ref()
            .expect("the value is present")
            .repository
            .clone()
    };
    let service_repository = |app: &piqueld_core::NormalizedApplication| {
        let Source::Git { repository, .. } = &app.spec().services[0].source else {
            panic!("the service builds from Git")
        };
        repository.clone()
    };

    let commit = "b".repeat(40);
    let pinned = app.clone().pin_manifest_sources(&commit);
    assert_eq!(
        service_repository(&pinned),
        SourceRepository::Git(GitRepository {
            commit: Some(commit),
            ..manifest_repository(&app)
        })
    );
    assert_eq!(pinned.spec().manifest, app.spec().manifest);

    let mut disconnected = app.to_manifest();
    ApplicationEdit::Repository(None)
        .apply(&mut disconnected)
        .expect("the change applies");
    let disconnected = disconnected
        .validate()
        .expect("the manifest validates")
        .normalize(app.id().clone());
    assert_eq!(
        service_repository(&disconnected),
        SourceRepository::Git(manifest_repository(&app))
    );
}

#[test]
fn docker_build_arguments_and_target_validate_and_are_part_of_source_identity() {
    let mut valid = manifest();
    valid["spec"]["services"][0]["source"]["build"]["args"] =
        serde_json::json!({"VITE_AUTH_ORIGIN": "https://auth.example.com"});
    valid["spec"]["services"][0]["source"]["build"]["target"] = "runtime".into();
    let app = parse_json(&valid.to_string())
        .expect("the json parses")
        .normalize(ApplicationId::parse("example-id").expect("the application id is valid"));
    for (field, value, code) in [
        (
            "args",
            serde_json::json!({"1BAD": "x"}),
            ApiErrorCode::BuildArgNameInvalid,
        ),
        (
            "args",
            serde_json::json!({"OK": "a\0b"}),
            ApiErrorCode::BuildArgValueInvalid,
        ),
        (
            "args",
            (0..5)
                .map(|index| (format!("A{index}"), "x".repeat(60_000)))
                .collect(),
            ApiErrorCode::BuildArgTotalExcessive,
        ),
        ("target", "--push".into(), ApiErrorCode::BuildTargetInvalid),
        ("target", "".into(), ApiErrorCode::BuildTargetInvalid),
    ] {
        let mut invalid = valid.clone();
        invalid["spec"]["services"][0]["source"]["build"][field] = value.clone();
        let errors = parse_json(&invalid.to_string()).expect_err("the json is rejected");
        assert!(
            errors.0.iter().any(|error| error.code == code),
            "{field}: {value}"
        );
    }

    let resolutions = ResolutionSet {
        sources: [(
            piqueld_core::ServiceName::parse("web").expect("the service name is valid"),
            ResolvedSource::Git {
                requested: app.spec().services[0].source.clone(),
                commit: piqueld_core::GitCommit::parse("b".repeat(40))
                    .expect("the commit is valid"),
                image_id: Sha256Digest::parse(format!("sha256:{}", "a".repeat(64)))
                    .expect("the sha256 digest is valid")
                    .into(),
            },
        )]
        .into(),
        ..ResolutionSet::default()
    };
    let environment = EnvironmentId::parse("example-id").expect("the environment id is valid");
    let resolved = compile_application(
        &app,
        &environment,
        InstanceId::parse("test").expect("the instance id is valid"),
        &resolutions,
    )
    .expect("the application compiles");
    assert_eq!(resolved.reusable_resolutions(&app), resolutions);
    for field in ["args", "target"] {
        let mut changed = valid.clone();
        changed["spec"]["services"][0]["source"]["build"]
            .as_object_mut()
            .expect("the value is an object")
            .remove(field);
        let changed = parse_json(&changed.to_string())
            .expect("the json parses")
            .normalize(app.id().clone());
        assert!(
            resolved.reusable_resolutions(&changed).sources.is_empty(),
            "{field}"
        );
    }
}
