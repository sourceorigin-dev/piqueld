//! Mutation acceptance: generations, preconditions, forced applies and receipts that replay across restarts.

use super::*;
use piqueld_client::edit::{EditOptions, StringValue};

#[tokio::test]
async fn generations_deploy_reconcile_and_event_pagination_share_the_http_contract() {
    let temp = tempfile::tempdir().expect("the temporary directory is created");
    let server = Server::tcp(router(state(&temp).await, FakeAuth)).await;
    let client = server.client();
    let mut request = new_application();
    let first = client
        .apply_and_deploy(&request)
        .await
        .expect("the application deploys");
    assert_eq!(first.generation, 1);
    let stale = client
        .apply_and_deploy(&request)
        .await
        .expect_err("the deploy is rejected");
    assert_eq!(api_error(stale).status, 409);
    request.expected_generation = Some(1);
    request.expected_application_id = Some(first.application_id.clone());
    request.manifest.spec.services[0].replicas = 2.into();
    let changed = client
        .apply_and_deploy(&request)
        .await
        .expect("the application deploys");
    assert_eq!(changed.generation, 2);
    let revision = piqueld_client::ManifestRevision::Branch("feature".into());
    let unbacked = client
        .deploy_environment(&first.environment_id, 2, Some(&revision), false)
        .await
        .expect_err("the deploy is rejected");
    assert!(
        matches!(unbacked, ClientError::Api { error, .. } if error.details.to_string().contains("manifest_repository_required"))
    );
    let refreshed = client
        .deploy_environment(&first.environment_id, 2, None, false)
        .await
        .expect("the environment deploys");
    assert_eq!(refreshed.generation, 2);
    assert_ne!(refreshed.operation_id, changed.operation_id);
    let reconciled = client
        .reconcile_environment(&first.environment_id, Some(2))
        .await
        .expect("the environment reconciles");
    assert_eq!(reconciled.operation_id, refreshed.operation_id);
    let first_page = client
        .events(None, Some(&first.environment_id), None, false, None, 2)
        .await
        .expect("the events list");
    assert_eq!(first_page.items.len(), 2);
    let second_page = client
        .events(
            None,
            Some(&first.environment_id),
            None,
            false,
            first_page.next_cursor.as_deref(),
            100,
        )
        .await
        .expect("the events list");
    assert!(!second_page.items.is_empty());
    assert!(
        second_page
            .items
            .iter()
            .all(|event| event.id > first_page.items.last().expect("the last item exists").id)
    );
    let deletion = client
        .delete_application_with_generation(&first.application_id, Some(2))
        .await
        .expect("the application deletes");
    assert_eq!(deletion.generation, 3);
    client
        .deploy_environment(&first.environment_id, 3, None, false)
        .await
        .expect_err("`` should be rejected");
    let repeated = client
        .delete_application_with_generation(&first.application_id, Some(deletion.generation))
        .await
        .expect("the application deletes");
    assert_eq!(
        repeated.operations[0].operation_id,
        deletion.operations[0].operation_id
    );
}

/// Rerunning a command with its request ID returns what the first run got,
/// even though the rerun inspected the preconditions again, after the first
/// run changed them, and never applies the change twice. Other content under
/// the same ID still conflicts.
#[tokio::test]
async fn a_rerun_with_the_same_request_id_replays_its_acceptance() {
    let temp = tempfile::tempdir().expect("the temporary directory is created");
    let api = AcceptanceApi::start(&temp).await;
    let request = new_application();
    let keyed = api.client.clone().with_request_id("agent-run-7");
    let accepted = keyed
        .apply_and_deploy(&request)
        .await
        .expect("the application deploys");
    let mut rerun = request.clone();
    rerun.expected_generation = Some(accepted.generation);
    rerun.expected_application_id = Some(accepted.application_id.clone());
    let replayed = keyed
        .apply_and_deploy(&rerun)
        .await
        .expect("the application deploys");
    assert_eq!(replayed.operation_id, accepted.operation_id);
    assert_eq!(
        api.client
            .application(&accepted.application_id)
            .await
            .expect("the application loads")
            .generation,
        accepted.generation
    );
    rerun.manifest.spec.services[0].replicas = 2.into();
    let error = keyed
        .apply_and_deploy(&rerun)
        .await
        .expect_err("the deploy is rejected");
    assert_eq!(api_error(error).code, "request_id_conflict");
}

