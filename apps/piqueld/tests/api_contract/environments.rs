//! Applications with several environments: independent deploys, shared history and environment branches.

use super::*;

/// Environments refuse an invalid branch or commit like previews do, as a
/// malformed request field rather than a manifest validation failure.
#[tokio::test]
async fn environment_branches_are_validated_like_preview_branches() {
    let temp = tempfile::tempdir().expect("the temporary directory is created");
    let server = Server::tcp(router(state(&temp).await, FakeAuth)).await;
    let saved = server
        .client()
        .apply_and_deploy(&new_application())
        .await
        .expect("the application deploys");
    let (application, environment) = (&saved.application_id, &saved.environment_id);
    let json = [("content-type", "application/json")];
    for (method, path, body, code) in [
        (
            Method::POST,
            format!("/api/v1/applications/{application}/environments"),
            r#"{"name":"staging","branch":"bad..branch","expected_generation":1}"#,
            "git_branch_invalid",
        ),
        (
            Method::PUT,
            format!("/api/v1/environments/{environment}/branch"),
            r#"{"branch":"main","commit":"abc","expected_generation":1}"#,
            "git_commit_invalid",
        ),
    ] {
        let response = server.send(method, &path, &json, body.into()).await;
        response.assert_error(StatusCode::BAD_REQUEST, code);
    }
}

/// A deployment reads on its own only through its environment, and
/// `view=summary` lists the same deployments without their manifests.
#[tokio::test]
async fn deployments_read_alone_and_list_as_summaries() {
    let temp = tempfile::tempdir().expect("the temporary directory is created");
    let server = Server::tcp(router(state(&temp).await, FakeAuth)).await;
    let client = server.client();
    let deployed = client
        .apply_and_deploy(&new_application())
        .await
        .expect("the application deploys");
    let (environment, id) = (&deployed.environment_id, &deployed.operation_id);
    let one = client
        .deployment(environment, id)
        .await
        .expect("the deployment loads");
    assert_eq!(&one.operation.id, id);
    let missing = client
        .deployment("app-missing", id)
        .await
        .expect_err("the lookup is rejected");
    assert_eq!(api_error(missing).status, StatusCode::NOT_FOUND);

    let full = client
        .deployments(environment, None, None)
        .await
        .expect("the deployments list");
    let summaries = client
        .deployment_summaries(environment, None, None)
        .await
        .expect("the deployment summaries list");
    let ids = |ids: Vec<&String>| ids.into_iter().cloned().collect::<Vec<_>>();
    assert_eq!(
        ids(full.items.iter().map(|item| &item.operation.id).collect()),
        ids(summaries
            .items
            .iter()
            .map(|item| &item.operation.id)
            .collect()),
    );
    let path = format!("/api/v1/environments/{environment}/deployments?view=summary");
    let body = server.send(Method::GET, &path, &[], Vec::new()).await.body;
    assert!(
        !String::from_utf8(body.to_vec())
            .expect("the bytes are utf-8")
            .contains("\"template\"")
    );
}

/// The application listing carries each environment's status and latest
/// operation, so listing applications needs no request per environment.
#[tokio::test]
async fn the_listing_carries_each_environments_status_and_latest_operation() {
    let temp = tempfile::tempdir().expect("the temporary directory is created");
    let server = Server::tcp(router(state(&temp).await, FakeAuth)).await;
    let client = server.client();
    let deployed = client
        .apply_and_deploy(&new_application())
        .await
        .expect("the application deploys");
    let listed = client
        .applications()
        .await
        .expect("the applications list")
        .items
        .remove(0);
    let environment = &listed.environments[0];
    assert_eq!(environment.environment.id.as_str(), deployed.environment_id);
    let status = environment.status.as_ref().expect("a status");
    let expected = client
        .environment_status(&deployed.environment_id)
        .await
        .expect("the environment status loads");
    assert_eq!(status.state, expected.state);
    let latest = environment.latest_operation.as_ref().expect("an operation");
    assert_eq!(latest.id, deployed.operation_id);
}

