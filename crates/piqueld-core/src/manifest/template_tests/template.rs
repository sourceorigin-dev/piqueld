use super::*;

#[test]
fn hostnames_render_per_environment_before_deployment() {
    let template = template(&format!(
        "{VARIABLES}\n[[spec.routes]]\nhostname = \"${{{{ vars.domain }}}}\"\nservice = \"web\"\nport = 80\n[[spec.routes]]\nhostname = \"${{{{ deployment.id }}}}.piquel.fr\"\nservice = \"web\"\nport = 80\n"
    ));
    let hostnames = |environment: &str| {
        template
            .hostnames(&super::environment(environment))
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
    };
    assert_eq!(hostnames("staging"), ["staging.piquel.fr"]);
    assert_eq!(hostnames("production"), ["piquel.fr"]);
}

#[test]
fn literal_rendering_conversion_hash_and_exports_match_the_domain_model() {
    for manifest in [base(), complete()] {
        let validated = manifest
            .clone()
            .validate_template()
            .expect("the template validates");
        assert_eq!(validated.name().as_str(), "notes");
        assert_eq!(validated.spec(), &manifest.spec);
        let normalized = manifest
            .clone()
            .validate()
            .expect("the manifest validates")
            .normalize(id());
        assert_eq!(
            validated
                .clone()
                .into_literal()
                .expect("the template is literal")
                .normalize(id()),
            normalized
        );
        let template = validated.normalize(id());
        assert_eq!(template.id(), &id());
        assert_eq!(template.metadata(), normalized.metadata());
        assert_eq!(template.spec(), &manifest.spec);
        assert_eq!(ApplicationTemplate::from(&normalized), template);
        let canonical = normalized.canonical_json().expect("the json canonicalizes");
        assert_eq!(
            template.canonical_json().expect("the json canonicalizes"),
            canonical
        );
        assert_eq!(
            serde_json::to_string(&template).expect("the value serializes"),
            canonical
        );
        assert_eq!(
            serde_json::from_str::<ApplicationTemplate>(&canonical)
                .expect("the application template deserializes"),
            template
        );
        assert_eq!(template.spec_hash(), normalized.spec_hash());
        assert_eq!(
            template
                .render(&saved())
                .expect("the template renders")
                .application,
            normalized
        );
        assert_eq!(
            parse_toml(&template.export_toml().expect("the manifest exports"))
                .expect("the toml parses")
                .normalize(id()),
            normalized
        );
        assert_eq!(
            parse_template_json(
                &serde_json::to_string(&template.to_manifest()).expect("the value serializes")
            )
            .expect("the template json parses")
            .normalize(id()),
            template
        );
    }
}

#[test]
fn normalization_orders_unordered_collections_but_preserves_process_order() {
    let mut manifest = complete();
    let mut third = manifest.spec.services[0].clone();
    third.name = "c-image".into();
    manifest.spec.services.push(third);
    manifest.spec.services[0].depends_on = vec!["c-image".into(), "b-build".into()];
    manifest.spec.volumes = vec![Volume { name: "z".into() }, Volume { name: "a".into() }];
    manifest.spec.services[0].mounts = vec![
        Mount {
            volume: "z".into(),
            target: "/z".into(),
            read_only: true,
        },
        Mount {
            volume: "a".into(),
            target: "/a".into(),
            read_only: true,
        },
    ];
    manifest.spec.services[0].secrets = vec![
        SecretMount {
            name: "z".into(),
            target: "/run/secrets/z".into(),
        },
        SecretMount {
            name: "a".into(),
            target: "/run/secrets/a".into(),
        },
    ];
    manifest.spec.secrets = serde_json::from_value(json!([
        {"name":"z", "generate":{"type":"random", "bytes":32, "encoding":"hex"}},
        {"name":"a", "generate":{"type":"random", "bytes":32, "encoding":"hex"}}
    ]))
    .expect("the input deserializes");
    let first = normalize(manifest.clone());
    manifest.spec.services.reverse();
    manifest.spec.routes.reverse();
    manifest.spec.volumes.reverse();
    manifest.spec.secrets.reverse();
    for service in &mut manifest.spec.services {
        service.mounts.reverse();
        service.secrets.reverse();
        service.depends_on.reverse();
    }
    let second = normalize(manifest.clone());
    assert_eq!(first, second);
    assert_eq!(
        first.canonical_json().expect("the json canonicalizes"),
        second.canonical_json().expect("the json canonicalizes")
    );
    assert_eq!(first.spec_hash(), second.spec_hash());
    assert_eq!(
        first.spec_hash(),
        manifest
            .validate()
            .expect("the manifest validates")
            .normalize(id())
            .spec_hash()
    );
    assert_eq!(
        first.spec().services[0].command,
        [Template::from("sh"), Template::from("-c")]
    );
    assert_eq!(first.spec().services[0].depends_on, ["b-build", "c-image"]);
    assert_eq!(normalize(first.to_manifest()), first);
}