#[tokio::test]
async fn acceptance_receipts_survive_restart_and_supersession() {
    let temp = tempfile::tempdir().expect("the temporary directory is created");
    let api = AcceptanceApi::start(&temp).await;
    let request = new_application();
    let keyed = api.client.clone().with_request_id("apply-command-1");
    let accepted = keyed
        .apply_and_deploy(&request)
        .await
        .expect("the application deploys");
    let mut replacement = request.clone();
    replacement.expected_generation = Some(1);
    replacement.expected_application_id = Some(accepted.application_id.clone());
    replacement.manifest.spec.services[0].replicas = 2.into();
    api.client
        .apply_and_deploy(&replacement)
        .await
        .expect("the application deploys");
    drop(api);
    let api = AcceptanceApi::start(&temp).await;
    let keyed = api.client.clone().with_request_id("apply-command-1");
    let replayed = keyed
        .apply_and_deploy(&request)
        .await
        .expect("the application deploys");
    assert_eq!(replayed.operation_id, accepted.operation_id);
    assert_eq!(replayed.generation, 1);
    assert_eq!(
        api.client
            .application(&accepted.application_id)
            .await
            .expect("the application loads")
            .generation,
        2
    );
    assert_eq!(
        api.client
            .operation(&accepted.operation_id)
            .await
            .expect("the operation loads")
            .state,
        piqueld_core::OperationState::Superseded
    );
    let error = keyed
        .apply_and_deploy(&replacement)
        .await
        .expect_err("the deploy is rejected");
    assert_eq!(api_error(error).code, "request_id_conflict");
    let mut fresh = request.clone();
    fresh.manifest.metadata.name = "another".into();
    let concurrent = api.client.clone().with_request_id("concurrent-command");
    let (a, b) = tokio::join!(
        concurrent.apply_and_deploy(&fresh),
        concurrent.apply_and_deploy(&fresh)
    );
    assert_eq!(
        a.expect("the first value is present").operation_id,
        b.expect("the second value is present").operation_id
    );
}

#[tokio::test]
async fn rename_is_conditioned_idle_only_and_replayable_without_deployment() {
    let temp = tempfile::tempdir().expect("the temporary directory is created");
    let api = AcceptanceApi::start(&temp).await;
    let accepted = api.deploy().await;
    let name = StringValue {
        value: "renamed".into(),
    };
    let options = at_generation(1);
    let keyed = api.client.clone().with_request_id("rename-command");
    let error = keyed
        .set_application_name(&accepted.application_id, &name, &options)
        .await
        .expect_err("the rename is rejected");
    assert_eq!(api_error(error).code, "application_busy");
    api.finish_operation(
        &accepted.operation_id,
        Some(("image_resolution_rejected", "image unavailable")),
    )
    .await;
    let renamed = keyed
        .set_application_name(&accepted.application_id, &name, &options)
        .await
        .expect("the name is set");
    assert_eq!(renamed.application_id, accepted.application_id);
    assert_eq!(renamed.generation, 2);
    assert_eq!(renamed.operation_id, None);
    let replay = keyed
        .set_application_name(&accepted.application_id, &name, &options)
        .await
        .expect("the name is set");
    assert_eq!(replay.generation, 2);
    let app = api
        .client
        .application(&accepted.application_id)
        .await
        .expect("the application loads");
    assert_eq!(app.application.metadata().name.as_str(), "renamed");
    assert_eq!(
        api.client
            .operation(&accepted.operation_id)
            .await
            .expect("the operation loads")
            .state,
        piqueld_core::OperationState::Failed
    );
    let mut identical = new_application();
    identical.manifest.metadata.name = "renamed".into();
    identical.expected_generation = Some(2);
    identical.expected_application_id = Some(accepted.application_id.clone());
    let no_op = api
        .client
        .apply_and_deploy(&identical)
        .await
        .expect("the application deploys");
    assert_ne!(no_op.operation_id, accepted.operation_id);
    assert_eq!(no_op.generation, 3);
    assert_eq!(
        api.client
            .operation(&accepted.operation_id)
            .await
            .expect("the operation loads")
            .state,
        piqueld_core::OperationState::Failed
    );
    let old_name = api.deploy().await;
    let mut stale = new_application();
    stale.expected_generation = Some(1);
    stale.expected_application_id = Some(accepted.application_id);
    stale.manifest.spec.services[0].replicas = 3.into();
    let error = api
        .client
        .apply_and_deploy(&stale)
        .await
        .expect_err("the deploy is rejected");
    assert_eq!(api_error(error).code, "identity_conflict");
    assert_eq!(
        api.client
            .application(&old_name.application_id)
            .await
            .expect("the application loads")
            .application
            .spec()
            .services[0]
            .replicas,
        1.into()
    );
}

