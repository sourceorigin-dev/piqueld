//! Fixtures shared by the store's tests.
use super::{
    Actor, ApplicationId, Caller, CredentialKind, MIGRATIONS, NewCredential, OperationState, Store,
    StoreError,
};
use crate::{Mutation, MutationResponse};
use piqueld_core::{
    EnvironmentId, NormalizedApplication, access::Grants, api::SavedApplication, auth::User,
    manifest::ValidatedTemplate,
};
use sqlx::{SqlitePool, sqlite::SqlitePoolOptions};
use std::path::Path;

/// Parses the application manifest named `name`, with `spec`, TOML following
/// its metadata.
pub(super) fn template(name: &str, spec: &str) -> ValidatedTemplate {
    piqueld_core::manifest::parse_template_toml(&format!(
        "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='{name}'\n{spec}"
    ))
    .expect("parse template toml succeeds")
}

/// The `notes` application of core's `prebuilt.toml` fixture, whose `web`
/// service pulls `ghcr.io/example/notes:1.4.0`, under `id`.
pub(super) fn prebuilt(id: &str) -> NormalizedApplication {
    piqueld_core::parse_toml(include_str!(
        "../../../crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml"
    ))
    .expect("parse toml succeeds")
    .normalize(ApplicationId::parse(id).expect("the application id parses"))
}

/// Renames application `id` to `name` through the name edit, as the CLI and
/// the dashboard do.
pub(super) fn rename_application(id: &ApplicationId, name: &str) -> Mutation {
    Mutation::Edit {
        id: id.clone(),
        edit: Box::new(piqueld_core::edit::ApplicationEdit::Name(name.into())),
        deploy: false,
    }
}

/// The account `name`, its username too.
pub(super) fn user(name: &str) -> User {
    User {
        id: name.into(),
        username: name.into(),
        display_name: String::new(),
    }
}

/// A credential of `kind` with no expiry, limits or passkey proof, named and
/// hashed after its ID.
pub(super) fn credential(id: &str, kind: CredentialKind) -> NewCredential<'_> {
    NewCredential {
        id: id.into(),
        secret_hash: id.into(),
        kind,
        name: id,
        expires_at: None,
        grants: None,
        authenticated_at: None,
    }
}

