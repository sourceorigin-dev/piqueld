use super::*;

#[test]
fn invalid_references_fail_validation_with_their_field_paths() {
    let text = manifest(
        "[spec.variables]\nweb_replicas = 1\n[[spec.routes]]\nhostname = \"${{ app.domain }}\"\nservice = \"web\"\nport = 80\n",
    )
    .replace("${{ env.name }}", "${{ secrets.token }} ${{ git.sha }}")
    .replace("name = \"web\"", "name = \"${{ vars.tag }}\"");
    let errors = parse_template_toml(&text).unwrap_err();
    assert_eq!(
        codes_and_paths(&errors),
        [
            (ApiErrorCode::VariableUnknown, "spec.routes[0].hostname"),
            (ApiErrorCode::RouteServiceMissing, "spec.routes[0].service"),
            (
                ApiErrorCode::VariableReserved,
                "spec.services[0].environment.GREETING"
            ),
            (ApiErrorCode::VariableNotAllowed, "spec.services[0].name"),
            (
                ApiErrorCode::VariableUndeclared,
                "spec.services[0].source.image"
            ),
        ]
    );
    let text = manifest("[spec.variables]\nweb_replicas = 1\ntag = \"${{ vars.web_replicas }}\"\n")
        .replace("${{ env.name }}", "${{ git.sha }}");
    assert_eq!(
        codes_and_paths(&parse_template_toml(&text).unwrap_err()),
        [
            (
                ApiErrorCode::VariableUnavailable,
                "spec.services[0].environment.GREETING"
            ),
            (ApiErrorCode::VariableNotAllowed, "spec.variables.tag"),
        ]
    );
    let errors = parse_toml(&manifest(VARIABLES)).unwrap_err();
    assert!(
        errors
            .0
            .iter()
            .all(|error| error.code == ApiErrorCode::VariableUnresolved),
        "{errors:?}"
    );
}

/// Oversized keys and repeated bad references cannot multiply diagnostics.
#[test]
fn diagnostics_stay_bounded_for_oversized_names_and_repeated_references() {
    let name = "n".repeat(10_000);
    let references = "${{ vars.missing }}".repeat(1_000);
    let text = manifest(&format!(
        "[spec.variables]\nweb_replicas = 1\ntag = \"stable\"\n{name} = \"{references}\"\n"
    ))
    .replace("hello ${{ env.name }}", &references);
    let errors = parse_template_toml(&text).unwrap_err();
    assert_eq!(errors.0.len(), 3, "{:?}", codes_and_paths(&errors));
    assert!(errors.0.iter().all(|error| error.path.len() < 300));
}

#[test]
fn declarations_validate_names_values_and_only_the_first_bad_reference() {
    for (name, value, code) in [
        (
            "1bad",
            Variable::Integer(1),
            ApiErrorCode::VariableNameInvalid,
        ),
        (
            "good",
            Variable::String("${{".into()),
            ApiErrorCode::TemplateInvalid,
        ),
        (
            "good",
            Variable::String("${{secrets.key}}".into()),
            ApiErrorCode::VariableReserved,
        ),
        (
            "good",
            Variable::String("${{other.key}}".into()),
            ApiErrorCode::VariableNamespaceUnknown,
        ),
        (
            "good",
            Variable::String("${{env.region}}".into()),
            ApiErrorCode::VariableUnknown,
        ),
        (
            "good",
            Variable::String("text ${{vars.first}} ${{vars.second}}".into()),
            ApiErrorCode::VariableNotAllowed,
        ),
        (
            "good",
            Variable::String("${{git.branch}}${{git.sha}}".into()),
            ApiErrorCode::VariableUnavailable,
        ),
    ] {
        for override_value in [false, true] {
            let mut spec = base().spec;
            let path = if override_value {
                spec.environments.insert(
                    "production".into(),
                    EnvironmentConfig {
                        visibility: Visibility::Public,
                        variables: std::collections::BTreeMap::from([(name.into(), value.clone())]),
                        ..EnvironmentConfig::default()
                    },
                );
                format!("spec.environments.production.variables.{name}")
            } else {
                spec.variables.insert(name.into(), value.clone());
                format!("spec.variables.{name}")
            };
            let mut errors = Vec::new();
            spec.check_variables(&mut errors);
            assert_eq!(
                codes_and_paths(&ValidationErrors(errors)),
                [(code, path.as_str())]
            );
        }
    }
    let mut manifest = base();
    manifest.spec.manifest = Some(repository());
    for (name, value) in [
        ("_a-9", Variable::Boolean(false)),
        ("n", Variable::Integer(-1)),
        (
            "git",
            Variable::String("${{git.branch}}/${{git.sha}}".into()),
        ),
        ("escaped", Variable::String("$${{vars.a}}".into())),
    ] {
        manifest.spec.variables.insert(name.into(), value);
    }
    manifest
        .validate_template()
        .expect("`validate_template` should succeed");
}