#[test]
fn into_literal_reports_each_unresolved_slot_in_path_order() {
    let wire = serde_json::to_value(complete()).expect("the value serializes");
    let mut input = wire.clone();
    input["spec"]["variables"] = json!({"value":"unused"});
    for &(pointer, _) in FIELDS {
        *input.pointer_mut(pointer).expect("the pointer exists") = json!("${{vars.value}}");
    }
    let validated = parse_template_json(&input.to_string()).expect("the template json parses");
    let errors = validated
        .into_literal()
        .expect_err("the template is not literal");
    let mut expected: Vec<_> = FIELDS
        .iter()
        .map(|&(_, path)| (ApiErrorCode::VariableUnresolved, path))
        .collect();
    expected.sort_by_key(|&(_, path)| path);
    assert_eq!(codes_and_paths(&errors), expected);
    assert!(
        errors
            .0
            .iter()
            .all(|error| error.message == "references must be rendered for an environment first")
    );
}

#[test]
fn template_export_round_trips_overrides_escapes_and_identity() {
    let original = template(VARIABLES);
    let id = ApplicationId::parse("app-renamed-02").expect("the application id is valid");
    let name = ApplicationName::parse("renamed").expect("the application name is valid");
    let changed = original.clone().with_id(id.clone()).with_name(name.clone());
    assert_eq!(changed.id(), &id);
    assert_eq!(&changed.metadata().name, &name);
    assert_eq!(changed.spec(), original.spec());
    assert_eq!(changed.spec_hash(), original.spec_hash());
    assert_ne!(
        changed.canonical_json().expect("the json canonicalizes"),
        original.canonical_json().expect("the json canonicalizes")
    );
    assert_eq!(changed.to_manifest().metadata.name, "renamed");
    assert!(
        changed.configures(
            &EnvironmentName::parse("production").expect("the environment name is valid")
        )
    );
    assert!(
        changed
            .configures(&EnvironmentName::parse("staging").expect("the environment name is valid"))
    );
    assert!(
        !changed
            .configures(&EnvironmentName::parse("other").expect("the environment name is valid"))
    );
    let exported = changed.export_toml().expect("the manifest exports");
    assert!(exported.contains("$${{ vars.tag }}"));
    assert_eq!(
        parse_template_toml(&exported)
            .expect("the template toml parses")
            .normalize(id),
        changed
    );
    assert_eq!(
        serde_json::from_str::<ApplicationTemplate>(
            &changed.canonical_json().expect("the json canonicalizes")
        )
        .expect("the application template deserializes"),
        changed
    );
    let mut edited = changed.to_manifest();
    edited
        .spec
        .variables
        .insert("tag".into(), Variable::String("next".into()));
    assert_ne!(normalize(edited).spec_hash(), changed.spec_hash());
    assert_eq!(
        render(&changed, "production")
            .expect("the template renders")
            .values["app.name"],
        VariableValue::String("renamed".into())
    );
}

#[test]
fn revision_overrides_validate_and_do_not_change_the_spec_hash() {
    let mut manifest = base();
    manifest.spec.manifest = Some(repository());
    let template = normalize(manifest);
    let sha = "a".repeat(40);
    let pinned = template
        .clone()
        .with_manifest_revision(&ManifestRevision::Commit(sha.clone()))
        .expect("the manifest revision applies");
    assert_eq!(
        pinned
            .spec()
            .manifest
            .as_ref()
            .expect("the value is present")
            .repository
            .commit,
        Some(sha)
    );
    let branched = pinned
        .clone()
        .with_manifest_revision(&ManifestRevision::Branch("release".into()))
        .expect("the manifest revision applies");
    let repository = &branched
        .spec()
        .manifest
        .as_ref()
        .expect("the value is present")
        .repository;
    assert_eq!(repository.branch, "release");
    assert_eq!(repository.commit, None);
    assert_eq!(pinned.spec_hash(), template.spec_hash());
    assert_eq!(branched.spec_hash(), template.spec_hash());
    assert_eq!(normalize(base()).spec_hash(), template.spec_hash());
    for revision in [
        ManifestRevision::Branch("-bad".into()),
        ManifestRevision::Commit("short".into()),
    ] {
        let errors = template
            .clone()
            .with_manifest_revision(&revision)
            .expect_err("the revision is rejected");
        assert!(
            errors
                .0
                .iter()
                .all(|error| error.path == "spec.manifest.repository")
        );
        assert_eq!(errors.0.len(), 1);
    }
    let errors = normalize(base())
        .with_manifest_revision(&ManifestRevision::Branch("main".into()))
        .expect_err("the revision is rejected");
    assert_eq!(
        codes_and_paths(&errors),
        [(
            ApiErrorCode::ManifestRepositoryRequired,
            "spec.manifest.repository"
        )]
    );
}