#[tokio::test]
async fn receipt_failure_rolls_back_acceptance_and_expired_keys_are_reusable() {
    let temp = tempfile::tempdir().expect("the temporary directory is created");
    let database = database(&temp);
    let api = AcceptanceApi::start(&temp).await;
    execute(&database, "CREATE TRIGGER reject_receipt BEFORE INSERT ON request_receipts BEGIN SELECT RAISE(ABORT,'injected receipt failure'); END").await;
    let keyed = api.client.clone().with_request_id("atomic-command");
    let request = new_application();
    keyed
        .apply_and_deploy(&request)
        .await
        .expect_err("`apply_and_deploy` should be rejected");
    assert_eq!(
        api.store
            .list(None, 50)
            .await
            .expect("the list loads")
            .items,
        [] as [StoredEnvironment; 0]
    );
    execute(&database, "DROP TRIGGER reject_receipt").await;
    let accepted = keyed
        .apply_and_deploy(&request)
        .await
        .expect("the application deploys");
    execute(&database, "UPDATE request_receipts SET expires_at_ms=0").await;
    let refreshed = keyed
        .deploy_environment(&accepted.environment_id, 1, None, false)
        .await
        .expect("the environment deploys");
    assert_ne!(refreshed.operation_id, accepted.operation_id);
}

#[tokio::test]
async fn preview_resolves_images_again_and_redacts_manifest_and_runtime_configuration() {
    let temp = tempfile::tempdir().expect("the temporary directory is created");
    let api = AcceptanceApi::start(&temp).await;
    let mut request = new_application();
    request.manifest.spec.services[0]
        .environment
        .insert("TOKEN".into(), "old-private-value".into());
    let normalized = request
        .manifest
        .clone()
        .validate()
        .expect("the value validates")
        .normalize(
            piqueld_core::ApplicationId::parse("app-preview-01")
                .expect("the application id parses"),
        );
    let resolve = |environment: &piqueld_core::EnvironmentId| {
        api.runtime
            .resolve(environment, &normalized)
            .expect("the value resolves")
    };
    api.store
        .save_application(
            &ApplicationTemplate::from(&normalized),
            Some(&resolve),
            Some(0),
        )
        .await
        .expect("the application saves");
    request.expected_generation = Some(1);
    request.expected_application_id = Some(normalized.id().to_string());
    request.manifest.spec.services[0].replicas = 3.into();
    request.manifest.spec.services[0]
        .environment
        .insert("TOKEN".into(), "new-private-value".into());
    request.manifest.spec.services[0].command = vec!["command-private-value".into()];
    let preview = api
        .client
        .plan_application(&request, None)
        .await
        .expect("the application plans");
    assert_eq!(preview.generation, 1);
    assert!(!preview.identical);
    assert!(
        preview
            .changes
            .iter()
            .any(|change| change.field == "services.web.replicas"
                && change.after.as_deref() == Some("3"))
    );
    assert!(
        preview
            .plan
            .actions
            .iter()
            .any(|action| matches!(action.kind, ActionKind::ResolveImage { .. }))
    );
    let json = serde_json::to_string(&preview).expect("the value serialises");
    for secret in [
        "old-private-value",
        "new-private-value",
        "command-private-value",
    ] {
        assert!(!json.contains(secret));
    }
    assert!(json.contains("redacted"));
    assert_eq!(
        api.store
            .application(normalized.id())
            .await
            .expect("the application loads")
            .generation,
        1,
        "preview is read-only"
    );
}