#[test]
fn variable_limits_accept_boundaries_and_reject_one_more() {
    for (count, invalid) in [(128, false), (129, true)] {
        for overrides in [false, true] {
            let mut spec = base().spec;
            let variables = (0..count)
                .map(|i| (format!("v{i}"), Variable::Integer(i)))
                .collect();
            if overrides {
                spec.environments.insert(
                    "production".into(),
                    EnvironmentConfig {
                        visibility: Visibility::Public,
                        variables,
                        ..EnvironmentConfig::default()
                    },
                );
            } else {
                spec.variables = variables;
            }
            let mut errors = Vec::new();
            spec.check_variables(&mut errors);
            assert_eq!(
                codes_and_paths(&ValidationErrors(errors)),
                if invalid {
                    vec![(ApiErrorCode::VariableCountExcessive, "spec.variables")]
                } else {
                    vec![]
                }
            );
        }
    }
    for (count, invalid) in [(64, false), (65, true)] {
        let mut spec = base().spec;
        spec.environments = (0..count)
            .map(|i| (format!("env-{i}"), EnvironmentConfig::default()))
            .collect();
        let mut errors = Vec::new();
        spec.check_variables(&mut errors);
        assert_eq!(
            codes_and_paths(&ValidationErrors(errors)),
            if invalid {
                vec![(ApiErrorCode::VariableCountExcessive, "spec.environments")]
            } else {
                vec![]
            }
        );
    }
    for (value, invalid) in [
        ("é".repeat(32768), false),
        (format!("{}x", "é".repeat(32768)), true),
    ] {
        let mut spec = base().spec;
        spec.variables
            .insert("text".into(), Variable::String(value.into()));
        let mut errors = Vec::new();
        spec.check_variables(&mut errors);
        assert_eq!(
            codes_and_paths(&ValidationErrors(errors)),
            if invalid {
                vec![(ApiErrorCode::VariableValueExcessive, "spec.variables.text")]
            } else {
                vec![]
            }
        );
    }
}

#[test]
fn unicode_diagnostic_keys_truncate_on_byte_boundaries_and_echo_on_characters() {
    let key = "é".repeat(200);
    let mut spec = complete().spec;
    spec.variables.insert(key.clone(), Variable::Integer(1));
    spec.environments.insert(
        key.clone(),
        EnvironmentConfig {
            visibility: Visibility::Public,
            variables: std::collections::BTreeMap::from([(key.clone(), Variable::Integer(1))]),
            ..EnvironmentConfig::default()
        },
    );
    spec.services[0]
        .environment
        .insert(key.clone(), "${{vars.missing}}".into());
    if let Source::Git {
        build: Build::Docker { args, .. },
        ..
    } = &mut spec.services[1].source
    {
        args.insert(key, "${{vars.missing}}".into());
    }
    let mut errors = Vec::new();
    spec.check_variables(&mut errors);
    errors.extend(spec.declared_environments().unwrap_err().0);
    let errors = ValidationErrors::sorted(errors);
    let short = "é".repeat(127);
    assert_eq!(
        codes_and_paths(&errors),
        [
            (ApiErrorCode::NameInvalid, "spec.environments"),
            (
                ApiErrorCode::VariableNameInvalid,
                format!("spec.environments.{short}.variables.{short}").as_str()
            ),
            (
                ApiErrorCode::VariableUndeclared,
                format!("spec.services[0].environment.{short}").as_str()
            ),
            (
                ApiErrorCode::VariableUndeclared,
                format!("spec.services[1].source.build.args.{short}").as_str()
            ),
            (
                ApiErrorCode::VariableNameInvalid,
                format!("spec.variables.{short}").as_str()
            ),
        ]
    );
    assert!(
        errors.0[0]
            .message
            .starts_with(&format!("[spec.environments.{}]:", "é".repeat(64)))
    );
}