#[test]
fn deserialization_revalidates_and_normalizes_saved_templates() {
    let template = normalize(complete());
    let mut wire = serde_json::to_value(&template).expect("the value serializes");
    wire["spec"]["services"]
        .as_array_mut()
        .expect("the value is an array")
        .reverse();
    wire["spec"]["routes"]
        .as_array_mut()
        .expect("the value is an array")
        .reverse();
    assert_eq!(
        serde_json::from_value::<ApplicationTemplate>(wire.clone())
            .expect("the application template deserializes"),
        template
    );
    for (pointer, value, diagnostic) in [
        ("/id", json!(""), "application"),
        (
            "/api_version",
            json!("bad"),
            ApiErrorCode::ApiVersionUnsupported.as_str(),
        ),
        (
            "/kind",
            json!("bad"),
            ApiErrorCode::KindUnsupported.as_str(),
        ),
        (
            "/metadata/name",
            json!("BAD"),
            ApiErrorCode::NameInvalid.as_str(),
        ),
        (
            "/spec/services/0/replicas",
            json!(0),
            ApiErrorCode::ReplicasOutOfRange.as_str(),
        ),
        (
            "/spec/services/0/replicas",
            json!("${{vars.missing}}"),
            ApiErrorCode::VariableUndeclared.as_str(),
        ),
    ] {
        let mut invalid = wire.clone();
        *invalid.pointer_mut(pointer).expect("the pointer exists") = value;
        let error = serde_json::from_value::<ApplicationTemplate>(invalid)
            .expect_err("the application template is rejected")
            .to_string();
        assert!(error.contains(diagnostic), "{pointer}: {error}");
    }
    wire["unexpected"] = json!(true);
    assert!(
        serde_json::from_value::<ApplicationTemplate>(wire)
            .expect_err("the application template is rejected")
            .to_string()
            .contains("unknown field")
    );
}

#[test]
fn saved_hostnames_skip_missing_invalid_and_deployment_only_values() {
    let mut manifest = base();
    manifest.spec.manifest = Some(repository());
    manifest.spec.variables.insert(
        "domain".into(),
        Variable::String("UPPER.Example.COM.".into()),
    );
    manifest
        .spec
        .variables
        .insert("invalid".into(), Variable::String("not a hostname".into()));
    manifest.spec.environments.insert(
        "staging".into(),
        EnvironmentConfig {
            visibility: Visibility::Public,
            variables: std::collections::BTreeMap::from([(
                "missing".into(),
                Variable::String("stage.example.com".into()),
            )]),
            ..EnvironmentConfig::default()
        },
    );
    manifest.spec.routes = [
        "${{vars.domain}}",
        "${{vars.invalid}}",
        "${{vars.missing}}",
        "${{deployment.id}}.example.com",
        "${{git.sha}}.example.com",
        "literal.example.com",
    ]
    .map(|hostname| Route {
        hostname: hostname.into(),
        name: None,
        visibility: Visibility::Private,
        service: Some("web".into()),
        port: Some(80),
        redirect: None,
    })
    .into();
    let template = normalize(manifest);
    let names: Vec<_> = template
        .hostnames(&environment("production"))
        .into_iter()
        .map(String::from)
        .collect();
    assert_eq!(names, ["upper.example.com", "literal.example.com"]);
}

