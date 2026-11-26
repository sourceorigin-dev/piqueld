pub(crate) use super::Schedule;
use super::*;
use crate::api::PreviewMutation;
use parking_lot::Mutex;
use piqueld_core::{EnvironmentId, EnvironmentName, TrackedBranch, manifest::parse_template_toml};
use std::sync::Arc;

pub(crate) const OLD: &str = "1111111111111111111111111111111111111111";
pub(crate) const NEW: &str = "2222222222222222222222222222222222222222";
const URL: &str = "https://example.com/notes.git";

/// A service over a fresh store, with `notes` syncing as `sync`.
pub(crate) struct Fixture {
    pub(crate) service: ApplicationService,
    pub(crate) application: ApplicationId,
    _temp: tempfile::TempDir,
}

impl Fixture {
    pub(crate) async fn new(sync: &str) -> Self {
        let temp = tempfile::tempdir().expect("the temp dir is created");
        let store = Arc::new(
            piqueld_store::Store::open_unsynced(temp.path().join("db"))
                .await
                .expect("the store opens"),
        );
        let service = ApplicationService::unconnected(store, temp.path()).await;
        Self {
            application: Self::save(&service, "notes", sync).await,
            service,
            _temp: temp,
        }
    }

    /// Saves application `name` reading `main` of the shared repository
    /// and syncing as `sync`, returning its ID.
    pub(crate) async fn save(
        service: &ApplicationService,
        name: &str,
        sync: &str,
    ) -> ApplicationId {
        let template = parse_template_toml(&format!(
            "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='{name}'\n[spec.manifest]\npath='app.toml'\nsync={sync}\n[spec.manifest.repository]\nurl='{URL}'\nbranch='main'\n"
        ))
        .expect("parse template toml succeeds")
        .normalize(ApplicationId::parse("app-pending-01").expect("the application id parses"));
        let save = Mutation::Save {
            application: Box::new(template),
            expected_application_id: None,
            deploy: false,
        };
        let MutationResponse::Saved(saved) = service
            .accept(Actor::Daemon, save, Some(0), false, None)
            .await
            .expect("the connection is accepted")
        else {
            panic!("saved")
        };
        ApplicationId::parse(saved.application_id).expect("the application id parses")
    }

    /// The application's first environment, `production` following
    /// `main`, opted in and deployed at `OLD`.
    pub(crate) async fn production(&self) -> EnvironmentId {
        let store = &self.service.store;
        let production = store
            .environments(&self.application)
            .await
            .expect("the environments load")[0]
            .id
            .clone();
        self.follow(&production).await;
        production
    }

    /// Opts environment `id` in and deploys it at `OLD`.
    pub(crate) async fn follow(&self, id: &EnvironmentId) {
        let opt_in = Mutation::SetSync {
            id: id.clone(),
            enabled: true,
        };
        self.service
            .accept(Actor::Daemon, opt_in, None, false, None)
            .await
            .expect("the connection is accepted");
        self.service.store.deployed_at(id, OLD).await;
    }

    /// Adds an environment following `branch`, opted in and deployed at `OLD`.
    pub(crate) async fn environment(&self, name: &str, branch: &str) -> EnvironmentId {
        let create = Mutation::CreateEnvironment {
            application: self.application.clone(),
            name: EnvironmentName::parse(name).expect("the environment name parses"),
            source: Some(crate::api::SourceChoice::Branch(
                TrackedBranch::new(branch.into(), None).expect("the value is valid"),
            )),
        };
        let MutationResponse::Environment(environment) = self
            .service
            .accept(Actor::Daemon, create, None, true, None)
            .await
            .expect("the connection is accepted")
        else {
            panic!("environment")
        };
        self.follow(&environment.id).await;
        environment.id
    }

    /// Adds a preview of `branch`, deployed at `OLD`.
    async fn preview(&self, branch: &str) -> EnvironmentId {
        let create = Mutation::Preview(PreviewMutation::Create {
            application: self.application.clone(),
            branch: GitBranch::parse(branch).expect("the git branch parses"),
            slot: None,
        });
        let MutationResponse::Preview(created) = self
            .service
            .accept(Actor::Daemon, create, None, false, None)
            .await
            .expect("the connection is accepted")
        else {
            panic!("preview")
        };
        self.service
            .store
            .deployed_at(&created.preview.id, OLD)
            .await;
        created.preview.id
    }

    /// The commits sync deployed to `id`, newest first, with their actor.
    pub(crate) async fn synced(&self, id: &EnvironmentId) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT json_extract(i.application_json,'$.spec.manifest.repository.commit'),o.actor_system FROM operations o JOIN deployment_inputs i ON i.operation_id=o.id WHERE o.environment_id=?1 AND o.actor_system IS NOT NULL ORDER BY o.created_at_ms DESC,o.id DESC",
        )
        .bind(id.as_str())
        .fetch_all(self.service.store.pool())
        .await
        .expect("fetch all succeeds")
    }
}

/// Answers listings with `heads`, or fails, counting them per URL.
#[derive(Default)]
pub(crate) struct Fake {
    pub(crate) heads: Mutex<Option<Vec<(&'static str, &'static str)>>>,
    pub(crate) listed: Mutex<Vec<String>>,
}