#[test]
fn references_check_declarations_and_repository_availability_before_rendering() {
    for (reference, code) in [
        ("vars.missing", ApiErrorCode::VariableUndeclared),
        ("git.branch", ApiErrorCode::VariableUnavailable),
        ("git.sha", ApiErrorCode::VariableUnavailable),
    ] {
        let mut manifest = base();
        manifest.spec.services[0].command =
            vec![format!("text ${{{{{reference}}}}}${{{{vars.second}}}}").into()];
        let errors = manifest.validate_template().unwrap_err();
        assert_eq!(
            codes_and_paths(&errors),
            [(code, "spec.services[0].command[0]")]
        );
        if reference.starts_with("git.") {
            assert_eq!(
                errors.0[0].message,
                format!("{reference} is only set when spec.manifest is configured")
            );
        }
    }
    let mut manifest = base();
    manifest.spec.environments.insert(
        "staging".into(),
        EnvironmentConfig {
            visibility: Visibility::Public,
            variables: std::collections::BTreeMap::from([("only".into(), Variable::Boolean(true))]),
            ..EnvironmentConfig::default()
        },
    );
    manifest.spec.services[0].command = vec!["${{vars.only}}".into()];
    manifest
        .validate_template()
        .expect("`validate_template` should succeed");
}

#[test]
fn plain_names_paths_and_keys_reject_even_escaped_references() {
    let mut input = serde_json::to_value(complete()).unwrap();
    input["spec"]["volumes"] = json!([{"name":"data"}]);
    input["spec"]["services"][0]["mounts"] = json!([{"volume":"data", "target":"/data"}]);
    input["spec"]["services"][0]["secrets"] = json!([{"name":"key", "target":"/run/secrets/key"}]);
    input["spec"]["secrets"] =
        json!([{"name":"key", "generate":{"type":"random", "bytes":32, "encoding":"hex"}}]);
    input["spec"]["services"][1]["source"]["repository"] =
        serde_json::to_value(repository().repository).unwrap();
    for &(pointer, path) in &[
        ("/metadata/name", "metadata.name"),
        ("/spec/services/0/name", "spec.services[0].name"),
        ("/spec/volumes/0/name", "spec.volumes[0].name"),
        ("/spec/jobs/0/name", "spec.jobs[0].name"),
        ("/spec/secrets/0/name", "spec.secrets[0].name"),
        (
            "/spec/services/0/secrets/0/target",
            "spec.services[0].secrets[0].target",
        ),
        (
            "/spec/services/0/mounts/0/target",
            "spec.services[0].mounts[0].target",
        ),
        ("/spec/manifest/path", "spec.manifest.path"),
        ("/spec/manifest/repository/url", "spec.manifest.repository"),
        (
            "/spec/manifest/repository/branch",
            "spec.manifest.repository",
        ),
        (
            "/spec/manifest/repository/commit",
            "spec.manifest.repository",
        ),
        (
            "/spec/services/1/source/repository/url",
            "spec.services[1].source.repository",
        ),
        (
            "/spec/services/1/source/repository/branch",
            "spec.services[1].source.repository",
        ),
        (
            "/spec/services/1/source/repository/commit",
            "spec.services[1].source.repository",
        ),
    ] {
        for text in ["${{env.name}}", "$${{env.name}}"] {
            let mut wire = input.clone();
            if pointer.ends_with("/commit") {
                let parent = pointer.strip_suffix("/commit").unwrap();
                wire.pointer_mut(parent).unwrap()["commit"] = json!(text);
            } else {
                *wire.pointer_mut(pointer).unwrap() = json!(text);
            }
            let errors = parse_template_json(&wire.to_string()).unwrap_err();
            assert!(
                codes_and_paths(&errors).contains(&(ApiErrorCode::VariableNotAllowed, path)),
                "{pointer}: {errors}"
            );
        }
    }
    for (pointer, path) in [
        (
            "/spec/services/0/environment",
            "spec.services[0].environment.name",
        ),
        (
            "/spec/services/1/source/build/args",
            "spec.services[1].source.build.args.name",
        ),
    ] {
        for text in ["${{env.name}}", "$${{env.name}}"] {
            let mut wire = input.clone();
            *wire.pointer_mut(pointer).unwrap() = json!({text: "literal"});
            let errors = parse_template_json(&wire.to_string()).unwrap_err();
            assert!(
                codes_and_paths(&errors).contains(&(ApiErrorCode::VariableNotAllowed, path)),
                "{errors}"
            );
        }
    }
}