/// `credential`, which a passkey proved just now, as a passkey sign-in or a
/// device approval records it, so it may take sensitive actions.
pub(super) fn proved(credential: NewCredential<'_>) -> NewCredential<'_> {
    NewCredential {
        authenticated_at: Some(crate::now_secs()),
        ..credential
    }
}

/// An API request by `alice` on `route`, whose changes record themselves.
pub(super) fn audit_request(route: &str) -> crate::AuditRequest {
    crate::AuditRequest {
        route: route.into(),
        origin: crate::AuditOrigin {
            user_id: Some("alice".into()),
            ..crate::AuditOrigin::default()
        },
        status: None,
        recorded: crate::Recorded::default(),
    }
}

impl Store {
    /// Makes every audit append fail while `refused`, as a full disk would:
    /// a record at the highest possible ID leaves none for the next one.
    pub(super) async fn refuse_audit(&self, refused: bool) {
        let change = if refused {
            "INSERT INTO audit_events(id,created_at_ms,action,outcome,status) \
            VALUES(9223372036854775807,0,'refused','failed',503)"
        } else {
            "DELETE FROM audit_events WHERE id=9223372036854775807"
        };
        sqlx::query(change)
            .execute(&self.pool)
            .await
            .expect("execute succeeds");
    }

    /// The audit trail's actions, oldest first.
    pub(super) async fn audit_actions(&self) -> Vec<String> {
        sqlx::query_scalar("SELECT action FROM audit_events ORDER BY id")
            .fetch_all(&self.pool)
            .await
            .expect("fetch all succeeds")
    }

    /// The ID of the `production` environment of the application `saved`.
    pub(crate) async fn saved_production(&self, saved: &SavedApplication) -> EnvironmentId {
        let application = saved.application_id.clone();
        self.production(&application).await
    }

    /// Opens a store in a new temporary directory, which must outlive it.
    pub(super) async fn temporary() -> (tempfile::TempDir, Self) {
        let directory = tempfile::tempdir().expect("tempdir succeeds");
        let store = Self::open_unsynced(directory.path().join("db"))
            .await
            .expect("open unsynced succeeds");
        (directory, store)
    }

    /// Opens the database at `path` migrated to schema `version`, as an
    /// older daemon left it.
    pub(super) async fn migrated(path: &Path, version: usize) -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(Self::connect_options(path, super::Durability::Unsynced))
            .await
            .expect("connect with succeeds");
        for (index, migration) in MIGRATIONS.iter().take(version).enumerate() {
            Self::apply_migration(&pool, index + 1, migration)
                .await
                .expect("apply migration succeeds");
        }
        pool
    }

    /// Accepts `mutation` as the daemon, forced past revision checks.
    pub(super) async fn force(&self, mutation: Mutation) -> Result<MutationResponse, StoreError> {
        let (response, _) = self
            .accept(Actor::Daemon, mutation, None, true, None)
            .await?;
        Ok(response)
    }

    /// Creates the application `template` as the daemon, deploying it when
    /// `deploy`; it must not exist yet.
    pub(super) async fn save_template(
        &self,
        template: ValidatedTemplate,
        deploy: bool,
    ) -> SavedApplication {
        self.saved(template, deploy, (Some(0), false)).await
    }

    /// Saves `template` as the daemon, deploying it when `deploy`, over the
    /// application of that name if any, forced past revision checks.
    pub(super) async fn save_template_forced(
        &self,
        template: ValidatedTemplate,
        deploy: bool,
    ) -> SavedApplication {
        self.saved(template, deploy, (None, true)).await
    }

    /// Saves `template` with the `expected` revision and `force` of
    /// [`Store::accept`].
    async fn saved(
        &self,
        template: ValidatedTemplate,
        deploy: bool,
        (expected, force): (Option<u64>, bool),
    ) -> SavedApplication {
        let save = Mutation::save(template, None, deploy);
        let (MutationResponse::Saved(saved), _) = self
            .accept(Actor::Daemon, save, expected, force, None)
            .await
            .expect("accept succeeds")
        else {
            panic!("saved response expected")
        };
        saved
    }

    /// Marks requested operation `id` running, as the controller does when
    /// it starts executing it.
    pub(super) async fn mark_running(&self, id: &str) {
        self.transition_operation(id, OperationState::Requested, OperationState::Running, None)
            .await
            .expect("transition operation succeeds");
    }

    /// Seeds the administrator `user`, signed in with the login `user`,
    /// which adds the passkey `<user>-key`, so it may take changes that
    /// must leave an administrator able to sign in. Returns that login.
    pub(super) async fn admin_with_passkey(&self, user: &'static str) -> Caller<'static> {
        let Actor::Account(caller) = self.seed_caller(user, &Grants::admin()).await else {
            panic!("seeded an account caller")
        };
        let key = format!("{user}-key");
        let passkey = crate::NewPasskey {
            id: &key,
            name: &key,
            credential: "{}",
        };
        let own = crate::PasskeyOwner::Existing(caller, user);
        let added = self.add_passkey(None, own, passkey).await;
        assert!(added.expect("an account adds its own passkey"));
        caller
    }

    /// Seeds the account `name` holding `grants`, with an unscoped CLI
    /// session also named `name` that a passkey just proved, and returns the
    /// caller acting with it.
    pub(super) async fn seed_caller(&self, name: &'static str, grants: &Grants) -> Actor<'static> {
        self.seed_auth_user(&user(name), grants).await;
        self.insert_credential(name, &proved(credential(name, CredentialKind::Cli)))
            .await
            .expect("insert credential succeeds");
        Actor::Account(Caller {
            credential_id: name,
            user_id: name,
        })
    }
}