#[tokio::test]
async fn mutations_require_preconditions_but_reconcile_uses_current_intent() {
    let temp = tempfile::tempdir().expect("the temporary directory is created");
    let api = AcceptanceApi::start(&temp).await;
    let mut request = new_application();
    request.expected_generation = None;
    assert_eq!(
        api_error(
            api.client
                .apply_and_deploy(&request)
                .await
                .expect_err("the deploy is rejected")
        )
        .code,
        "precondition_required"
    );
    request.expected_generation = Some(0);
    let accepted = api
        .client
        .apply_and_deploy(&request)
        .await
        .expect("the application deploys");
    assert_eq!(
        api_error(
            api.client
                .apply_and_deploy(&request)
                .await
                .expect_err("the deploy is rejected")
        )
        .code,
        "application_name_collision"
    );
    request.expected_generation = Some(1);
    assert_eq!(
        api_error(
            api.client
                .apply_and_deploy(&request)
                .await
                .expect_err("the deploy is rejected")
        )
        .code,
        "precondition_required"
    );
    request.expected_application_id = Some(accepted.application_id.clone());
    request.manifest.spec.services[0].replicas = 2.into();
    let mut competing = request.clone();
    competing.manifest.spec.services[0].replicas = 3.into();
    let (first, second) = tokio::join!(
        api.client.apply_and_deploy(&request),
        api.client.apply_and_deploy(&competing)
    );
    assert_ne!(first.is_ok(), second.is_ok());
    assert_eq!(
        api_error(first.err().or(second.err()).expect("the value is present")).code,
        "generation_conflict"
    );
    assert_eq!(
        api_error(
            api.client
                .delete_application_with_generation(&accepted.application_id, None)
                .await
                .expect_err("the delete is rejected")
        )
        .code,
        "precondition_required"
    );
    assert_eq!(
        api_error(
            api.client
                .delete_application_with_generation(&accepted.application_id, Some(1))
                .await
                .expect_err("the delete is rejected")
        )
        .code,
        "generation_conflict"
    );
    let refreshed = api
        .client
        .deploy_environment(&accepted.environment_id, 2, None, false)
        .await
        .expect("the environment deploys");
    assert_eq!(refreshed.generation, 2);
    let deletion = api
        .client
        .delete_application_with_preconditions(&accepted.application_id, Some(1), true, &[])
        .await
        .expect("the application deletes");
    assert_eq!(deletion.generation, 3);
    let reconciled = api
        .client
        .reconcile_environment(&accepted.environment_id, None)
        .await
        .expect("the environment reconciles");
    assert_eq!(reconciled.operation_id, deletion.operations[0].operation_id);
    api.client
        .deploy_environment(&accepted.environment_id, 2, None, false)
        .await
        .expect_err("`` should be rejected");
}

/// Stopping needs a deployment and lasts until the environment is started,
/// which reconciles it, or deployed. A deployment in progress continues.
#[tokio::test]
async fn environments_stay_stopped_until_started_or_deployed() {
    let temp = tempfile::tempdir().expect("the temporary directory is created");
    let api = AcceptanceApi::start(&temp).await;
    let saved = api.save().await;
    let client = &api.client;
    let id = &production_through(client, &saved.application_id).await;
    let stopped = async || {
        client
            .environment(id)
            .await
            .expect("the environment loads")
            .stopped()
    };
    assert_eq!(
        api_error(
            client
                .stop_environment(id, None)
                .await
                .expect_err("the stop is rejected")
        )
        .code,
        "not_found"
    );

    let deployed = client
        .deploy_environment(id, saved.generation, None, false)
        .await
        .expect("the environment deploys");
    let stop = client
        .stop_environment(id, None)
        .await
        .expect("the environment stops");
    assert_eq!(stop.operation_id, deployed.operation_id);
    assert!(stopped().await);
    let start = client
        .start_environment(id, None)
        .await
        .expect("the environment starts");
    assert_eq!(start.operation_id, deployed.operation_id);
    assert!(!stopped().await);

    client
        .stop_environment(id, None)
        .await
        .expect("the environment stops");
    let redeployed = client
        .deploy_environment(id, saved.generation, None, false)
        .await
        .expect("the environment deploys");
    assert_ne!(redeployed.operation_id, deployed.operation_id);
    assert!(!stopped().await, "deploying starts it");
}

