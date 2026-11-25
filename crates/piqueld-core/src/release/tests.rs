use super::*;
use crate::{
    ApiErrorCode, ApplicationId, EnvironmentId, InstanceId, ResolutionSet, compile_application,
    compile_release, manifest::parse_template_toml,
};

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// A repository-backed shop whose `web` service builds `self` with `arg`
/// as its `VITE_ORIGIN` build argument and runs with per-environment
/// replicas and origin.
fn template(arg: &str) -> ApplicationTemplate {
    parse_template_toml(&format!(
        r#"api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "shop"
[spec.manifest]
path = "piqueld.toml"
[spec.manifest.repository]
url = "https://example.com/shop.git"
branch = "main"
[spec.variables]
replicas = 1
[spec.environments.staging.variables]
domain = "staging.example.com"
[spec.environments.production.variables]
domain = "example.com"
replicas = 3
[[spec.services]]
name = "web"
replicas = "${{{{ vars.replicas }}}}"
[spec.services.environment]
ORIGIN = "https://${{{{ vars.domain }}}}"
[spec.services.source]
type = "git"
repository = "self"
[spec.services.source.build]
type = "docker"
dockerfile = "Dockerfile"
[spec.services.source.build.args]
VITE_ORIGIN = "{arg}"
REVISION = "${{{{ git.sha }}}}"
"#
    ))
    .unwrap()
    .normalize(ApplicationId::parse("app-shop-0001").unwrap())
}

fn environment(name: &str) -> EnvironmentId {
    EnvironmentId::parse(format!("env-{name}-0001")).unwrap()
}

fn instance() -> InstanceId {
    InstanceId::parse("instance").unwrap()
}

/// The release staging records after building `web` from `template`.
fn staging_release(template: ApplicationTemplate) -> Release {
    let unprepared = Release {
        template,
        commit: Some(COMMIT.into()),
        sources: BTreeMap::new(),
    };
    let rendering = unprepared
        .render(EnvironmentName::parse("staging").unwrap(), None)
        .unwrap();
    let web = &rendering.application.spec().services[0];
    let resolutions = ResolutionSet {
        sources: BTreeMap::from([(
            web.name.clone(),
            ResolvedSource::Git {
                requested: web.source.clone(),
                commit: COMMIT.into(),
                image_id: Sha256Digest::parse(format!("sha256:{}", "a".repeat(64)))
                    .unwrap()
                    .into(),
            },
        )]),
        secret_names: BTreeMap::new(),
    };
    let target = compile_application(
        &rendering.application,
        &environment("staging"),
        instance(),
        &resolutions,
    )
    .unwrap();
    Release::new(unprepared.template, unprepared.commit, &target)
}

#[test]
fn records_provenance_images_and_fingerprint_and_hashes_content() {
    let release = staging_release(template("https://example.com"));
    let web = ServiceName::parse("web").unwrap();
    let ResolvedSource::Git {
        commit, image_id, ..
    } = &release.sources()[&web]
    else {
        panic!("Git build")
    };
    assert_eq!(
        (commit.as_str(), image_id.as_str()),
        (COMMIT, &*format!("sha256:{}", "a".repeat(64)))
    );
    assert_eq!(
        *release.fingerprint().services()[&web].fields(),
        BTreeMap::from(
            [
                ("source.build.args.REVISION", COMMIT),
                ("source.build.args.VITE_ORIGIN", "https://example.com"),
                ("source.build.context", "."),
                ("source.build.dockerfile", "Dockerfile"),
                ("source.commit", COMMIT),
                ("source.repository", "https://example.com/shop.git"),
            ]
            .map(|(field, value)| (field.to_owned(), value.to_owned()))
        )
    );
    // The name renders through `${{ app.name }}`, so it is content, like
    // a build argument.
    let renamed = Release {
        template: release
            .template
            .clone()
            .with_name(crate::ApplicationName::parse("store").unwrap()),
        ..release.clone()
    };
    assert_ne!(renamed.content_hash(), release.content_hash());
    // So is the branch the manifest was read from, `${{ git.branch }}`.
    let mut manifest = release.template.spec().manifest.clone().unwrap();
    manifest.repository.branch = "release".into();
    let branched = Release {
        template: release.template.clone().with_manifest(Some(manifest)),
        ..release.clone()
    };
    assert_ne!(branched.content_hash(), release.content_hash());
    assert_ne!(
        staging_release(template("https://shop.example.com")).content_hash(),
        release.content_hash()
    );
}

#[test]
fn instantiates_for_an_environment_whose_variables_change_only_runtime_fields() {
    let release = staging_release(template("https://example.com"));
    let rendering = release
        .render(
            EnvironmentName::parse("production").unwrap(),
            Some("deployment".into()),
        )
        .unwrap();
    let target = compile_release(
        &rendering.application,
        &environment("production"),
        instance(),
        &release,
        BTreeMap::new(),
    )
    .unwrap();
    let web = &target.services[0];
    assert_eq!(web.replicas, 3);
    assert_eq!(web.environment["ORIGIN"], "https://example.com");
    assert_eq!(
        web.source,
        release.sources().values().next().unwrap().clone()
    );
}

#[test]
fn refuses_an_environment_that_renders_a_build_argument_differently() {
    let release = staging_release(template("https://${{ vars.domain }}"));
    let rendering = release
        .render(EnvironmentName::parse("production").unwrap(), None)
        .unwrap();
    let errors = compile_release(
        &rendering.application,
        &environment("production"),
        instance(),
        &release,
        BTreeMap::new(),
    )
    .unwrap_err();
    assert_eq!(
        errors
            .iter()
            .map(|error| (error.code.as_str(), error.resource.as_str()))
            .collect::<Vec<_>>(),
        [(
            ApiErrorCode::ReleaseIncompatible.as_str(),
            "web.source.build.args.VITE_ORIGIN"
        )]
    );
}