/// A name read alongside `deployment.id` or `git.*` still changes the
/// rendering, so the rename is not alike.
#[test]
fn renders_like_compares_names_read_with_deployment_values() {
    let renamed = |hostname: &str| {
        let mut manifest = base();
        manifest.spec.manifest = Some(repository());
        manifest.spec.routes = vec![Route {
            hostname: hostname.into(),
            name: None,
            visibility: Visibility::Private,
            service: Some("web".into()),
            port: Some(80),
            redirect: None,
        }];
        let template = normalize(manifest);
        let target = environment("production");
        let renamed = template
            .clone()
            .with_name(ApplicationName::parse("renamed").expect("the application name is valid"));
        template.renders_like(&target, &renamed, &target)
    };
    assert!(renamed("${{ deployment.id }}-${{ git.sha }}.example.com"));
    assert!(!renamed("${{ app.name }}-${{ deployment.id }}.example.com"));
    assert!(!renamed("${{ app.name }}-${{ git.branch }}.example.com"));
    assert!(!renamed("${{ deployment.id }} invalid.example.com"));
}

#[test]
fn validation_errors_sort_by_path_then_code_stably() {
    let error =
        |path: &str, code: ApiErrorCode, message: &str| ValidationError::new(code, path, message);
    let (first, last) = (ApiErrorCode::ImageInvalid, ApiErrorCode::NameInvalid);
    let errors = ValidationErrors::sorted(vec![
        error("z", first, "last"),
        error("a", last, "third"),
        error("a", first, "first"),
        error("a", first, "second"),
    ]);
    assert_eq!(
        codes_and_paths(&errors),
        [(first, "a"), (first, "a"), (last, "a"), (first, "z")]
    );
    assert_eq!(errors.0[0].message, "first");
    assert_eq!(errors.0[1].message, "second");
    assert_eq!(ValidationErrors::sorted(Vec::new()).0, Vec::new());
}

#[test]
fn into_literal_preserves_text_escapes_and_rejects_unresolved_typed_escapes() {
    let mut manifest = base();
    manifest.spec.services[0]
        .environment
        .insert("VALUE".into(), "$${{vars.literal}}".into());
    let literal = manifest
        .clone()
        .validate_template()
        .expect("the template validates")
        .into_literal()
        .expect("the template is literal");
    assert_eq!(
        literal.spec().services[0].environment["VALUE"],
        "${{vars.literal}}"
    );
    let normalized = literal.normalize(id());
    let converted = ApplicationTemplate::from(&normalized);
    assert_eq!(
        converted.spec().services[0].environment["VALUE"],
        "$${{vars.literal}}"
    );
    assert_eq!(
        converted
            .render(&saved())
            .expect("the template renders")
            .application,
        normalized
    );
    manifest.spec.services[0].replicas = Typed::Template("$${{vars.literal}}".into());
    let errors = manifest
        .validate_template()
        .expect("the template validates")
        .into_literal()
        .expect_err("the template is not literal");
    assert_eq!(
        codes_and_paths(&errors),
        [(
            ApiErrorCode::VariableUnresolved,
            "spec.services[0].replicas"
        )]
    );
}

/// A route's name is literal: checked once, and the same in every environment
/// whatever hostname it renders there.
#[test]
fn route_names_are_validated_unique_and_stable_across_environments() {
    let route = |name: &str, hostname: &str| {
        format!(
            "[[spec.routes]]\nname = \"{name}\"\nhostname = \"{hostname}\"\nservice = \"web\"\nport = 80\n"
        )
    };
    let template = template(&format!(
        "{VARIABLES}\n{}{}",
        route("web", "${{ vars.domain }}"),
        route("api", "api.${{ vars.domain }}")
    ));
    for (environment, hostname) in [
        ("staging", "staging.piquel.fr"),
        ("production", "piquel.fr"),
    ] {
        let rendering = render(&template, environment).expect("the template renders");
        let route = rendering
            .application
            .spec()
            .routes
            .iter()
            .find(|route| {
                route
                    .name
                    .as_ref()
                    .is_some_and(|name| name.as_str() == "web")
            })
            .expect("the item is found");
        assert_eq!(route.hostname.as_str(), hostname);
    }

    for (routes, code, path) in [
        (
            route("Web!", "a.piquel.fr"),
            ApiErrorCode::NameInvalid,
            "spec.routes[0].name",
        ),
        (
            route("${{ vars.tag }}", "a.piquel.fr"),
            ApiErrorCode::VariableNotAllowed,
            "spec.routes[0].name",
        ),
        (
            format!(
                "{}{}",
                route("web", "a.piquel.fr"),
                route("web", "b.piquel.fr")
            ),
            ApiErrorCode::RouteNameDuplicate,
            "spec.routes[1].name",
        ),
    ] {
        let errors = parse_template_toml(&manifest(&format!("{VARIABLES}\n{routes}")))
            .expect_err("the manifest is rejected");
        assert_eq!(codes_and_paths(&errors), [(code, path)], "{routes}");
    }
}
