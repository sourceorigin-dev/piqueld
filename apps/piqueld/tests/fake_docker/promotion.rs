use super::*;
use piqueld::api::ApplicationError;
use piqueld_core::api::{
    AcceptedPromotion, DeploymentOrigin, DeploymentView, EnvironmentAccess, PromotionSelection,
    SecretAccess, SecretProblem,
};
use piqueld_core::{ReleaseId, manifest::parse_template_toml};
use piqueld_store::{PromotionError, StoreError};

/// `shop`, whose `web` service renders `ORIGIN` from each environment's
/// `domain`, with more `staging` and `production` variables, then `spec`.
/// It declares `staging`, and `production` promoted from it.
fn shop_toml(staging: &str, production: &str, spec: &str) -> String {
    format!(
        "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='shop'\n\
         [spec.environments.staging.variables]\ndomain='staging.example.com'\n{staging}\n\
         [spec.environments.production]\n{PROMOTED}\
         [spec.environments.production.variables]\ndomain='example.com'\n{production}\n\
         [[spec.services]]\nname='web'\n[spec.services.environment]\nORIGIN='https://${{{{ vars.domain }}}}'\n{spec}"
    )
}

/// Declares `production` promoted from `staging` in [`shop_toml`].
const PROMOTED: &str = "promote_from='staging'\n";

/// The image source every `shop` deploys unless a test changes it.
const IMAGE: &str = "[spec.services.source]\ntype='image'\nimage='ghcr.io/example/notes:1.4.0'";

/// `shop` saved from `manifest`, which declares `staging` and
/// `production`.
struct Shop {
    harness: ControllerHarness,
    application: ApplicationId,
    staging: EnvironmentId,
    production: EnvironmentId,
}

impl Shop {
    async fn new(manifest: &str) -> Self {
        let harness = ControllerHarness::new().await;
        let saved = Box::pin(harness.applications().save(
            parse_template_toml(manifest).expect("parse template toml succeeds"),
            Some(0),
        ))
        .await
        .expect("pin succeeds");
        let application =
            ApplicationId::parse(saved.application_id).expect("the application id parses");
        let environments = harness
            .store
            .environments(&application)
            .await
            .expect("environments succeeds");
        let named = |name: &str| {
            environments
                .iter()
                .find(|environment| environment.name.as_str() == name)
                .expect("the item exists")
                .id
                .clone()
        };
        let (staging, production) = (named("staging"), named("production"));
        Self {
            harness,
            application,
            staging,
            production,
        }
    }

    /// `shop` with the default image, whose `production` is not promoted
    /// yet but tracks its own configuration.
    async fn tracking() -> Self {
        Self::new(&shop_toml("", "", IMAGE).replace(PROMOTED, "")).await
    }

    /// Saves [`Self::tracking`]'s manifest again with `production` promoted
    /// from `staging`, converging the environments it declares.
    async fn declare_promoted(&self) {
        self.harness
            .applications()
            .save(
                parse_template_toml(&shop_toml("", "", IMAGE))
                    .expect("parse template toml succeeds"),
                None,
            )
            .await
            .expect("save succeeds");
    }

    /// Requests a deployment of `production`, without running it.
    async fn deploy_production(&self) -> Operation {
        self.harness
            .applications()
            .deploy(&self.production, None)
            .await
            .expect("deploy succeeds")
    }

    /// Retries `production`'s failed deployment `requested` and asserts it
    /// fails again as `environment_promoted`.
    async fn assert_retry_refused(&self, requested: &Operation) {
        let retried = self
            .harness
            .applications()
            .reconcile(&self.production, None)
            .await
            .expect("reconcile succeeds");
        assert_eq!(retried.id, requested.id);
        self.harness.scan().await;
        let failed = self.harness.operation(&requested.id).await;
        assert_eq!(failed.state, OperationState::Failed);
        assert_eq!(failed.error_code.as_deref(), Some("environment_promoted"));
    }

    /// Deploys `staging` until it succeeds.
    async fn deploy_staging(&self) -> Operation {
        let operation = self
            .harness
            .applications()
            .deploy(&self.staging, None)
            .await
            .expect("deploy succeeds");
        self.harness.finish(&operation).await;
        operation
    }