impl Fake {
    pub(crate) fn with(heads: Vec<(&'static str, &'static str)>) -> Self {
        Self {
            heads: Mutex::new(Some(heads)),
            listed: Mutex::default(),
        }
    }

    pub(crate) fn listings(&self) -> usize {
        self.listed.lock().len()
    }
}

impl Lister for Fake {
    fn list(&self, url: &str) -> impl Future<Output = anyhow::Result<Heads>> + Send {
        self.listed.lock().push(url.to_owned());
        let heads = self.heads.lock().clone();
        let url = url.to_owned();
        async move {
            let heads =
                heads.ok_or_else(|| anyhow::anyhow!("git ls-remote failed (exit status: 128)"))?;
            Ok(Heads::new(
                &url,
                heads
                    .into_iter()
                    .map(|(branch, head)| (branch.into(), head.into())),
            ))
        }
    }
}

#[tokio::test]
async fn polling_lists_a_repository_once_for_every_application_and_deploys_only_what_moved() {
    let fixture = Fixture::new("{mode='poll',interval_seconds=60}").await;
    let production = fixture.production().await;
    let wiki = Fixture::save(
        &fixture.service,
        "wiki",
        "{mode='poll',interval_seconds=600}",
    )
    .await;
    let wiki = fixture
        .service
        .store
        .environments(&wiki)
        .await
        .expect("the environments load")[0]
        .id
        .clone();
    fixture.follow(&wiki).await;
    let staging = fixture.environment("staging", "develop").await;
    let pinned = fixture.environment("pinned", "main").await;
    let pin = Mutation::SetBranch {
        id: pinned.clone(),
        branch: TrackedBranch::new("main".into(), Some(OLD.into())).expect("the value is valid"),
    };
    fixture
        .service
        .accept(Actor::Daemon, pin, None, true, None)
        .await
        .expect("the connection is accepted");
    let manual = fixture.environment("manual", "main").await;
    let opt_out = Mutation::SetSync {
        id: manual.clone(),
        enabled: false,
    };
    fixture
        .service
        .accept(Actor::Daemon, opt_out, None, false, None)
        .await
        .expect("the connection is accepted");
    let moved = fixture.preview("feat/login").await;
    let gone = fixture.preview("feat/gone").await;
    let lister = Fake::with(vec![("main", NEW), ("develop", OLD), ("feat/login", NEW)]);
    let mut schedule = Schedule::default();
    let now = Instant::now();
    fixture.service.sync_due(&lister, &mut schedule, now).await;

    assert_eq!(lister.listings(), 1);
    let deployed = vec![(NEW.to_owned(), "sync:poll".to_owned())];
    assert_eq!(fixture.synced(&production).await, deployed);
    assert_eq!(fixture.synced(&wiki).await, deployed);
    assert_eq!(fixture.synced(&moved).await, deployed);
    for unchanged in [&staging, &pinned, &manual, &gone] {
        assert_eq!(fixture.synced(unchanged).await, []);
    }
    fixture
        .service
        .store
        .get(&gone)
        .await
        .expect("`get` should succeed");

    fixture
        .service
        .sync_due(&lister, &mut schedule, now + Duration::from_secs(59))
        .await;
    assert_eq!(lister.listings(), 1);
    fixture
        .service
        .sync_due(&lister, &mut schedule, now + Duration::from_secs(90))
        .await;
    assert_eq!(lister.listings(), 2);
    assert_eq!(fixture.synced(&production).await.len(), 1);
}

#[tokio::test]
async fn failed_listings_deploy_nothing_and_back_off() {
    let fixture = Fixture::new("{mode='poll',interval_seconds=60}").await;
    let production = fixture.production().await;
    let lister = Fake::default();
    let mut schedule = Schedule::default();
    let now = Instant::now();
    fixture.service.sync_due(&lister, &mut schedule, now).await;
    assert_eq!(lister.listings(), 1);
    assert_eq!(fixture.synced(&production).await, []);
    let check = fixture
        .service
        .store
        .sync_check(&fixture.application)
        .await
        .expect("sync check succeeds")
        .expect("the check has a result");
    assert!(
        check
            .error
            .expect("the error is set")
            .contains("ls-remote failed")
    );

    fixture
        .service
        .sync_due(&lister, &mut schedule, now + Duration::from_secs(59))
        .await;
    assert_eq!(lister.listings(), 1);
    fixture
        .service
        .sync_due(&lister, &mut schedule, now + Duration::from_secs(90))
        .await;
    assert_eq!(lister.listings(), 2);
    fixture
        .service
        .sync_due(&lister, &mut schedule, now + Duration::from_secs(180))
        .await;
    assert_eq!(lister.listings(), 2);

    *lister.heads.lock() = Some(vec![("main", NEW)]);
    fixture
        .service
        .sync_due(&lister, &mut schedule, now + Duration::from_secs(240))
        .await;
    assert_eq!(fixture.synced(&production).await.len(), 1);
    let check = fixture
        .service
        .store
        .sync_check(&fixture.application)
        .await
        .expect("sync check succeeds")
        .expect("the check has a result");
    assert_eq!(check.error, None);
}