#[test]
fn non_template_wire_types_and_headers_reject_references_during_decode_or_validation() {
    let mut input = serde_json::to_value(complete()).unwrap();
    input["spec"]["volumes"] = json!([{"name":"data"}]);
    input["spec"]["services"][0]["mounts"] =
        json!([{"volume":"data", "target":"/data", "read_only":true}]);
    input["spec"]["secrets"] = json!([
        {"name":"random", "generate":{"type":"random", "bytes":32, "encoding":"hex"}},
        {"name":"rsa", "generate":{"type":"rsa", "bits":2048}}
    ]);
    for (pointer, path) in [
        (
            "/spec/services/1/source/repository",
            "spec.services[1].source",
        ),
        (
            "/spec/services/0/mounts/0/read_only",
            "spec.services[0].mounts[0].read_only",
        ),
        (
            "/spec/routes/1/redirect/status",
            "spec.routes[1].redirect.status",
        ),
        (
            "/spec/routes/1/redirect/preserve_path",
            "spec.routes[1].redirect.preserve_path",
        ),
        (
            "/spec/secrets/0/generate/type",
            "spec.secrets[0].generate.type",
        ),
        ("/spec/secrets/0/generate/bytes", "spec.secrets[0].generate"),
        (
            "/spec/secrets/0/generate/encoding",
            "spec.secrets[0].generate",
        ),
        ("/spec/secrets/1/generate/bits", "spec.secrets[1].generate"),
        (
            "/spec/services/0/source/type",
            "spec.services[0].source.type",
        ),
        (
            "/spec/services/1/source/build/type",
            "spec.services[1].source",
        ),
        (
            "/spec/services/0/healthcheck/type",
            "spec.services[0].healthcheck.type",
        ),
        ("/spec/routes/0/port", "spec.routes[0].port"),
        ("/spec/jobs/0/run", "spec.jobs[0].run"),
        (
            "/spec/jobs/0/timeout_seconds",
            "spec.jobs[0].timeout_seconds",
        ),
    ] {
        for text in ["${{env.name}}", "$${{env.name}}"] {
            let mut wire = input.clone();
            *wire.pointer_mut(pointer).unwrap() = json!(text);
            let errors = parse_template_json(&wire.to_string()).unwrap_err();
            assert_eq!(
                codes_and_paths(&errors),
                [(ApiErrorCode::ManifestDecodeFailed, path)],
                "{pointer}: {errors}"
            );
        }
    }
    for (field, code) in [
        ("api_version", ApiErrorCode::ApiVersionUnsupported),
        ("kind", ApiErrorCode::KindUnsupported),
    ] {
        let mut wire = input.clone();
        wire[field] = json!("${{env.name}}");
        assert_eq!(
            codes_and_paths(&parse_template_json(&wire.to_string()).unwrap_err()),
            [(code, field)]
        );
    }
}