    /// Promotes `selection` into `production`.
    async fn promote(
        &self,
        selection: PromotionSelection,
    ) -> Result<AcceptedPromotion, ApplicationError> {
        self.harness
            .applications()
            .promote(Daemon, &self.production, selection, None, true, None)
            .await
    }

    /// Promotes the source's current release.
    async fn promote_current(&self) -> Result<AcceptedPromotion, ApplicationError> {
        self.promote(PromotionSelection::Source { deployment: None })
            .await
    }

    /// Promotes the source's current release and runs it until it succeeds.
    async fn promoted(&self) -> AcceptedPromotion {
        let accepted = self
            .promote_current()
            .await
            .expect("promote current succeeds");
        let operation = self
            .harness
            .operation(&accepted.operation.operation_id)
            .await;
        self.harness.finish(&operation).await;
        accepted
    }

    /// The deployment by operation `id`, of whichever environment it ran in.
    async fn deployment(&self, id: &str) -> DeploymentView {
        let environment = self.harness.operation(id).await.environment_id;
        self.harness.deployment(&environment, id).await
    }

    /// The value `environment`'s running `web` gets for `ORIGIN`, and its image.
    async fn web(&self, environment: &EnvironmentId) -> (String, String) {
        let target = self.harness.target(environment).await;
        let web = &target.services[0];
        (web.environment["ORIGIN"].clone(), web.image().to_string())
    }

    async fn release(&self, deployment: &str) -> ReleaseId {
        self.deployment(deployment)
            .await
            .release
            .expect("the release is present")
    }
}

/// The promotion error a request failed with.
fn refusal(result: Result<AcceptedPromotion, ApplicationError>) -> PromotionError {
    match result {
        Err(ApplicationError::Store(StoreError::Promotion(error))) => error,
        other => panic!("promotion refusal expected, got {other:?}"),
    }
}

/// The release staging built is deployed to production as recorded:
/// rendered with production's block, its image never pulled or built
/// again, except a missing registry image, pulled again by digest at
/// acceptance once the request's revision holds. Each deployment records where it came from, and the
/// release where it was promoted.
#[tokio::test]
async fn promotes_the_source_release_rendered_for_the_target_without_building() {
    let shop = Shop::new(&shop_toml("", "", IMAGE)).await;
    let promote = async |generation, request_id| {
        shop.harness
            .applications()
            .promote(
                Daemon,
                &shop.production,
                PromotionSelection::Source { deployment: None },
                Some(generation),
                false,
                request_id,
            )
            .await
    };
    let stale = async || {
        assert!(matches!(
            promote(0, None).await,
            Err(ApplicationError::Store(
                StoreError::GenerationConflict { .. }
            ))
        ));
    };
    stale().await;
    let built = shop.deploy_staging().await;
    let release = shop.release(&built.id).await;
    let image = shop.web(&shop.staging).await.1;
    let next = shop
        .harness
        .store
        .release(&shop.application, &release)
        .await
        .expect("release succeeds")
        .release
        .template()
        .clone();
    let detail = shop
        .harness
        .applications()
        .environment_detail(&shop.production, &piqueld_core::access::Scope::All)
        .await
        .expect("the environment detail loads");
    assert_eq!(detail.manifest, Some(next));
    let pulls = shop.harness.pulls().await;
    shop.harness.docker.local_images.lock().await.clear();
    stale().await;
    let generation = shop.harness.store.get(&shop.production).await;
    let generation = generation
        .expect("the generation is present")
        .application
        .generation;
    assert!(matches!(
        promote(generation, Some("not a key")).await,
        Err(ApplicationError::Store(StoreError::InvalidInput))
    ));
    assert_eq!(shop.harness.pulls().await, pulls);

    let accepted = shop.promoted().await;
    assert_eq!(accepted.release_id, release);
    let origin = DeploymentOrigin::Promotion {
        environment: shop.staging.clone(),
        deployment: built.id.clone(),
    };
    assert_eq!(accepted.origin, origin);
    assert_eq!(shop.harness.pulls().await, pulls + 1);
    assert!(!shop.harness.docker.local_images.lock().await.is_empty());

    assert_eq!(
        shop.web(&shop.production).await,
        ("https://example.com".to_owned(), image.clone())
    );
    assert_eq!(
        shop.web(&shop.staging).await,
        ("https://staging.example.com".to_owned(), image)
    );
    let promoted = shop.deployment(&accepted.operation.operation_id).await;
    assert_eq!(promoted.release, Some(release.clone()));
    assert_eq!(promoted.origin, origin);
    assert_eq!(
        shop.deployment(&built.id).await.origin,
        DeploymentOrigin::Build
    );
    let releases = shop
        .harness
        .store
        .releases(&shop.application, None, 20)
        .await
        .expect("releases succeeds");
    let [listed] = releases.items.as_slice() else {
        panic!("one release expected, got {releases:?}")
    };
    assert_eq!(
        listed
            .promotions
            .iter()
            .map(|promotion| (&promotion.environment_id, &promotion.origin))
            .collect::<Vec<_>>(),
        [(&shop.production, &origin)]
    );
}