#[tokio::test]
async fn environments_deploy_independently_and_runtime_commands_never_pick_one() {
    use piqueld::api::{ApplicationError, Mutation, MutationResponse};
    use piqueld_core::EnvironmentName;
    use piqueld_store::StoreError;
    let temp = tempfile::tempdir().expect("tempdir opens");
    let service = state(&temp).await;
    let (saved, staging) = two_environments(&service).await;
    let application = saved.application_id.clone();
    let view = service.application(&application).await.expect("app loads");
    assert_eq!(view.generation, saved.generation);
    let production = view
        .environment("production")
        .expect("env loads")
        .id
        .clone();
    assert!(production.as_str().starts_with("env-"), "{production}");
    assert_eq!(view.sole_environment().expect_err("lookup fails").len(), 2);

    assert!(matches!(
        service
            .accept(Daemon,
                Mutation::save(
                    manifest().validate_template().expect("it validates"),
                    Some(saved.application_id.clone()),
                    true,
                ),
                Some(saved.generation),
                false,
                None,
            )
            .await,
        Err(ApplicationError::Store(StoreError::EnvironmentRequired { environments }))
            if environments.len() == 2
    ));

    let MutationResponse::Operation(deployed) = service
        .accept(
            Daemon,
            Mutation::deploy(staging.id.clone()),
            Some(saved.generation),
            false,
            None,
        )
        .await
        .expect("it accepts")
    else {
        panic!("deployment")
    };
    assert_eq!(deployed.environment_id, staging.id);
    assert!(
        service
            .environment_detail(&production, &piqueld_core::access::Scope::All)
            .await
            .expect("detail loads")
            .latest_operation
            .is_none()
    );
    assert_eq!(
        service
            .environment_detail(&staging.id, &piqueld_core::access::Scope::All)
            .await
            .expect("detail loads")
            .latest_operation
            .expect("operation exists")
            .id,
        deployed.operation_id
    );

    let delete = |names: &[&str]| Mutation::DeleteApplication {
        id: application.clone(),
        environments: names
            .iter()
            .map(|name| EnvironmentName::parse(*name).expect("name parses"))
            .collect(),
    };
    assert!(matches!(
        service
            .accept(
                Daemon,
                delete(&["production"]),
                Some(saved.generation),
                false,
                None
            )
            .await,
        Err(ApplicationError::Store(
            StoreError::ConfirmationRequired { .. }
        ))
    ));
    let MutationResponse::Deleted(deleted) = service
        .accept(
            Daemon,
            delete(&["staging", "production"]),
            Some(saved.generation),
            false,
            None,
        )
        .await
        .expect("it accepts")
    else {
        panic!("deleted")
    };
    assert_eq!(deleted.operations.len(), 2);
    assert_eq!(deleted.generation, saved.generation + 1);
}

#[tokio::test]
async fn application_history_spans_its_environments() {
    use piqueld::api::Mutation;
    use piqueld_core::observability::EventFilter;
    let temp = tempfile::tempdir().expect("the temporary directory is created");
    let service = state(&temp).await;
    let (saved, staging) = two_environments(&service).await;
    let application = saved.application_id.clone();
    let production = production(&service, saved.application_id.as_str()).await;
    service
        .accept(
            Daemon,
            rename(&application, "renamed"),
            Some(saved.generation),
            false,
            None,
        )
        .await
        .expect("the connection is accepted");
    service
        .accept(
            Daemon,
            Mutation::deploy(production.clone()),
            Some(saved.generation + 1),
            false,
            None,
        )
        .await
        .expect("the connection is accepted");
    let events = |filter: EventFilter| {
        let service = service.clone();
        async move {
            service
                .filtered_events(&filter, &piqueld_store::Visibility::ALL, None, 100)
                .await
                .expect("the filtered events list")
                .items
                .into_iter()
                .map(|event| (event.kind, event.environment_id))
                .collect::<Vec<_>>()
        }
    };

    let history = events(EventFilter {
        application_id: Some(application.to_string()),
        ..EventFilter::default()
    })
    .await;
    assert!(history.contains(&("environment_created".into(), Some(staging.id.clone()))));
    assert_eq!(
        history
            .iter()
            .filter(|(kind, _)| kind == "application_renamed")
            .collect::<Vec<_>>(),
        [&("application_renamed".to_owned(), None)]
    );
    let production_history = events(EventFilter {
        environment_id: Some(production.to_string()),
        ..EventFilter::default()
    })
    .await;
    let in_production = |(_, environment): &(String, Option<piqueld_core::EnvironmentId>)| {
        environment.as_ref() == Some(&production)
    };
    assert!(production_history.iter().any(in_production));
    assert!(production_history.iter().all(in_production));
}