#[tokio::test]
async fn forced_apply_retargets_reused_names_and_creates_absent_names() {
    let temp = tempfile::tempdir().expect("the temporary directory is created");
    let api = AcceptanceApi::start(&temp).await;
    let mut request = new_application();
    request.expected_generation = None;
    let original = api
        .client
        .apply_and_deploy_with_force(&request, true)
        .await
        .expect("the forced deploy succeeds");
    let deletion = api
        .client
        .delete_application_with_generation(&original.application_id, Some(1))
        .await
        .expect("the application deletes");
    api.finish_deletion(&deletion).await;
    let replacement = api.deploy().await;
    assert_ne!(replacement.environment_id, original.environment_id);
    request.expected_generation = Some(1);
    request.expected_application_id = Some(original.application_id);
    request.manifest.spec.services[0].replicas = 4.into();
    assert_eq!(
        api_error(
            api.client
                .apply_and_deploy(&request)
                .await
                .expect_err("the deploy is rejected")
        )
        .code,
        "identity_conflict"
    );
    let forced = api
        .client
        .apply_and_deploy_with_force(&request, true)
        .await
        .expect("the forced deploy succeeds");
    assert_eq!(forced.environment_id, replacement.environment_id);
    assert_eq!(forced.generation, 2);
    assert_eq!(
        api.client
            .application(&forced.application_id)
            .await
            .expect("the application loads")
            .application
            .spec()
            .services[0]
            .replicas,
        4.into()
    );
    request.manifest.spec.services[0].replicas = 0.into();
    assert!(
        api.client
            .apply_and_deploy_with_force(&request, true)
            .await
            .is_err(),
        "force must not bypass manifest validation"
    );
    assert_eq!(
        api.client
            .application(&forced.application_id)
            .await
            .expect("the application loads")
            .generation,
        2
    );
}

#[tokio::test]
async fn forced_receipts_replay_after_restart_without_overwriting_newer_intent() {
    let temp = tempfile::tempdir().expect("the temporary directory is created");
    let api = AcceptanceApi::start(&temp).await;
    let request = new_application();
    let keyed = api.client.clone().with_request_id("forced-command");
    let accepted = keyed
        .apply_and_deploy_with_force(&request, true)
        .await
        .expect("the forced deploy succeeds");
    let mut changed = request.clone();
    changed.manifest.spec.services[0].replicas = 3.into();
    let newer = api
        .client
        .apply_and_deploy_with_force(&changed, true)
        .await
        .expect("the forced deploy succeeds");
    drop(api);
    let api = AcceptanceApi::start(&temp).await;
    let keyed = api.client.clone().with_request_id("forced-command");
    let replay = keyed
        .apply_and_deploy_with_force(&request, true)
        .await
        .expect("the forced deploy succeeds");
    assert_eq!(replay.operation_id, accepted.operation_id);
    assert_eq!(replay.generation, accepted.generation);
    assert_eq!(
        api.client
            .operation(&accepted.operation_id)
            .await
            .expect("the operation loads")
            .state,
        piqueld_core::OperationState::Superseded
    );
    let current = api
        .client
        .application(&accepted.application_id)
        .await
        .expect("the application loads");
    assert_eq!(current.generation, newer.generation);
    assert_eq!(current.application.spec().services[0].replicas, 3.into());
    assert_eq!(
        api_error(
            keyed
                .apply_and_deploy(&request)
                .await
                .expect_err("the deploy is rejected")
        )
        .code,
        "request_id_conflict"
    );
    let separately_invoked = api.client.clone().with_request_id("new-forced-command");
    let reapplied = separately_invoked
        .apply_and_deploy_with_force(&request, true)
        .await
        .expect("the forced deploy succeeds");
    assert_eq!(reapplied.generation, newer.generation + 1);
}

#[tokio::test]
async fn forced_rename_bypasses_revision_but_preserves_busy_and_name_checks() {
    let temp = tempfile::tempdir().expect("the temporary directory is created");
    let api = AcceptanceApi::start(&temp).await;
    let accepted = api.deploy().await;
    let mut name = StringValue {
        value: "renamed".into(),
    };
    let rename = async |name: &StringValue, options: EditOptions| {
        api.client
            .set_application_name(&accepted.application_id, name, &options)
            .await
    };
    let forced = EditOptions {
        force: true,
        ..EditOptions::default()
    };
    assert_eq!(
        api_error(
            rename(&name, EditOptions::default())
                .await
                .expect_err("the rename is rejected")
        )
        .code,
        "precondition_required"
    );
    assert_eq!(
        api_error(
            rename(&name, forced)
                .await
                .expect_err("the rename is rejected")
        )
        .code,
        "application_busy"
    );
    api.finish_operation(&accepted.operation_id, None).await;
    let stale = EditOptions {
        force: true,
        ..at_generation(99)
    };
    let renamed = rename(&name, stale).await.expect("the rename applies");
    assert_eq!(renamed.generation, 2);
    assert_eq!(renamed.application_id, accepted.application_id);
    api.deploy().await;
    name.value = "notes".into();
    assert_eq!(
        api_error(
            rename(&name, forced)
                .await
                .expect_err("the rename is rejected")
        )
        .code,
        "application_name_collision"
    );
}