/// A later source deployment never retargets an accepted promotion, a
/// reviewed deployment that is no longer current is refused, and an
/// earlier release can be deployed again by ID.
#[tokio::test]
async fn the_release_is_pinned_at_acceptance() {
    let shop = Shop::new(&shop_toml("", "", IMAGE)).await;
    let first = shop.deploy_staging().await;
    let release = shop.release(&first.id).await;
    let applications = shop.harness.applications();
    let promote = |request| {
        applications.promote(
            Daemon,
            &shop.production,
            PromotionSelection::Source {
                deployment: Some(first.id.clone()),
            },
            None,
            true,
            Some(request),
        )
    };
    let accepted = promote("promote-first").await.expect("promote succeeds");

    shop.harness
        .docker
        .registry
        .lock()
        .await
        .digests
        .insert("ghcr.io/example/notes:1.4.0".into(), "b".repeat(64));
    let second = shop.deploy_staging().await;
    assert_ne!(shop.release(&second.id).await, release);
    let promoted = shop
        .harness
        .operation(&accepted.operation.operation_id)
        .await;
    assert_eq!(promoted.state, OperationState::Succeeded);
    assert_eq!(
        shop.deployment(&promoted.id).await.release,
        Some(release.clone())
    );
    assert!(
        shop.web(&shop.production)
            .await
            .1
            .ends_with(&"a".repeat(64))
    );

    assert!(matches!(
        refusal(
            shop.promote(PromotionSelection::Source {
                deployment: Some(first.id.clone()),
            })
            .await
        ),
        PromotionError::SourceChanged { deployment, .. } if deployment == first.id
    ));
    let repeated = promote("promote-first").await.expect("promote succeeds");
    assert_eq!(
        repeated.operation.operation_id,
        accepted.operation.operation_id
    );

    let again = shop
        .promote(PromotionSelection::Release {
            release: release.clone(),
        })
        .await
        .expect("promote succeeds");
    let operation = shop.harness.operation(&again.operation.operation_id).await;
    shop.harness.finish(&operation).await;
    assert_eq!(again.origin, DeploymentOrigin::Release);
    assert_eq!(
        shop.deployment(&operation.id).await.origin,
        DeploymentOrigin::Release
    );
    assert!(
        shop.web(&shop.production)
            .await
            .1
            .ends_with(&"a".repeat(64))
    );
}