#[test]
fn template_parsers_agree_and_reject_bad_wire_data_and_trailing_json() {
    let text = manifest(VARIABLES);
    let input: ApplicationManifest = toml::from_str(&text).unwrap();
    let json = serde_json::to_string(&input).unwrap();
    assert_eq!(
        parse_template_toml(&text).unwrap(),
        parse_template_json(&json).unwrap()
    );
    for text in ["not TOML", "[spec", "api_version = 2"] {
        assert_eq!(
            parse_template_toml(text).unwrap_err().0[0].code,
            ApiErrorCode::ManifestDecodeFailed
        );
    }
    for text in [
        "{".to_owned(),
        format!("{json} false"),
        json.replace(
            "\"replicas\":\"${{ vars.web_replicas }}\"",
            "\"replicas\":[]",
        ),
    ] {
        assert_eq!(
            parse_template_json(&text).unwrap_err().0[0].code,
            ApiErrorCode::ManifestDecodeFailed
        );
    }
    let mut wire = serde_json::to_value(input).unwrap();
    wire["spec"]["variables"]["tag"] = json!(["not scalar"]);
    assert_eq!(
        codes_and_paths(&parse_template_json(&wire.to_string()).unwrap_err()),
        [(ApiErrorCode::ManifestDecodeFailed, "spec.variables")]
    );
}

#[test]
fn foreign_resource_names_and_declaration_keys_reject_references() {
    let mut input = serde_json::to_value(complete()).unwrap();
    input["spec"]["services"][0]["depends_on"] = json!(["b-build"]);
    input["spec"]["volumes"] = json!([{"name":"data"}]);
    input["spec"]["services"][0]["mounts"] = json!([{"volume":"data", "target":"/data"}]);
    for (pointer, code, path) in [
        (
            "/spec/services/0/depends_on/0",
            ApiErrorCode::ServiceDependencyMissing,
            "spec.services[0].depends_on[0]",
        ),
        (
            "/spec/services/0/mounts/0/volume",
            ApiErrorCode::MountVolumeMissing,
            "spec.services[0].mounts[0].volume",
        ),
        (
            "/spec/routes/0/service",
            ApiErrorCode::RouteServiceMissing,
            "spec.routes[0].service",
        ),
        (
            "/spec/jobs/0/service",
            ApiErrorCode::JobServiceMissing,
            "spec.jobs[0].service",
        ),
    ] {
        for text in ["${{env.name}}", "$${{env.name}}"] {
            let mut wire = input.clone();
            *wire.pointer_mut(pointer).unwrap() = json!(text);
            assert_eq!(
                codes_and_paths(&parse_template_json(&wire.to_string()).unwrap_err()),
                [(code, path)]
            );
        }
    }
    for text in ["${{env.name}}", "$${{env.name}}"] {
        let mut wire = input.clone();
        wire["spec"]["variables"] = json!({text: 1});
        let errors = parse_template_json(&wire.to_string()).unwrap_err();
        assert_eq!(
            codes_and_paths(&errors),
            [(
                ApiErrorCode::VariableNameInvalid,
                format!("spec.variables.{text}").as_str()
            )]
        );
        wire["spec"]["variables"] = json!({});
        wire["spec"]["environments"] = json!({text: {}});
        assert_eq!(
            codes_and_paths(&parse_template_json(&wire.to_string()).unwrap_err()),
            [(ApiErrorCode::NameInvalid, "spec.environments")]
        );
    }
}

/// Type errors quote a bounded excerpt of the rendered value and name what the field accepts.
#[test]
fn type_error_diagnostics_bound_large_substituted_values() {
    let mut manifest = base();
    manifest
        .spec
        .variables
        .insert("large".into(), Variable::String("x".repeat(60_000).into()));
    manifest.spec.services[0].replicas = Typed::Template("${{vars.large}}".into());
    let errors = render(&normalize(manifest), "production").unwrap_err();
    assert_eq!(
        codes_and_paths(&errors),
        [(
            ApiErrorCode::VariableTypeInvalid,
            "spec.services[0].replicas"
        )]
    );
    assert_eq!(
        errors.0[0].message,
        format!(
            "renders to \"{}, expected an integer from 0 to 65535",
            "x".repeat(63)
        )
    );
}