#[tokio::test]
async fn docker_outage_allows_acceptance_and_preserves_receipt_replay() {
    let temp = tempfile::tempdir().expect("the temporary directory is created");
    let api = AcceptanceApi::start(&temp).await;
    let request = new_application();
    let keyed = api.client.clone().with_request_id("outage-replay");
    let accepted = keyed
        .apply_and_deploy(&request)
        .await
        .expect("the application deploys");
    api.runtime.set_unavailable(true);
    assert_eq!(
        keyed
            .apply_and_deploy(&request)
            .await
            .expect("the application deploys")
            .operation_id,
        accepted.operation_id
    );
    let mut changed = request.clone();
    changed.expected_generation = Some(accepted.generation);
    changed.expected_application_id = Some(accepted.application_id.clone());
    changed.manifest.spec.services[0].replicas = 2.into();
    let unavailable = api.client.plan_application(&changed, None).await;
    assert_eq!(
        api_error(unavailable.expect_err("the plan is refused")).status,
        503
    );
    let next = api
        .client
        .apply_and_deploy(&changed)
        .await
        .expect("the application deploys");
    assert_ne!(next.operation_id, accepted.operation_id);
    assert_eq!(
        api_error(
            keyed
                .apply_and_deploy(&changed)
                .await
                .expect_err("the deploy is rejected")
        )
        .code,
        "request_id_conflict"
    );
}

/// A request key replays only for the account that used it, so another caller
/// reusing it cannot learn the outcome, e.g. of an application since renamed.
/// A creator replays its creation even without `apps:write` on the application.
#[tokio::test]
async fn receipts_replay_only_for_their_account() {
    use piqueld::api::{Actor, ApplicationError, Mutation};
    use piqueld_store::{Caller, StoreError};
    let temp = tempfile::tempdir().expect("the temporary directory is created");
    let state = state(&temp).await;
    let database = database(&temp);
    seed_account(&database, "author", &[("apps:create", None)]).await;
    let grants = [("apps:create", None), ("apps:write", None)];
    seed_account(&database, "other", &grants).await;
    let save = || {
        Mutation::save(
            manifest()
                .validate_template()
                .expect("the template validates"),
            None,
            false,
        )
    };
    let author = Actor::Account(Caller {
        credential_id: "author",
        user_id: "author",
    });
    let other = Actor::Account(Caller {
        credential_id: "other",
        user_id: "other",
    });
    let original = state
        .accept(author, save(), Some(0), false, Some("create-key"))
        .await
        .expect("the connection is accepted");
    let replayed = state
        .accept(author, save(), Some(0), false, Some("create-key"))
        .await
        .expect("the connection is accepted");
    assert_eq!(format!("{replayed:?}"), format!("{original:?}"));
    for caller in [other, Daemon] {
        assert!(matches!(
            state
                .accept(caller, save(), Some(0), false, Some("create-key"))
                .await,
            Err(ApplicationError::Store(StoreError::ReplayConflict))
        ));
    }
}

#[tokio::test]
async fn event_kinds_list_each_recorded_kind_once_in_name_order() {
    let temp = tempfile::tempdir().expect("the temporary directory is created");
    let server = Server::tcp(router(state(&temp).await, FakeAuth)).await;
    let client = server.client();
    let request = new_application();
    let deployed = client
        .apply_and_deploy(&request)
        .await
        .expect("the application deploys");
    client
        .deploy_environment(&deployed.environment_id, 1, None, false)
        .await
        .expect("the environment deploys");
    let kinds = client.event_kinds().await.expect("the event kinds list");
    let mut sorted = kinds.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(kinds, sorted, "distinct, in name order");
    assert!(kinds.iter().any(|kind| kind == "environment_created"));
    assert!(kinds.iter().any(|kind| kind == "refresh_requested"));
    let requests = client
        .events(None, None, Some("refresh_requested"), false, None, 100)
        .await
        .expect("the events list");
    assert_eq!(requests.items.len(), 2);
    assert!(
        requests
            .items
            .iter()
            .all(|event| event.kind == "refresh_requested")
    );
}