/// Every precondition refuses the promotion before anything is captured:
/// a source without a succeeded deployment or with unhealthy services, a
/// build input that renders differently, and missing or inaccessible
/// secrets, all listed at once (also by the plan).
#[tokio::test]
async fn preconditions_are_checked_before_anything_is_captured() {
    let manifest = shop_toml(
        "api='staging-api'",
        "api='production-api'",
        &format!(
            "{IMAGE}\n[[spec.services.secrets]]\nname='${{{{ vars.api }}}}'\ntarget='/run/secrets/api'\n\
             [[spec.services.secrets]]\nname='${{{{ vars.db }}}}'\ntarget='/run/secrets/db'\n\
             [spec.variables]\ndb='shared-db'"
        ),
    );
    let shop = Shop::new(&manifest).await;
    assert!(matches!(
        refusal(shop.promote_current().await),
        PromotionError::SourceNotReady { .. }
    ));
    let applications = shop.harness.applications();
    applications
        .put_stored_secret(
            Daemon,
            &shop.application,
            "staging-api",
            0,
            b"api".to_vec(),
            None,
        )
        .await
        .expect("the secret is stored");
    let staging_only = SecretAccess {
        environments: EnvironmentAccess::Only([shop.staging.clone()].into()),
        previews: false,
    };
    applications
        .put_stored_secret(
            Daemon,
            &shop.application,
            "shared-db",
            0,
            b"db".to_vec(),
            Some(&staging_only),
        )
        .await
        .expect("the secret is stored");
    shop.deploy_staging().await;

    let expected = [
        SecretProblem::Missing {
            secret: "production-api".into(),
        },
        SecretProblem::AccessDenied {
            secret: "shared-db".into(),
        },
    ];
    match refusal(shop.promote_current().await) {
        PromotionError::Secrets { secrets, .. } => assert_eq!(secrets, expected),
        other => panic!("secrets refusal expected, got {other:?}"),
    }
    let plan = applications
        .plan_release(
            &shop.production,
            &PromotionSelection::Source { deployment: None },
        )
        .await
        .expect("plan release succeeds");
    assert_eq!(
        plan.release.expect("the release is present").secrets,
        expected
    );

    shop.harness.docker.observed.lock().await.services[0].convergence = Convergence::Degraded;
    assert!(matches!(
        refusal(shop.promote_current().await),
        PromotionError::SourceNotReady { .. }
    ));
    {
        let mut observed = shop.harness.docker.observed.lock().await;
        let web = &mut observed.services[0];
        web.convergence = Convergence::Converged;
        web.image = format!("ghcr.io/example/notes@sha256:{}", "f".repeat(64));
    }
    assert!(matches!(
        refusal(shop.promote_current().await),
        PromotionError::SourceNotReady { .. }
    ));
    assert!(
        shop.harness
            .store
            .latest_operation_for_environment(&shop.production)
            .await
            .expect("latest operation for environment succeeds")
            .is_none()
    );
}

/// The environment's declared secrets are generated for a promotion, but
/// stored only once it is accepted.
#[tokio::test]
async fn generated_secrets_are_stored_with_the_accepted_promotion() {
    let manifest = shop_toml(
        "",
        "",
        &format!(
            "{IMAGE}\n[[spec.services.secrets]]\nname='session'\ntarget='/run/secrets/session'\n\
             [[spec.secrets]]\nname='session'\n[spec.secrets.generate]\ntype='random'\nbytes=16"
        ),
    );
    let shop = Shop::new(&manifest).await;
    shop.deploy_staging().await;
    let applications = shop.harness.applications();
    let current = PromotionSelection::Source { deployment: None };
    assert!(matches!(
        applications
            .promote(
                Daemon,
                &shop.production,
                current.clone(),
                Some(0),
                false,
                None
            )
            .await,
        Err(ApplicationError::Store(
            StoreError::GenerationConflict { .. }
        ))
    ));
    assert!(
        shop.harness
            .store
            .secrets(&shop.production)
            .await
            .expect("the secrets list")
            .is_empty()
    );
    shop.promoted().await;
    let stored = shop
        .harness
        .store
        .secrets(&shop.production)
        .await
        .expect("the secrets list");
    assert_eq!(
        stored
            .iter()
            .map(|secret| secret.name.as_str())
            .collect::<Vec<_>>(),
        ["session"]
    );
    let plan = applications
        .plan_release(&shop.production, &current)
        .await
        .expect("plan release succeeds");
    assert!(plan.identical, "{:?}", plan.changes);
    assert!(
        !plan
            .plan
            .actions
            .iter()
            .any(|action| matches!(action.kind, piqueld_core::ActionKind::EnsureService { .. })),
        "{:?}",
        plan.plan.actions
    );
}