#[tokio::test]
async fn previews_compare_with_the_selected_environment() {
    use piqueld::api::{ApplicationError, Mutation};
    use piqueld_core::api::RuntimeCoverage;
    use piqueld_store::StoreError;
    let temp = tempfile::tempdir().expect("the temporary directory is created");
    let service = state(&temp).await;
    let (saved, staging) = two_environments(&service).await;
    let production = production(&service, saved.application_id.as_str()).await;
    service
        .accept(
            Daemon,
            Mutation::deploy(staging.id.clone()),
            Some(saved.generation),
            false,
            None,
        )
        .await
        .expect("the connection is accepted");
    let plan = |environment: Option<piqueld_core::EnvironmentId>| {
        let service = service.clone();
        async move {
            service
                .plan(
                    &piqueld_core::access::Grants::admin(),
                    manifest()
                        .validate_template()
                        .expect("the template validates"),
                    None,
                    None,
                    environment.as_ref(),
                )
                .await
        }
    };

    let unselected = plan(None).await.expect("the plan builds");
    assert!(unselected.operation.is_none() && unselected.plan.actions.is_empty());
    assert_eq!(unselected.runtime_coverage, RuntimeCoverage::Omitted);

    let deployed = plan(Some(staging.id.clone()))
        .await
        .expect("the plan builds");
    assert!(deployed.identical);
    assert!(deployed.operation.is_some());
    let resolves_image = |plan: &piqueld_core::api::PlanView| {
        plan.plan
            .actions
            .iter()
            .any(|action| matches!(action.kind, ActionKind::ResolveImage { .. }))
    };
    assert!(resolves_image(&deployed));
    assert_eq!(deployed.runtime_coverage, RuntimeCoverage::Resolution);

    let undeployed = plan(Some(production)).await.expect("the plan builds");
    assert!(!undeployed.identical && undeployed.operation.is_none());
    assert!(!undeployed.changes.is_empty() && resolves_image(&undeployed));

    let mut other = manifest();
    other.metadata.name = "other".into();
    save(&service, other).await;
    let foreign = service
        .applications(&piqueld_core::access::Scope::All, None, None, None)
        .await
        .expect("the applications list")
        .items;
    let foreign = foreign
        .iter()
        .find(|application| application.name == "other")
        .expect("the item exists")
        .environments[0]
        .environment
        .id
        .clone();
    assert!(matches!(
        plan(Some(foreign)).await,
        Err(ApplicationError::Store(StoreError::NotFound))
    ));
}

#[tokio::test]
async fn previews_render_variables_for_the_selected_environment() {
    use piqueld::api::ApplicationError;
    use piqueld_core::manifest::{EnvironmentConfig, Variable, VariableValue};
    use piqueld_store::StoreError;
    let temp = tempfile::tempdir().expect("the temporary directory is created");
    let service = state(&temp).await;
    let (saved, staging) = two_environments(&service).await;
    let production = production(&service, saved.application_id.as_str()).await;
    let level = |value: &str| EnvironmentConfig {
        variables: [("level".into(), Variable::String(value.into()))].into(),
        ..EnvironmentConfig::default()
    };
    let mut manifest = manifest();
    manifest.spec.services[0]
        .environment
        .insert("LEVEL".into(), "${{ vars.level }}".into());
    manifest.spec.environments.extend([
        ("staging".into(), level("debug")),
        ("preview".into(), level("trace")),
    ]);
    let plan = |environment: piqueld_core::EnvironmentId| {
        let (service, manifest) = (service.clone(), manifest.clone());
        async move {
            service
                .plan(
                    &piqueld_core::access::Grants::admin(),
                    manifest
                        .validate_template()
                        .expect("the template validates"),
                    None,
                    None,
                    Some(&environment),
                )
                .await
        }
    };

    let rendered = plan(staging.id.clone()).await.expect("the plan builds");
    assert_eq!(
        rendered.variables["vars.level"],
        VariableValue::String("debug".into())
    );
    assert_eq!(
        rendered.variables["env.name"],
        VariableValue::String("staging".into())
    );
    let name = |name: &str| {
        piqueld_core::EnvironmentName::parse(name).expect("the environment name parses")
    };
    assert_eq!(
        rendered.environments,
        [
            piqueld_core::manifest::EnvironmentChange::Create {
                environment: name("preview"),
                source: piqueld_core::manifest::DeclaredSource::Saved,
                sync: false,
            },
            piqueld_core::manifest::EnvironmentChange::Undeclared {
                environment: name("production"),
            },
        ]
    );
    assert!(!rendered.plan.is_blocked());

    let Err(ApplicationError::Store(StoreError::Validation(errors))) = plan(production).await
    else {
        panic!("missing value expected")
    };
    assert_eq!(
        (errors.0[0].code.as_str(), errors.0[0].path.as_str()),
        (
            "variable_value_missing",
            "spec.services[0].environment.LEVEL"
        )
    );
}