/// A build input production renders differently can't reuse the
/// release's image.
#[tokio::test]
async fn a_release_whose_build_inputs_render_differently_is_refused() {
    let tagged = shop_toml(
        "",
        "tag='1.5.0'",
        "[spec.services.source]\ntype='image'\nimage='ghcr.io/example/notes:${{ vars.tag }}'\n\
         [spec.variables]\ntag='1.4.0'",
    );
    let shop = Shop::new(&tagged).await;
    shop.deploy_staging().await;
    match shop.promote_current().await {
        Err(ApplicationError::Store(StoreError::Validation(errors))) => assert_eq!(
            errors
                .0
                .iter()
                .map(|error| (error.code.as_str(), error.path.as_str()))
                .collect::<Vec<_>>(),
            [("release_incompatible", "web.source.image")]
        ),
        other => panic!("release_incompatible expected, got {other:?}"),
    }
}

/// An environment can't become promoted while a deployment may be
/// fetching or building: its declaration waits for the next convergence.
/// Retrying a deployment that never prepared fails instead of fetching or
/// building once it is promoted.
#[tokio::test]
async fn a_promoted_environment_never_builds_a_deployment_requested_before() {
    let shop = Shop::tracking().await;
    let requested = shop.deploy_production().await;
    let source = || async {
        shop.harness
            .store
            .get(&shop.production)
            .await
            .expect("the item exists")
            .environment
            .source
            .promoted_from()
            .cloned()
    };
    shop.declare_promoted().await;
    assert_eq!(source().await, None);

    shop.harness
        .docker
        .incompatible_swarm
        .store(true, Ordering::SeqCst);
    shop.harness.scan().await;
    let failed = shop.harness.operation(&requested.id).await;
    assert_eq!(failed.state, OperationState::Failed);
    shop.harness
        .docker
        .incompatible_swarm
        .store(false, Ordering::SeqCst);
    shop.declare_promoted().await;
    assert_eq!(source().await, Some(shop.staging.clone()));

    let pulls = shop.harness.pulls().await;
    shop.assert_retry_refused(&requested).await;
    assert_eq!(shop.harness.pulls().await, pulls);
}

/// An environment made promoted still repairs drift in the build it
/// runs: converging its active target again rolls nothing new in.
#[tokio::test]
async fn a_promoted_environment_repairs_the_build_it_runs() {
    let shop = Shop::tracking().await;
    let deployed = shop.deploy_production().await;
    shop.harness.finish(&deployed).await;
    shop.declare_promoted().await;
    let production = shop.production.to_string();
    let web = async || {
        let mut observed = shop.harness.docker.observed.lock().await;
        let web = observed
            .services
            .iter_mut()
            .find(|service| service.labels.get(APPLICATION_LABEL) == Some(&production))
            .expect("the item exists");
        let replicas = web.replicas;
        web.replicas = 0;
        web.convergence = Convergence::Degraded;
        replicas
    };
    web().await;
    shop.harness.finish(&deployed).await;
    assert_eq!(web().await, 1);
}

/// A build that prepared its target before the environment was made
/// promoted doesn't roll out into it when retried: a promoted
/// environment only runs releases.
#[tokio::test]
async fn a_promoted_environment_never_rolls_out_a_build_prepared_before() {
    let mut shop = Shop::tracking().await;
    shop.harness.set_convergence_timeout(Duration::from_secs(1));
    let requested = shop.deploy_production().await;
    shop.harness
        .docker
        .fail_observations
        .store(true, Ordering::SeqCst);
    shop.harness.scan().await;
    assert_eq!(
        shop.harness.operation(&requested.id).await.state,
        OperationState::Failed
    );
    assert!(
        shop.harness
            .store
            .prepared_target(&requested.id)
            .await
            .expect("prepared target succeeds")
            .is_some()
    );
    shop.harness
        .docker
        .fail_observations
        .store(false, Ordering::SeqCst);
    shop.declare_promoted().await;

    shop.assert_retry_refused(&requested).await;
    assert!(
        shop.harness
            .store
            .get(&shop.production)
            .await
            .expect("the item exists")
            .resolved
            .is_none()
    );
}
