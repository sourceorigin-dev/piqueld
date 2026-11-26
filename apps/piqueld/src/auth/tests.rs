use super::*;
use crate::api::http::Authenticator as _;
use piqueld_core::access::{
    Denied, GlobalPermission, Grants, Permission, Preset, Reauthenticate, Scope,
};
use piqueld_core::auth::{
    HostOperator, Manage, OperatorLink, RegistrationStart, SensitiveAction, User,
};
use piqueld_store::{Invitation, Lockout, NewPasskey, PasskeyOwner, StoreError};

/// The `public_url` every fixture's `Auth` serves.
const ORIGIN: &str = "http://localhost:7845";

impl Identity {
    /// The account behind an identity a test signed in as.
    fn account(&self) -> &User {
        self.user().expect("an account identity")
    }

    /// The ID of the account behind an identity a test signed in as.
    fn id(&self) -> String {
        self.account().id.clone()
    }
}

struct Fixture {
    auth: Auth,
    dir: tempfile::TempDir,
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().expect("the temp dir is created");
        let store = piqueld_store::Store::open_unsynced(dir.path().join("state.db"))
            .await
            .expect("the store opens");
        Self {
            auth: Auth::new(&store, ORIGIN).expect("the value is valid"),
            dir,
        }
    }
    /// Seeds an account holding `grants`, returning its ID and a credential.
    /// Browser sessions and CLI logins have just signed in with a passkey.
    async fn account(
        &self,
        name: &str,
        kind: CredentialKind,
        expires: Option<i64>,
        grants: &Grants,
    ) -> (String, String) {
        let user = User {
            id: Auth::id(),
            username: name.into(),
            display_name: String::new(),
        };
        self.auth.0.store.seed_auth_user(&user, grants).await;
        let (token, mut credential) = Auth::new_credential(kind, "Test", expires, None)
            .expect("the system RNG yields a credential secret");
        credential.authenticated_at = (kind != CredentialKind::Token).then(now_secs);
        self.auth
            .0
            .store
            .insert_credential(&user.id, &credential)
            .await
            .expect("insert credential succeeds");
        (user.id, token)
    }

    /// Seeds an account holding `grants` behind a non-expiring CLI session
    /// with the account's full access, and returns its identity.
    async fn identity(&self, name: &str, grants: &Grants) -> Identity {
        let (_, identity) = self
            .signed_in(name, CredentialKind::Cli, None, grants)
            .await;
        identity
    }

    /// Seeds an account holding `grants` behind a credential of `kind`
    /// expiring at `expires`, returning the credential and its identity.
    async fn signed_in(
        &self,
        name: &str,
        kind: CredentialKind,
        expires: Option<i64>,
        grants: &Grants,
    ) -> (String, Identity) {
        let (_, token) = self.account(name, kind, expires, grants).await;
        let identity = self
            .auth
            .authenticate(&token)
            .await
            .expect("authenticate succeeds");
        (token, identity)
    }

    /// Has `owner` create a token with `create`, returning its secret and
    /// its identity.
    async fn token(&self, owner: &Identity, create: Manage) -> (String, Identity) {
        let managed = self
            .auth
            .manage(owner, None, create)
            .await
            .expect("manage succeeds");
        let secret = managed.token.expect("token succeeds");
        let identity = self
            .auth
            .authenticate(&secret)
            .await
            .expect("authenticate succeeds");
        (secret, identity)
    }

    /// Has `by` create the invitation or enrollment link `create`, returning
    /// its secret.
    async fn link(&self, by: &Identity, create: Manage) -> String {
        let fragment = match create {
            Manage::CreateInvitation { .. } => "#invite=",
            Manage::CreateEnrollment { .. } => "#enroll=",
            _ => panic!("not a link: {create:?}"),
        };
        let managed = self
            .auth
            .manage(by, None, create)
            .await
            .expect("manage succeeds");
        let url = managed.invitation_url.expect("invitation url succeeds");
        url.split_once(fragment)
            .expect("the text splits")
            .1
            .to_owned()
    }

    /// Lets the CLI login started with `device_code` poll again at once.
    fn allow_poll(&self, device_code: &str) {
        let key = Auth::hash(device_code);
        self.auth
            .0
            .devices
            .lock()
            .get_mut(&key)
            .expect("the entry exists")
            .next_poll = 0;
    }

    /// Opens a browser session for `identity`'s account that no passkey
    /// signed in, like one opened by redeeming a link, returning its secret
    /// and identity.
    async fn stale_session(&self, identity: &Identity) -> (String, Identity) {
        self.stale(
            identity,
            Auth::browser_session(None).expect("the system RNG yields a credential secret"),
        )
        .await
    }

    /// Adds a CLI login to `identity`'s account that no passkey proved
    /// recently, returning its secret and identity.
    async fn stale_login(&self, identity: &Identity) -> (String, Identity) {
        let login = Auth::new_credential(CredentialKind::Cli, "piquelctl", None, None);
        let login = login.expect("the system RNG yields a credential secret");
        self.stale(identity, login).await
    }

    /// Stores the credential with `secret` for `identity`'s account,
    /// returning the secret and its identity.
    async fn stale(
        &self,
        identity: &Identity,
        (secret, credential): (String, NewCredential<'static>),
    ) -> (String, Identity) {
        self.auth
            .0
            .store
            .insert_credential(&identity.account().id, &credential)
            .await
            .expect("the store keeps the credential");
        let identity = self
            .auth
            .authenticate(&secret)
            .await
            .expect("the stored credential authenticates");
        (secret, identity)
    }

    /// Confirms `identity`'s credential with `passkey`, as a verified
    /// assertion would.
    async fn confirm(
        &self,
        identity: &Identity,
        passkey: &str,
    ) -> std::result::Result<bool, StoreError> {
        let caller = identity.caller().expect("the identity is an account's");
        let verify = |stored: &str| Ok::<_, StoreError>(stored.to_owned());
        let store = &self.auth.0.store;
        store
            .confirm_with_passkey(caller, passkey, verify, now_secs())
            .await
    }

    /// Whether the credential with `secret` may take sensitive actions now.
    async fn fresh(&self, secret: &str) -> bool {
        let identity = self
            .auth
            .authenticate(secret)
            .await
            .expect("the stored credential authenticates");
        identity.require_fresh().is_ok()
    }

    /// Reads an account's current profile and grants.
    async fn reload(&self, identity: &Identity) -> piqueld_core::auth::Account {
        self.auth
            .directory(identity)
            .await
            .expect("directory succeeds")
            .users
            .into_iter()
            .find(|account| account.user.id == identity.account().id)
            .expect("the match exists")
    }

    /// Management tests only need a stored key; no WebAuthn ceremony reads it.
    async fn passkey(&self, owner: &Identity) -> String {
        let id = Auth::id();
        let passkey = NewPasskey {
            id: &id,
            name: "Test key",
            credential: "{}",
        };
        let store = &self.auth.0.store;
        assert!(
            store
                .add_passkey(
                    None,
                    PasskeyOwner::Existing(
                        owner.caller().expect("caller succeeds"),
                        &owner.account().id
                    ),
                    passkey,
                )
                .await
                .expect("add passkey succeeds")
        );
        id
    }

    /// Redeems the invitation or enrollment link `secret` for `user` in a
    /// new browser session, returning whether it added a passkey.
    async fn redeem(&self, user: &User, secret: &str) -> bool {
        let (_, session) = Auth::browser_session(None).expect("browser session succeeds");
        let owner = PasskeyOwner::Redeem {
            user,
            invitation_hash: &Auth::hash(secret),
            session,
        };
        let passkey = NewPasskey {
            id: &Auth::id(),
            name: "Key",
            credential: "{}",
        };
        self.auth
            .0
            .store
            .add_passkey(None, owner, passkey)
            .await
            .expect("add passkey succeeds")
    }
}

/// A new account signing up through a link.
fn carol() -> User {
    User {
        id: Auth::id(),
        username: "carol".into(),
        display_name: String::new(),
    }
}

/// A token named `name` holding `grants`, expiring after `days`.
fn create_token(grants: Grants, name: &str, days: Option<u32>) -> Manage {
    Manage::CreateToken {
        grants,
        name: name.into(),
        days,
        tailnet: None,
    }
}

/// Sets `user`'s grants to `grants`.
fn set_grants(user: &Identity, grants: Grants) -> Manage {
    Manage::SetGrants {
        user_id: user.id(),
        grants,
    }
}

/// Every grant on every application, except managing accounts.
fn developer() -> Grants {
    Preset::Developer.grants(&Scope::All)
}

/// Developer access plus managing accounts it covers.
fn team_lead() -> Grants {
    let mut grants = developer();
    grants
        .grant(
            Permission::Global(GlobalPermission::AccountsManage),
            &Scope::All,
        )
        .expect("grant succeeds");
    grants
}

fn denied(result: Result<piqueld_core::auth::Managed>) -> Denied {
    denied_auth(result.map(|_| ()))
}

/// Why an authentication action was refused.
fn denied_auth(result: Result<()>) -> Denied {
    match result {
        Err(AuthError::Denied(denied) | AuthError::Store(StoreError::Denied(denied))) => denied,
        other => panic!("expected a refusal: {other:?}"),
    }
}

#[tokio::test]
async fn sessions_expire_revoke_and_survive_restart_without_storing_secrets() {
    let f = Fixture::new().await;
    let (id, token) = f
        .account(
            "alice",
            CredentialKind::Browser,
            Some(now_secs() + 7 * DAY),
            &Grants::admin(),
        )
        .await;
    let identity = f
        .auth
        .authenticate(&token)
        .await
        .expect("authenticate succeeds");
    assert_eq!(identity.account().id, id);
    assert_eq!(identity.grants, Grants::admin());
    assert!(
        f.auth
            .0
            .store
            .credential_owner(&token)
            .await
            .expect("credential owner succeeds")
            .is_none()
    );
    let store = piqueld_store::Store::open_unsynced(f.dir.path().join("state.db"))
        .await
        .expect("the store opens");
    let restarted = Auth::new(&store, ORIGIN).expect("the value is valid");
    restarted
        .authenticate(&token)
        .await
        .expect("authenticate succeeds");
    f.auth.0.store.age_auth_credentials(DAY).await;
    assert!(matches!(
        f.auth.authenticate(&token).await,
        Err(AuthError::Unauthorized)
    ));
    let (_, cli) = f
        .account(
            "bob",
            CredentialKind::Cli,
            Some(now_secs() - 1),
            &Grants::admin(),
        )
        .await;
    assert!(matches!(
        f.auth.authenticate(&cli).await,
        Err(AuthError::Unauthorized)
    ));
    let (other, api) = f
        .account("carol", CredentialKind::Token, None, &developer())
        .await;
    let admin = f.identity("dave", &Grants::admin()).await;
    f.auth
        .manage(
            &admin,
            None,
            Manage::RevokeAll {
                user_id: other,
                cascade: true,
            },
        )
        .await
        .expect("manage succeeds");
    assert!(matches!(
        f.auth.authenticate(&api).await,
        Err(AuthError::Unauthorized)
    ));
}

#[tokio::test]
async fn accounts_manage_themselves_and_last_admin_deletion_is_atomic() {
    let f = Fixture::new().await;
    let alice = f.identity("alice", &Grants::admin()).await;
    let bob = f.identity("bob", &Grants::admin()).await;
    f.auth
        .manage(
            &bob,
            None,
            Manage::UpdateUser {
                user_id: bob.id(),
                username: "robert".into(),
                display_name: "Bob".into(),
            },
        )
        .await
        .expect("manage succeeds");
    assert_eq!(f.reload(&bob).await.user.username, "robert");
    let input = |user_id: &str| RegistrationStart {
        invitation: None,
        user_id: Some(user_id.into()),
        username: String::new(),
        display_name: String::new(),
        passkey_name: "Authenticator".into(),
    };
    f.auth
        .registration_start(input(&bob.account().id), "binding", Some(&alice))
        .await
        .expect_err("`` should be rejected");
    f.auth
        .registration_start(input(&alice.account().id), "binding", Some(&alice))
        .await
        .expect("`` should succeed");
    let (_, made) = f
        .token(&bob, create_token(Grants::admin(), "automation", None))
        .await;
    assert_eq!(made.id(), bob.id());
    f.passkey(&alice).await;
    f.passkey(&bob).await;
    let (first, second) = tokio::join!(
        f.auth.manage(
            &alice,
            None,
            Manage::DeleteUser {
                user_id: bob.id(),
                cascade: true
            }
        ),
        f.auth.manage(
            &alice,
            None,
            Manage::DeleteUser {
                user_id: alice.id(),
                cascade: true
            }
        )
    );
    assert_ne!(first.is_ok(), second.is_ok());
    assert_eq!(
        f.auth
            .directory(&alice)
            .await
            .expect("directory succeeds")
            .users
            .len(),
        1
    );
    assert!(f.auth.status().await.expect("status succeeds").initialized);
}

/// Changing another account needs `accounts:manage` and every grant it holds,
/// and grants can only be handed out by someone holding them.
#[tokio::test]
async fn managers_change_only_accounts_and_grants_they_cover() {
    let f = Fixture::new().await;
    let admin = f.identity("admin", &Grants::admin()).await;
    f.passkey(&admin).await;
    let lead = f.identity("lead", &team_lead()).await;
    let dev = f.identity("dev", &developer()).await;
    let viewer = f
        .identity("viewer", &Preset::ReadOnly.grants(&Scope::All))
        .await;
    let rename = |user: &Identity| Manage::UpdateUser {
        user_id: user.id(),
        username: format!("{}-renamed", user.account().username),
        display_name: String::new(),
    };
    assert_eq!(
        denied(f.auth.manage(&dev, None, rename(&viewer)).await),
        Denied::Missing(Permission::Global(GlobalPermission::AccountsManage))
    );
    assert_eq!(
        denied(f.auth.manage(&lead, None, rename(&admin)).await),
        Denied::Exceeds
    );
    f.auth
        .manage(&lead, None, rename(&viewer))
        .await
        .expect("manage succeeds");
    f.auth
        .manage(&lead, None, set_grants(&viewer, developer()))
        .await
        .expect("manage succeeds");
    for grants in [Grants::admin(), team_lead()] {
        let mut more = grants;
        more.grant(
            Permission::Global(GlobalPermission::SystemOperate),
            &Scope::All,
        )
        .expect("grant succeeds");
        assert_eq!(
            denied(
                f.auth
                    .manage(&lead, None, set_grants(&dev, more.clone()))
                    .await
            ),
            Denied::Exceeds
        );
        assert_eq!(
            denied(
                f.auth
                    .manage(&lead, None, Manage::CreateInvitation { grants: more })
                    .await
            ),
            Denied::Exceeds
        );
    }
    let directory = f.auth.directory(&dev).await.expect("directory succeeds");
    assert_eq!(directory.users.len(), 1);
    assert_eq!(directory.users[0].user.id, dev.account().id);
    assert_eq!(
        f.auth
            .directory(&lead)
            .await
            .expect("directory succeeds")
            .users
            .len(),
        4
    );
}

#[tokio::test]
async fn invitations_carry_grants_and_enrollment_targets_an_account() {
    let f = Fixture::new().await;
    let admin = f.identity("admin", &Grants::admin()).await;
    let bob = f.identity("bob", &developer()).await;
    let invite = Manage::CreateInvitation {
        grants: developer(),
    };
    let secret = f.link(&admin, invite).await;
    let carol = carol();
    assert!(f.redeem(&carol, &secret).await);
    let directory = f.auth.directory(&admin).await.expect("directory succeeds");
    let account = directory
        .users
        .iter()
        .find(|account| account.user.id == carol.id)
        .expect("the match exists");
    assert_eq!(account.grants, developer());

    let enroll = Manage::CreateEnrollment { user_id: bob.id() };
    let secret = f.link(&admin, enroll).await;
    assert!(matches!(
        f.auth.invitation(&secret).await.expect("invitation succeeds"),
        Some(Invitation::Enrollment(user)) if user.id == bob.account().id
    ));
    assert!(!f.redeem(&carol, &secret).await);
    assert_eq!(
        denied(
            f.auth
                .manage(
                    &bob,
                    None,
                    Manage::CreateEnrollment {
                        user_id: admin.id(),
                    },
                )
                .await
        ),
        Denied::Missing(Permission::Global(GlobalPermission::AccountsManage))
    );
}

/// A caller demoted or revoked after authenticating cannot act on its old
/// grants: every change re-reads the caller inside its transaction.
#[tokio::test]
async fn changes_use_the_callers_current_grants() {
    let f = Fixture::new().await;
    let root = f.identity("root", &Grants::admin()).await;
    f.passkey(&root).await;
    let alice = f.identity("alice", &Grants::admin()).await;
    f.auth
        .manage(&root, None, set_grants(&alice, developer()))
        .await
        .expect("manage succeeds");
    assert_eq!(
        denied(
            f.auth
                .manage(&alice, None, set_grants(&alice, Grants::admin()))
                .await
        ),
        Denied::Exceeds
    );
    assert_eq!(
        denied(
            f.auth
                .manage(
                    &alice,
                    None,
                    Manage::CreateInvitation {
                        grants: developer()
                    }
                )
                .await
        ),
        Denied::Missing(Permission::Global(GlobalPermission::AccountsManage))
    );
    f.auth
        .manage(
            &root,
            None,
            Manage::RevokeAll {
                user_id: alice.id(),
                cascade: true,
            },
        )
        .await
        .expect("manage succeeds");
    assert!(matches!(
        f.auth
            .manage(
                &alice,
                None,
                Manage::UpdateUser {
                    user_id: alice.id(),
                    username: "alice".into(),
                    display_name: "Revoked".into(),
                },
            )
            .await,
        Err(AuthError::Store(StoreError::CredentialRevoked))
    ));
    let passkey = NewPasskey {
        id: "revoked-key",
        name: "Key",
        credential: "{}",
    };
    let owner = PasskeyOwner::Existing(
        alice.caller().expect("caller succeeds"),
        &alice.account().id,
    );
    assert!(matches!(
        f.auth.0.store.add_passkey(None, owner, passkey).await,
        Err(StoreError::CredentialRevoked)
    ));
    let (_, bob) = f
        .signed_in(
            "bob",
            CredentialKind::Browser,
            Some(now_secs() + DAY),
            &developer(),
        )
        .await;
    f.auth.0.store.age_auth_credentials(DAY).await;
    let late = create_token(developer(), "late", None);
    assert!(matches!(
        f.auth.manage(&bob, None, late).await,
        Err(AuthError::Store(StoreError::CredentialRevoked))
    ));
}

/// Links act with their issuer's authority, so they are refused once either
/// side's grants put the target beyond the issuer, and only issuers able to
/// create a link may revoke it.
#[tokio::test]
async fn links_are_revalidated_when_redeemed_and_revoked() {
    let f = Fixture::new().await;
    let admin = f.identity("admin", &Grants::admin()).await;
    f.passkey(&admin).await;
    let lead = f.identity("lead", &team_lead()).await;
    let dev = f.identity("dev", &developer()).await;
    let enroll = f
        .link(&lead, Manage::CreateEnrollment { user_id: dev.id() })
        .await;
    let invitation = Manage::CreateInvitation {
        grants: developer(),
    };
    let invite = f.link(&lead, invitation).await;
    f.auth
        .manage(
            &admin,
            None,
            Manage::CreateEnrollment {
                user_id: admin.id(),
            },
        )
        .await
        .expect("manage succeeds");
    let directory = f.auth.directory(&admin).await.expect("directory succeeds");
    let admin_link = directory
        .invitations
        .iter()
        .find(|link| link.user_id.as_deref() == Some(admin.account().id.as_str()))
        .expect("the match exists")
        .id
        .clone();
    assert_eq!(
        denied(
            f.auth
                .manage(&lead, None, Manage::RevokeInvitation { id: admin_link })
                .await
        ),
        Denied::Exceeds
    );
    f.auth
        .manage(&admin, None, set_grants(&dev, Grants::admin()))
        .await
        .expect("manage succeeds");
    f.auth
        .manage(&admin, None, set_grants(&lead, developer()))
        .await
        .expect("manage succeeds");
    let carol = carol();
    for (user, secret) in [(dev.account(), &enroll), (&carol, &invite)] {
        assert!(!f.redeem(user, secret).await);
    }
}

/// Tokens act with their grants within the owner's current access, carry the
/// `pqd_` prefix, and cannot create credentials of any kind or change their
/// own account.
#[tokio::test]
async fn tokens_act_within_their_grants_and_cannot_create_credentials() {
    let f = Fixture::new().await;
    let root = f.identity("root", &Grants::admin()).await;
    f.passkey(&root).await;
    let alice = f.identity("alice", &developer()).await;
    let token = |grants| create_token(grants, "ci", Some(30));
    let deploy = Preset::Deploy.grants(&Scope::All);
    assert_eq!(
        denied(f.auth.manage(&alice, None, token(Grants::admin())).await),
        Denied::Exceeds
    );
    let (secret, ci) = f.token(&alice, token(deploy.clone())).await;
    assert!(secret.starts_with("pqd_"));
    assert!(ci.scoped);
    assert_eq!(ci.grants, deploy);
    let read_only = Preset::ReadOnly.grants(&Scope::All);
    f.auth
        .manage(&root, None, set_grants(&alice, read_only.clone()))
        .await
        .expect("manage succeeds");
    assert_eq!(
        f.auth
            .authenticate(&secret)
            .await
            .expect("authenticate succeeds")
            .grants,
        read_only.intersection(&deploy)
    );
    for command in [
        token(Grants::default()),
        Manage::CreateEnrollment {
            user_id: alice.id(),
        },
        Manage::CreateInvitation {
            grants: Grants::default(),
        },
    ] {
        assert_eq!(
            denied(f.auth.manage(&ci, None, command).await),
            Denied::Scoped
        );
    }
    let passkey = NewPasskey {
        id: "token-key",
        name: "Key",
        credential: "{}",
    };
    let owner = PasskeyOwner::Existing(ci.caller().expect("caller succeeds"), &alice.account().id);
    assert!(matches!(
        f.auth.0.store.add_passkey(None, owner, passkey).await,
        Err(StoreError::Denied(Denied::Scoped))
    ));
    let start = f
        .auth
        .device_start(None, None)
        .expect("device start succeeds");
    assert!(matches!(
        f.auth.device_approve(&start.user_code, &ci),
        Err(AuthError::Denied(Denied::Scoped))
    ));
    let own = || alice.id();
    for command in [
        Manage::UpdateUser {
            user_id: own(),
            username: "mallory".into(),
            display_name: String::new(),
        },
        Manage::SetGrants {
            user_id: own(),
            grants: Grants::default(),
        },
        Manage::RevokeAll {
            user_id: own(),
            cascade: true,
        },
        Manage::DeleteUser {
            user_id: own(),
            cascade: true,
        },
    ] {
        assert_eq!(
            denied(f.auth.manage(&ci, None, command).await),
            Denied::Scoped
        );
    }
    f.auth.logout(&ci, None).await.expect("logout succeeds");
    assert!(f.auth.authenticate(&secret).await.is_err());
}

/// Without `accounts:manage`, a token sees only itself in the account
/// directory: its account with the token's grants, and neither the account's
/// passkeys nor its other credentials.
#[tokio::test]
async fn tokens_see_only_themselves_in_the_directory() {
    let f = Fixture::new().await;
    let alice = f.identity("alice", &developer()).await;
    f.passkey(&alice).await;
    let deploy = Preset::Deploy.grants(&Scope::All);
    let (_, ci) = f
        .token(&alice, create_token(deploy.clone(), "ci", Some(30)))
        .await;

    let seen = f.auth.directory(&ci).await.expect("directory succeeds");
    assert_eq!(seen.users.len(), 1);
    assert_eq!(seen.users[0].grants, deploy);
    assert_eq!(seen.passkeys, []);
    let credentials: Vec<_> = seen.credentials.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        credentials,
        [ci.credential_id().expect("credential id succeeds")]
    );
    let own = f.auth.directory(&alice).await.expect("directory succeeds");
    assert_eq!(own.users[0].grants, developer());
    assert_eq!((own.passkeys.len(), own.credentials.len()), (1, 2));
}

/// A token keeps no access its owner lost after it authenticated: writes
/// re-read the owner's grants, and a scoped credential without grants can do
/// nothing.
#[tokio::test]
async fn scoped_credentials_follow_their_owner_and_never_widen() {
    let f = Fixture::new().await;
    let root = f.identity("root", &Grants::admin()).await;
    f.passkey(&root).await;
    let alice = f.identity("alice", &Grants::admin()).await;
    let bob = f.identity("bob", &developer()).await;
    let (_, ops) = f
        .token(&alice, create_token(Grants::admin(), "ops", None))
        .await;
    f.auth
        .manage(&root, None, set_grants(&alice, developer()))
        .await
        .expect("manage succeeds");
    assert_eq!(
        denied(
            f.auth
                .manage(
                    &ops,
                    None,
                    Manage::RevokeAll {
                        user_id: bob.id(),
                        cascade: true
                    }
                )
                .await
        ),
        Denied::Missing(Permission::Global(GlobalPermission::AccountsManage))
    );
    let read_only = Preset::ReadOnly.grants(&Scope::All);
    let admin = Grants::admin();
    let (secret, credential) =
        Auth::new_credential(CredentialKind::Cli, "limited", None, Some(&admin))
            .expect("new credential succeeds");
    f.auth
        .0
        .store
        .insert_credential(&root.account().id, &credential)
        .await
        .expect("insert credential succeeds");
    let limited = f
        .auth
        .authenticate(&secret)
        .await
        .expect("authenticate succeeds");
    let grant = set_grants(&bob, read_only);
    assert_eq!(
        denied(f.auth.manage(&limited, None, grant).await),
        Denied::Scoped
    );
    let empty = Grants::default();
    let (secret, credential) =
        Auth::new_credential(CredentialKind::Token, "empty", None, Some(&empty))
            .expect("new credential succeeds");
    f.auth
        .0
        .store
        .insert_credential(&alice.account().id, &credential)
        .await
        .expect("insert credential succeeds");
    let nothing = f
        .auth
        .authenticate(&secret)
        .await
        .expect("authenticate succeeds");
    assert!(nothing.scoped);
    assert!(nothing.grants.is_empty());
}

/// Requests carry their credential's passkey proof, so sensitive actions can
/// refuse stale callers before doing anything; tokens are told they can never
/// qualify, and the host operator is exempt.
#[tokio::test]
async fn identities_refuse_sensitive_actions_without_a_recent_passkey() {
    let f = Fixture::new().await;
    let fresh = f.identity("alice", &Grants::admin()).await;
    fresh
        .require_fresh()
        .expect("a passkey proved the credential's holder just now");
    let (_, stale) = f.stale_session(&fresh).await;
    let (_, token) = f
        .signed_in("ci", CredentialKind::Token, None, &Grants::admin())
        .await;
    for (identity, reason) in [
        (&stale, Reauthenticate::Stale),
        (&token, Reauthenticate::Token),
    ] {
        assert!(matches!(
            identity.require_fresh(),
            Err(AuthError::Denied(Denied::Reauthenticate(refused))) if refused == reason
        ));
    }
    let operator = HostOperator { uid: 0 };
    Identity::operator(operator, None)
        .require_fresh()
        .expect("require fresh succeeds");
}

/// Adding a passkey to one's own account needs a recent passkey sign-in,
/// refused before any passkey is created and again when it is stored.
#[tokio::test]
async fn adding_a_passkey_needs_a_recent_sign_in() {
    let f = Fixture::new().await;
    let alice = f.identity("alice", &Grants::admin()).await;
    let (_, stale) = f.stale_session(&alice).await;
    let start = RegistrationStart {
        invitation: None,
        user_id: Some(alice.account().id.clone()),
        username: String::new(),
        display_name: String::new(),
        passkey_name: "Key".into(),
    };
    let refused = Denied::Reauthenticate(Reauthenticate::Stale);
    assert!(matches!(
        f.auth.registration_start(start, "browser", Some(&stale)).await,
        Err(AuthError::Denied(denied)) if denied == refused
    ));
    let passkey = NewPasskey {
        id: "key",
        name: "Key",
        credential: "{}",
    };
    let owner = PasskeyOwner::Existing(
        stale.caller().expect("the identity is an account's"),
        &alice.account().id,
    );
    assert!(matches!(
        f.auth.0.store.add_passkey(None, owner, passkey).await,
        Err(StoreError::Denied(denied)) if denied == refused
    ));
}

/// Only browser sessions confirm with a passkey: tokens are refused for
/// good, and CLI logins and the host operator are pointed elsewhere.
#[tokio::test]
async fn only_browser_sessions_start_a_passkey_confirmation() {
    let f = Fixture::new().await;
    let (_, token) = f
        .account("ci", CredentialKind::Token, None, &Grants::admin())
        .await;
    let token = f
        .auth
        .authenticate(&token)
        .await
        .expect("the stored credential authenticates");
    assert!(matches!(
        f.auth.reauthenticate_start(&token, "browser").await,
        Err(AuthError::Denied(Denied::Reauthenticate(
            Reauthenticate::Token
        )))
    ));
    let cli = f.identity("alice", &Grants::admin()).await;
    let (_, browser) = f.stale_session(&cli).await;
    let operator = Identity::operator(HostOperator { uid: 0 }, None);
    for identity in [&cli, &operator, &browser] {
        assert!(matches!(
            f.auth.reauthenticate_start(identity, "browser").await,
            Err(AuthError::Invalid(_))
        ));
    }
}

/// A confirmation that fails leaves the session signed in: a ceremony
/// another session started is stale, and a passkey that isn't the account's
/// is refused, neither with the 401 that tells the dashboard it signed out.
#[tokio::test]
async fn failed_passkey_confirmations_keep_the_session() {
    let f = Fixture::new().await;
    let alice = f.identity("alice", &Grants::admin()).await;
    let (_, session) = f.stale_session(&alice).await;
    let (_, other) = f.stale_session(&alice).await;
    let id = session.credential_id().expect("a session has an ID");
    let start = || f.auth.remember_reauthentication("browser", id, Vec::new());
    let assertion = serde_json::json!({
        "id": "AA",
        "rawId": "AA",
        "response": {
            "authenticatorData": "AA",
            "clientDataJSON": "AA",
            "signature": "AA",
            "userHandle": null
        },
        "extensions": {},
        "type": "public-key"
    });
    let finish = |ceremony: piqueld_core::auth::Ceremony| piqueld_core::auth::CeremonyFinish {
        id: ceremony.id,
        credential: assertion.clone(),
    };
    for identity in [&other, &session] {
        let ceremony = start().await.expect("the ceremony is stored");
        let refused = f
            .auth
            .reauthenticate_finish(finish(ceremony), "browser", identity, None)
            .await;
        assert!(matches!(refused, Err(AuthError::Invalid(_))), "{refused:?}");
    }
}

/// Confirming with a passkey verifies it under the writer lock and marks
/// only the confirming credential fresh, never another account's passkey,
/// a token, or a revoked credential.
#[tokio::test]
async fn confirming_with_a_passkey_refreshes_only_the_confirming_session() {
    let f = Fixture::new().await;
    let alice = f.identity("alice", &Grants::admin()).await;
    let key = f.passkey(&alice).await;
    let bob = f.identity("bob", &Grants::admin()).await;
    let bobs = f.passkey(&bob).await;
    let (secret, session) = f.stale_session(&alice).await;
    let (sibling_secret, sibling) = f.stale_session(&alice).await;
    let failed = |_: &str| Err::<String, _>(StoreError::Corrupt);
    let caller = session.caller().expect("the identity is an account's");
    let store = &f.auth.0.store;
    assert!(matches!(
        store
            .confirm_with_passkey(caller, &key, failed, now_secs())
            .await,
        Err(StoreError::Corrupt)
    ));
    assert!(!f.fresh(&secret).await);
    assert!(
        !f.confirm(&session, &bobs)
            .await
            .expect("the confirming credential is live")
    );
    assert!(
        f.confirm(&session, &key)
            .await
            .expect("the confirming credential is live")
    );
    assert!(f.fresh(&secret).await);
    assert!(!f.fresh(&sibling_secret).await);
    let (_, token) = f
        .account("ci", CredentialKind::Token, None, &Grants::admin())
        .await;
    let token = f
        .auth
        .authenticate(&token)
        .await
        .expect("the stored credential authenticates");
    assert!(matches!(
        f.confirm(&token, &key).await,
        Err(StoreError::Denied(Denied::Reauthenticate(
            Reauthenticate::Token
        )))
    ));
    f.auth
        .logout(&sibling, None)
        .await
        .expect("signing out succeeds");
    assert!(matches!(
        f.confirm(&sibling, &key).await,
        Err(StoreError::CredentialRevoked)
    ));
}

/// Creating any credential or link, and handing out `admin`,
/// `accounts:manage` or `system:operate`, need a recent passkey sign-in.
/// Other grant changes don't, and neither does the host operator.
#[tokio::test]
async fn minting_and_privileged_grants_need_a_recent_sign_in() {
    let f = Fixture::new().await;
    let fresh = f.identity("alice", &Grants::admin()).await;
    f.passkey(&fresh).await;
    let (_, stale) = f.stale_session(&fresh).await;
    let bob = f.identity("bob", &developer()).await;
    let bob_id = bob.account().id.clone();
    let grants = |grants: Grants| Manage::SetGrants {
        user_id: bob_id.clone(),
        grants,
    };
    let minting = || {
        [
            Manage::CreateToken {
                grants: Preset::ReadOnly.grants(&Scope::All),
                name: "ci".into(),
                days: Some(1),
                tailnet: None,
            },
            Manage::CreateInvitation {
                grants: Grants::default(),
            },
            Manage::CreateEnrollment {
                user_id: bob_id.clone(),
            },
            grants(Grants::admin()),
            grants(team_lead()),
        ]
    };
    let refused = Denied::Reauthenticate(Reauthenticate::Stale);
    for command in minting() {
        assert_eq!(denied(f.auth.manage(&stale, None, command).await), refused);
    }
    let start = f
        .auth
        .device_start(None, None)
        .expect("a device login starts");
    assert_eq!(
        denied_auth(f.auth.device_approve(&start.user_code, &stale)),
        refused
    );
    let read_only = Preset::ReadOnly.grants(&Scope::All);
    f.auth
        .manage(&stale, None, grants(read_only))
        .await
        .expect("the account change is allowed");
    f.auth
        .manage(&stale, None, grants(developer()))
        .await
        .expect("the account change is allowed");
    for command in minting() {
        f.auth
            .manage(&fresh, None, command)
            .await
            .expect("the account change is allowed");
    }
    f.auth
        .manage(&stale, None, grants(developer()))
        .await
        .expect("the account change is allowed");
    let operator = Identity::operator(HostOperator { uid: 0 }, None);
    f.auth
        .manage(&operator, None, grants(Grants::admin()))
        .await
        .expect("the account change is allowed");
}

/// A CLI login inherits the passkey proof its approval recorded. Approving
/// needs that proof to stay recent a while longer, and an approval whose
/// proof is too old by the time the CLI polls is refused for good, even if
/// the approver has confirmed again since.
#[tokio::test]
async fn device_logins_inherit_their_approvals_passkey_proof() {
    let f = Fixture::new().await;
    let alice = f.identity("alice", &Grants::admin()).await;
    let approver = async |proved: i64| {
        let session = Auth::new_credential(CredentialKind::Browser, "Browser", None, None);
        let (secret, mut session) = session.expect("the system RNG yields a credential secret");
        session.authenticated_at = Some(proved);
        f.auth
            .0
            .store
            .insert_credential(&alice.account().id, &session)
            .await
            .expect("the store keeps the credential");
        f.auth
            .authenticate(&secret)
            .await
            .expect("the stored credential authenticates")
    };
    let lapsing = approver(now_secs() - 100).await;
    let start = f
        .auth
        .device_start(None, None)
        .expect("a device login starts");
    let refused = Denied::Reauthenticate(Reauthenticate::Stale);
    assert_eq!(
        denied_auth(f.auth.device_approve(&start.user_code, &lapsing)),
        refused
    );
    let proved = now_secs() - 60;
    let approver = approver(proved).await;
    f.auth
        .device_approve(&start.user_code, &approver)
        .expect("the approver may approve the request");
    let issued = f
        .auth
        .device_poll(&start.device_code, None)
        .await
        .expect("the poll answers");
    let login = f
        .auth
        .authenticate(&issued.token.expect("the approved login was issued a token"))
        .await
        .expect("the issued login authenticates");
    let Credential::Account { proof, .. } = &login.credential else {
        panic!("a CLI login belongs to an account");
    };
    assert_eq!(proof.authenticated_at, Some(proved));

    let start = f
        .auth
        .device_start(None, None)
        .expect("a device login starts");
    f.auth
        .device_approve(&start.user_code, &alice)
        .expect("the approver may approve the request");
    let key = Auth::hash(&start.device_code);
    let old = now_secs() - Reauthenticate::WINDOW_SECS - 1;
    if let Some(approval) = &mut f
        .auth
        .0
        .devices
        .lock()
        .get_mut(&key)
        .expect("the request was approved")
        .approved_by
    {
        approval.authenticated_at = old;
    }
    assert!(matches!(
        f.auth.device_poll(&start.device_code, None).await,
        Err(AuthError::Store(StoreError::Denied(
            Denied::Reauthenticate(Reauthenticate::Stale)
        )))
    ));
    assert!(matches!(
        f.auth.device_poll(&start.device_code, None).await,
        Err(AuthError::Unauthorized)
    ));
}

/// A CLI login confirms itself through a device request its own account
/// approves in a fresh browser session. That marks the login fresh without
/// issuing another; other accounts can't see or approve the request, and a
/// poll that fails on its way leaves it to the next.
#[tokio::test]
async fn cli_logins_confirm_with_a_passkey_through_the_device_flow() {
    let f = Fixture::new().await;
    let alice = f.identity("alice", &Grants::admin()).await;
    let bob = f.identity("bob", &Grants::admin()).await;
    let (secret, login) = f.stale_login(&alice).await;
    let (_, browser) = f.stale_session(&alice).await;
    let action = SensitiveAction::CreateToken;
    let start = f
        .auth
        .device_confirm(&login, None, action)
        .expect("a CLI login may start a confirmation");
    let code = &start.user_code;
    assert!(matches!(
        f.auth.device_inspect(code, &bob),
        Err(AuthError::Invalid(_))
    ));
    assert!(f.auth.device_approve(code, &bob).is_err());
    let refused = Denied::Reauthenticate(Reauthenticate::Stale);
    assert_eq!(denied_auth(f.auth.device_approve(code, &browser)), refused);
    let request = f
        .auth
        .device_inspect(code, &alice)
        .expect("the request is shown to its own account");
    let confirms = request.confirms.expect("the request confirms a login");
    assert_eq!(confirms.action, action);
    assert_eq!(Some(confirms.login.as_str()), login.credential_id());
    let credentials = async || {
        f.auth
            .directory(&alice)
            .await
            .expect("the account's directory is readable")
            .credentials
            .len()
    };
    let before = credentials().await;
    f.auth
        .device_approve(code, &alice)
        .expect("the approver may approve the request");
    let writer = f.auth.0.store.hold_writer().await;
    let cancelled = tokio::spawn({
        let auth = f.auth.clone();
        let device_code = start.device_code.clone();
        async move { auth.device_poll(&device_code, None).await }
    });
    f.auth.0.store.writer_waiting().await;
    cancelled.abort();
    assert!(
        cancelled
            .await
            .expect_err("the poll waits for the store")
            .is_cancelled()
    );
    drop(writer);
    let key = Auth::hash(&start.device_code);
    f.auth
        .0
        .devices
        .lock()
        .get_mut(&key)
        .expect("the request is still pending")
        .next_poll = 0;
    let done = f
        .auth
        .device_poll(&start.device_code, None)
        .await
        .expect("the poll answers");
    assert_eq!(done.status, "complete");
    assert!(done.token.is_none());
    assert!(f.fresh(&secret).await);
    assert_eq!(credentials().await, before);
}

/// A CLI login revoked before its confirmation's poll stays revoked, and
/// only CLI logins confirm through the device flow.
#[tokio::test]
async fn device_confirmations_revive_nothing_and_only_serve_cli_logins() {
    let f = Fixture::new().await;
    let alice = f.identity("alice", &Grants::admin()).await;
    let action = SensitiveAction::CreateToken;
    let (_, revoked) = f.stale_login(&alice).await;
    let start = f
        .auth
        .device_confirm(&revoked, None, action)
        .expect("a CLI login may start a confirmation");
    f.auth
        .device_approve(&start.user_code, &alice)
        .expect("the approver may approve the request");
    f.auth
        .logout(&revoked, None)
        .await
        .expect("signing out succeeds");
    assert!(matches!(
        f.auth.device_poll(&start.device_code, None).await,
        Err(AuthError::Unauthorized)
    ));
    let (_, token) = f
        .account("ci", CredentialKind::Token, None, &Grants::admin())
        .await;
    let token = f
        .auth
        .authenticate(&token)
        .await
        .expect("the stored credential authenticates");
    assert!(matches!(
        f.auth.device_confirm(&token, None, action),
        Err(AuthError::Denied(Denied::Reauthenticate(
            Reauthenticate::Token
        )))
    ));
    let (_, browser) = f.stale_session(&alice).await;
    let operator = Identity::operator(HostOperator { uid: 0 }, None);
    for identity in [&browser, &operator] {
        assert!(matches!(
            f.auth.device_confirm(identity, None, action),
            Err(AuthError::Invalid(_))
        ));
    }
}

/// `auth.max_token_days` bounds new tokens, and secrets issued before the
/// `pqd_` prefix still authenticate, including ones that happen to start
/// with it.
#[tokio::test]
async fn token_lifetimes_are_bounded_and_legacy_secrets_authenticate() {
    let f = Fixture::new().await;
    let alice = f.identity("alice", &Grants::admin()).await;
    f.auth
        .0
        .store
        .assume_settings(|settings| settings.auth.max_token_days = Some(30));
    let auth = Auth::configured(&f.auth.0.store, ORIGIN, TokenPolicy::default())
        .expect("the auth is configured");
    let token = |days| create_token(developer(), "ci", days);
    for days in [None, Some(31)] {
        assert!(matches!(
            auth.manage(&alice, None, token(days)).await,
            Err(AuthError::Invalid(_))
        ));
    }
    auth.manage(&alice, None, token(Some(30)))
        .await
        .expect("manage succeeds");
    for legacy in ["l".repeat(43), format!("pqd_{}", "A".repeat(39))] {
        let credential = NewCredential {
            id: Auth::id(),
            secret_hash: Auth::hash(&legacy),
            kind: CredentialKind::Token,
            name: "legacy",
            expires_at: None,
            grants: Some(&Grants::admin()),
            authenticated_at: None,
        };
        f.auth
            .0
            .store
            .insert_credential(&alice.account().id, &credential)
            .await
            .expect("insert credential succeeds");
        assert_eq!(
            f.auth
                .authenticate(&legacy)
                .await
                .expect("authenticate succeeds")
                .grants,
            Grants::admin()
        );
    }
}

/// A CLI login can ask for less than the approver's access; the approver must
/// hold what it asks for, and the issued session is limited to it.
#[tokio::test]
async fn device_logins_can_request_limited_sessions() {
    let f = Fixture::new().await;
    let alice = f.identity("alice", &developer()).await;
    let start = f
        .auth
        .device_start(None, Some(Grants::admin()))
        .expect("device start succeeds");
    assert!(matches!(
        f.auth.device_approve(&start.user_code, &alice),
        Err(AuthError::Denied(Denied::Exceeds))
    ));
    let read_only = Preset::ReadOnly.grants(&Scope::All);
    let start = f
        .auth
        .device_start(None, Some(read_only.clone()))
        .expect("device start succeeds");
    let request = f
        .auth
        .device_inspect(&start.user_code, &alice)
        .expect("the request is shown to its own account");
    assert_eq!(request.grants.as_ref(), Some(&read_only));
    f.auth
        .device_approve(&start.user_code, &alice)
        .expect("device approve succeeds");
    let result = f
        .auth
        .device_poll(&start.device_code, None)
        .await
        .expect("device poll succeeds");
    let session = f
        .auth
        .authenticate(result.token.as_ref().expect("the value is present"))
        .await
        .expect("authenticate succeeds");
    assert!(session.scoped);
    assert_eq!(session.grants, read_only);
    let root = f.identity("root", &Grants::admin()).await;
    f.passkey(&root).await;
    let start = f
        .auth
        .device_start(None, Some(read_only.clone()))
        .expect("device start succeeds");
    f.auth
        .device_approve(&start.user_code, &alice)
        .expect("device approve succeeds");
    f.auth
        .manage(
            &root,
            None,
            set_grants(&alice, Preset::Deploy.grants(&Scope::All)),
        )
        .await
        .expect("manage succeeds");
    assert!(matches!(
        f.auth.device_poll(&start.device_code, None).await,
        Err(AuthError::Store(StoreError::Denied(Denied::Exceeds)))
    ));
}

#[tokio::test]
async fn deleting_an_issuer_invalidates_its_invitations_and_credentials() {
    let f = Fixture::new().await;
    let (token, alice) = f
        .signed_in("alice", CredentialKind::Cli, None, &Grants::admin())
        .await;
    let bob = f.identity("bob", &Grants::admin()).await;
    f.passkey(&bob).await;
    let invite = Manage::CreateInvitation {
        grants: developer(),
    };
    let secret = f.link(&alice, invite).await;
    assert!(
        f.auth
            .invitation(&secret)
            .await
            .expect("invitation succeeds")
            .is_some()
    );
    f.auth
        .manage(
            &bob,
            None,
            Manage::DeleteUser {
                user_id: alice.id(),
                cascade: true,
            },
        )
        .await
        .expect("manage succeeds");
    assert!(
        f.auth
            .invitation(&secret)
            .await
            .expect("invitation succeeds")
            .is_none()
    );
    assert!(f.auth.authenticate(&token).await.is_err());
}

#[tokio::test]
async fn changes_keep_an_admin_with_a_passkey() {
    let f = Fixture::new().await;
    let alice = f.identity("alice", &Grants::admin()).await;
    let bob = f.identity("bob", &Grants::admin()).await;
    let key = f.passkey(&alice).await;
    for command in [
        Manage::RemovePasskey {
            id: key.clone(),
            cascade: true,
        },
        Manage::DeleteUser {
            user_id: alice.id(),
            cascade: true,
        },
        set_grants(&alice, developer()),
    ] {
        assert!(matches!(
            f.auth.manage(&bob, None, command).await,
            Err(AuthError::Store(StoreError::Lockout(Lockout::LastAdmin)))
        ));
        let directory = f.auth.directory(&alice).await.expect("directory succeeds");
        assert_eq!(directory.users.len(), 2);
        assert_eq!(directory.passkeys.len(), 1);
        assert_eq!(directory.passkeys[0].id, key);
        assert_eq!(f.reload(&alice).await.grants, Grants::admin());
    }
    f.auth
        .manage(
            &alice,
            None,
            Manage::DeleteUser {
                user_id: bob.id(),
                cascade: true,
            },
        )
        .await
        .expect("manage succeeds");
    assert_eq!(
        f.auth
            .directory(&alice)
            .await
            .expect("directory succeeds")
            .users
            .len(),
        1
    );
}

#[tokio::test]
async fn concurrent_passkey_and_account_deletions_keep_one_admin() {
    for delete_account in [false, true] {
        let f = Fixture::new().await;
        let alice = f.identity("alice", &Grants::admin()).await;
        let bob = f.identity("bob", &Grants::admin()).await;
        let alice_key = f.passkey(&alice).await;
        let bob_key = f.passkey(&bob).await;
        let second = if delete_account {
            Manage::DeleteUser {
                user_id: bob.id(),
                cascade: true,
            }
        } else {
            Manage::RemovePasskey {
                id: bob_key,
                cascade: true,
            }
        };
        let (first, second) = tokio::join!(
            f.auth.manage(
                &alice,
                None,
                Manage::RemovePasskey {
                    id: alice_key,
                    cascade: true
                }
            ),
            f.auth.manage(&alice, None, second),
        );
        let error = match (first, second) {
            (Ok(_), Err(error)) | (Err(error), Ok(_)) => error,
            results => panic!("exactly one deletion must succeed: {results:?}"),
        };
        assert!(matches!(
            error,
            AuthError::Store(StoreError::Lockout(Lockout::LastAdmin))
        ));
        assert_eq!(
            f.auth
                .directory(&alice)
                .await
                .expect("directory succeeds")
                .passkeys
                .len(),
            1
        );
    }
}

/// Revoking a credential or invitation that does not exist is reported,
/// rather than succeeding without revoking anything.
#[tokio::test]
async fn revoking_unknown_ids_is_not_found() {
    let f = Fixture::new().await;
    let root = f.identity("root", &Grants::admin()).await;
    for command in [
        Manage::RevokeCredential {
            id: Auth::id(),
            cascade: true,
        },
        Manage::RevokeInvitation { id: Auth::id() },
    ] {
        let result = f.auth.manage(&root, None, command).await;
        assert!(matches!(
            result,
            Err(AuthError::Store(StoreError::NotFound))
        ));
    }
}

/// Logging out leaves the tokens a login created valid; revoking a login
/// revokes them too unless asked not to.
#[tokio::test]
async fn logout_never_cascades_and_revocation_does_by_default() {
    let f = Fixture::new().await;
    let root = f.identity("root", &Grants::admin()).await;
    let token = async |identity: &Identity| {
        let create = Manage::CreateToken {
            grants: Grants::default(),
            name: "ci".into(),
            days: None,
            tailnet: None,
        };
        let managed = f.auth.manage(identity, None, create).await;
        let managed = managed.expect("an administrator creates a token");
        managed.token.expect("a created token is returned once")
    };
    let revoke = async |identity: &Identity, cascade| {
        let id = identity
            .credential_id()
            .expect("accounts sign in with a credential");
        let id = id.to_owned();
        let revoke = Manage::RevokeCredential { id, cascade };
        let revoked = f.auth.manage(&root, None, revoke).await;
        revoked.expect("root may revoke any credential");
    };
    let alice = f.identity("alice", &Grants::admin()).await;
    let after_logout = token(&alice).await;
    f.auth.logout(&alice, None).await.expect("alice logs out");
    let survived = f.auth.authenticate(&after_logout).await;
    survived.expect("the token outlives the logout of its issuer");

    let bob = f.identity("bob", &Grants::admin()).await;
    let kept = token(&bob).await;
    revoke(&bob, false).await;
    let kept = f.auth.authenticate(&kept).await;
    kept.expect("a revocation without cascade keeps the token");
    let carol = f.identity("carol", &Grants::admin()).await;
    let revoked = token(&carol).await;
    revoke(&carol, true).await;
    assert!(f.auth.authenticate(&revoked).await.is_err());
}

/// Issuing an approved CLI login does not hold up other logins while it
/// waits for storage, and answers one session once: a concurrent poll
/// waits, and a poll cancelled after storing its session leaves it for the
/// next poll, unless it was revoked meanwhile, which refuses the login.
#[tokio::test]
async fn device_polls_issue_once_without_blocking_other_logins() {
    /// Approves a new login by `alice` and polls it until the poll is
    /// cancelled waiting to store its session, which is then stored as if
    /// that poll had committed just before. Returns the device code and
    /// the stored session's credential.
    async fn committed_then_cancelled(
        f: &Fixture,
        alice: &Identity,
    ) -> (String, NewCredential<'static>, String) {
        let start = f
            .auth
            .device_start(None, None)
            .expect("device start succeeds");
        f.auth
            .device_approve(&start.user_code, alice)
            .expect("device approve succeeds");
        let key = Auth::hash(&start.device_code);
        let writer = f.auth.0.store.hold_writer().await;
        let cancelled = tokio::spawn({
            let auth = f.auth.clone();
            let code = start.device_code.clone();
            async move { auth.device_poll(&code, None).await }
        });
        f.auth.0.store.writer_waiting().await;
        f.auth
            .device_start(None, None)
            .expect("device start succeeds");
        f.allow_poll(&start.device_code);
        let concurrent = f
            .auth
            .device_poll(&start.device_code, None)
            .await
            .expect("device poll succeeds");
        assert_eq!(concurrent.status, "authorization_pending");
        cancelled.abort();
        assert!(
            cancelled
                .await
                .expect_err("the task was cancelled")
                .is_cancelled()
        );
        drop(writer);
        let (prepared, approval) = {
            let device = &f.auth.0.devices.lock()[&key];
            (device.prepared.clone(), device.approved_by.clone())
        };
        let prepared = prepared.expect("the first poll prepared the session");
        let credential = prepared.credential(None, &approval.expect("the request was approved"));
        let store = &f.auth.0.store;
        let stored =
            store.issue_for_credential_owner(alice.caller().expect("caller succeeds"), &credential);
        assert!(stored.await.expect("the token issues").is_some());
        f.allow_poll(&start.device_code);
        (start.device_code, credential, prepared.token)
    }

    let f = Fixture::new().await;
    let alice = f.identity("alice", &Grants::admin()).await;
    let cli_logins = async || {
        f.auth
            .directory(&alice)
            .await
            .expect("directory succeeds")
            .credentials
            .len()
            - 1
    };

    let (code, _, token) = committed_then_cancelled(&f, &alice).await;
    let issued = f
        .auth
        .device_poll(&code, None)
        .await
        .expect("device poll succeeds");
    assert_eq!(issued.status, "complete");
    assert_eq!(issued.token.as_ref(), Some(&token));
    f.auth
        .authenticate(&token)
        .await
        .expect("authenticate succeeds");
    assert!(matches!(
        f.auth.device_poll(&code, None).await,
        Err(AuthError::Unauthorized)
    ));
    assert_eq!(cli_logins().await, 1);

    let (code, credential, token) = committed_then_cancelled(&f, &alice).await;
    let revoke = Manage::RevokeCredential {
        id: credential.id,
        cascade: true,
    };
    f.auth
        .manage(&alice, None, revoke)
        .await
        .expect("manage succeeds");
    assert!(matches!(
        f.auth.device_poll(&code, None).await,
        Err(AuthError::Unauthorized)
    ));
    assert!(f.auth.authenticate(&token).await.is_err());
    assert_eq!(cli_logins().await, 1, "the revoked session stays revoked");
}

#[tokio::test]
async fn device_approval_is_explicit_single_use_and_bound_to_a_live_session() {
    let f = Fixture::new().await;
    let (_, identity) = f
        .signed_in(
            "alice",
            CredentialKind::Browser,
            Some(now_secs() + DAY),
            &Grants::admin(),
        )
        .await;
    let start = f
        .auth
        .device_start(None, None)
        .expect("device start succeeds");
    assert_eq!(
        f.auth
            .device_poll(&start.device_code, None)
            .await
            .expect("device poll succeeds")
            .status,
        "authorization_pending"
    );
    assert_eq!(
        f.auth
            .device_poll(&start.device_code, None)
            .await
            .expect("device poll succeeds")
            .status,
        "slow_down"
    );
    f.auth
        .device_approve(&start.user_code, &identity)
        .expect("device approve succeeds");
    assert!(f.auth.device_approve(&start.user_code, &identity).is_err());
    f.allow_poll(&start.device_code);
    let result = f
        .auth
        .device_poll(&start.device_code, None)
        .await
        .expect("device poll succeeds");
    f.auth
        .authenticate(result.token.as_ref().expect("the value is present"))
        .await
        .expect("authenticate succeeds");
    f.auth
        .device_poll(&start.device_code, None)
        .await
        .expect_err("`device_poll` should be rejected");
    let start = f
        .auth
        .device_start(None, None)
        .expect("device start succeeds");
    f.auth
        .device_approve(&start.user_code, &identity)
        .expect("device approve succeeds");
    f.auth
        .logout(&identity, None)
        .await
        .expect("logout succeeds");
    f.auth
        .device_poll(&start.device_code, None)
        .await
        .expect_err("`device_poll` should be rejected");
}

#[tokio::test]
async fn setup_link_is_private_stable_and_never_reopens() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new().await;
    let path = f.dir.path().join("setup-link");
    f.auth
        .prepare_setup(&path)
        .await
        .expect("prepare setup succeeds");
    let link = std::fs::read_to_string(&path).expect("read to string succeeds");
    assert_eq!(
        std::fs::metadata(&path)
            .expect("the file has metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    f.auth
        .prepare_setup(&path)
        .await
        .expect("prepare setup succeeds");
    assert_eq!(
        link,
        std::fs::read_to_string(&path).expect("read to string succeeds")
    );
    assert_eq!(
        f.auth.setup_link().await.expect("setup link succeeds").url,
        link.trim()
    );
    let (_, secret) = link.split_once("#invite=").expect("the text splits");
    std::fs::write(
        &path,
        format!("https://old.example/dashboard/auth#invite={secret}"),
    )
    .expect("the file is written");
    f.auth
        .prepare_setup(&path)
        .await
        .expect("prepare setup succeeds");
    assert_eq!(
        link,
        std::fs::read_to_string(&path).expect("read to string succeeds")
    );
    f.account("alice", CredentialKind::Token, None, &Grants::admin())
        .await;
    assert!(matches!(
        f.auth.setup_link().await,
        Err(AuthError::SetupCompleted)
    ));
    f.auth
        .prepare_setup(&path)
        .await
        .expect("prepare setup succeeds");
    assert!(!path.exists());
    f.auth.0.store.clear_auth_users().await;
    f.auth
        .prepare_setup(&path)
        .await
        .expect("prepare setup succeeds");
    assert!(!path.exists());
}

/// Registration refuses up front what finishing it would: a scoped
/// credential adding a passkey, and a taken username, before any passkey
/// is created.
#[tokio::test]
async fn registration_refuses_scoped_callers_and_taken_usernames_at_start() {
    let f = Fixture::new().await;
    let alice = f.identity("alice", &Grants::admin()).await;
    let read_only = Preset::ReadOnly.grants(&Scope::All);
    let (_, ci) = f
        .token(&alice, create_token(read_only, "ci", Some(30)))
        .await;
    let start = |invitation: Option<&str>, user_id: Option<&str>| RegistrationStart {
        invitation: invitation.map(str::to_owned),
        user_id: user_id.map(str::to_owned),
        username: "ALICE".into(),
        display_name: String::new(),
        passkey_name: "Key".into(),
    };
    let own = start(None, Some(&alice.account().id));
    assert!(matches!(
        f.auth.registration_start(own, "browser", Some(&ci)).await,
        Err(AuthError::Denied(Denied::Scoped))
    ));
    let invite = Manage::CreateInvitation {
        grants: Grants::default(),
    };
    let invitation = f.link(&alice, invite).await;
    assert!(matches!(
        f.auth
            .registration_start(start(Some(&invitation), None), "browser", None)
            .await,
        Err(AuthError::Store(StoreError::AlreadyExists))
    ));
}

#[tokio::test]
async fn challenges_require_the_original_browser_and_expire() {
    let f = Fixture::new().await;
    let ceremony = f
        .auth
        .login_start("browser-one")
        .await
        .expect("login start succeeds");
    let input = piqueld_core::auth::CeremonyFinish {
        id: ceremony.id.clone(),
        credential: serde_json::json!({}),
    };
    assert!(matches!(
        f.auth
            .login_finish(input.clone(), "browser-two", None)
            .await,
        Err(AuthError::Invalid(_))
    ));
    assert!(f.auth.0.ceremonies.lock().await.contains_key(&ceremony.id));
    f.auth
        .login_finish(input.clone(), "browser-one", None)
        .await
        .expect_err("`` should be rejected");
    assert!(!f.auth.0.ceremonies.lock().await.contains_key(&ceremony.id));
    assert!(matches!(
        f.auth.login_finish(input, "browser-one", None).await,
        Err(AuthError::Invalid(_))
    ));
    let ceremony = f
        .auth
        .login_start("browser")
        .await
        .expect("login start succeeds");
    f.auth
        .0
        .ceremonies
        .lock()
        .await
        .get_mut(&ceremony.id)
        .expect("the entry exists")
        .expires = 0;
    assert!(matches!(
        f.auth
            .login_finish(
                piqueld_core::auth::CeremonyFinish {
                    id: ceremony.id,
                    credential: serde_json::json!({})
                },
                "browser",
                None
            )
            .await,
        Err(AuthError::Invalid(_))
    ));
}

#[tokio::test]
async fn middleware_authenticates_api_and_enforces_cookie_csrf() {
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        routing::get,
    };
    use tower::ServiceExt;
    let f = Fixture::new().await;
    let (_, token) = f
        .account("alice", CredentialKind::Token, None, &Grants::admin())
        .await;
    let router = f.auth.clone().guard(
        Router::new()
            .route("/api/v1/private", get(signed_in).post(signed_in))
            .route("/health", get(|| async { "ok" })),
    );
    for (path, method, credential, origin, expected) in [
        ("/health", "GET", None, None, StatusCode::OK),
        (
            "/api/v1/private",
            "GET",
            None,
            None,
            StatusCode::UNAUTHORIZED,
        ),
        (
            "/api/v1/private",
            "GET",
            Some(("authorization", format!("Bearer {token}"))),
            None,
            StatusCode::OK,
        ),
        (
            "/api/v1/private",
            "POST",
            Some(("cookie", format!("piqueld_session_7845={token}"))),
            None,
            StatusCode::FORBIDDEN,
        ),
        (
            "/api/v1/private",
            "POST",
            Some(("cookie", format!("piqueld_session_7845={token}"))),
            Some("https://evil.example"),
            StatusCode::FORBIDDEN,
        ),
        (
            "/api/v1/private",
            "POST",
            Some(("cookie", format!("piqueld_session_7845={token}"))),
            Some(ORIGIN),
            StatusCode::OK,
        ),
        (
            "/api/v1/private",
            "POST",
            Some(("authorization", format!("Bearer {token}"))),
            None,
            StatusCode::OK,
        ),
    ] {
        let mut request = Request::builder().method(method).uri(path);
        if let Some((name, value)) = credential {
            request = request.header(name, value);
        }
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        let response = router
            .clone()
            .oneshot(request.body(Body::empty()).expect("the request builds"))
            .await
            .expect("the request is served");
        assert_eq!(response.status(), expected, "{method} {path}");
    }
    for (origin, expected) in [
        ("https://evil.example", StatusCode::FORBIDDEN),
        (ORIGIN, StatusCode::OK),
    ] {
        let request = Request::get("/api/v1/private")
            .header("cookie", format!("piqueld_session_7845={token}"))
            .header("origin", origin)
            .header("upgrade", "websocket")
            .body(Body::empty())
            .expect("the request builds");
        let response = router
            .clone()
            .oneshot(request)
            .await
            .expect("the request is served");
        assert_eq!(response.status(), expected, "{origin}");
    }
}

/// Stands in for route authorization (`access::enforce`): signed-in callers only.
async fn signed_in(identity: Option<axum::Extension<Identity>>) -> http::StatusCode {
    if identity.is_some() {
        http::StatusCode::OK
    } else {
        http::StatusCode::UNAUTHORIZED
    }
}

#[tokio::test]
async fn login_start_limits_share_listeners_ignore_forwarded_ips_and_leave_sessions_usable() {
    use axum::{
        Router,
        body::Body,
        extract::ConnectInfo,
        http::{Request, StatusCode},
        routing::{get, post},
    };
    use tower::ServiceExt;
    let f = Fixture::new().await;
    let (_, token) = f
        .account("alice", CredentialKind::Token, None, &Grants::admin())
        .await;
    let router = f.auth.clone().guard(
        Router::new()
            .route("/api/v1/auth/login/start", post(|| async { "ok" }))
            .route("/api/v1/auth/device/start", post(|| async { "ok" }))
            .route("/api/v1/private", get(signed_in)),
    );
    for attempt in 0..31 {
        let request = Request::builder()
            .method("POST")
            .uri(if attempt % 2 == 0 {
                "/api/v1/auth/login/start"
            } else {
                "/api/v1/auth/device/start"
            })
            .header("origin", f.auth.origin())
            .header("x-forwarded-for", format!("192.0.2.{attempt}"))
            .extension(ConnectInfo(
                "192.0.2.1:1234"
                    .parse::<std::net::SocketAddr>()
                    .expect("the socket addr parses"),
            ))
            .body(Body::empty())
            .expect("the request builds");
        let response = router
            .clone()
            .oneshot(request)
            .await
            .expect("the request is served");
        assert_eq!(
            response.status(),
            if attempt == 30 {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::OK
            }
        );
        if attempt == 30 {
            assert_eq!(response.headers()["retry-after"], "60");
        }
    }
    let other = f
        .auth
        .guard(Router::new().route("/api/v1/auth/device/start", post(|| async { "ok" })));
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/auth/device/start")
        .extension(ConnectInfo(
            "192.0.2.1:5678"
                .parse::<std::net::SocketAddr>()
                .expect("the socket addr parses"),
        ))
        .body(Body::empty())
        .expect("the request builds");
    assert_eq!(
        other
            .oneshot(request)
            .await
            .expect("the request is served")
            .status(),
        StatusCode::TOO_MANY_REQUESTS
    );
    let request = Request::builder()
        .uri("/api/v1/private")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .expect("the request builds");
    assert_eq!(
        router
            .oneshot(request)
            .await
            .expect("the request is served")
            .status(),
        StatusCode::OK
    );
}

/// Requests that find a credential's last-used time stale together leave
/// its refresh to the first, rather than each queueing for the writer.
#[tokio::test]
async fn concurrent_requests_refresh_a_credential_once() {
    use std::time::Duration;
    let f = Fixture::new().await;
    let (_, token) = f
        .account("alice", CredentialKind::Token, None, &Grants::admin())
        .await;
    f.auth.0.store.age_auth_credentials(120).await;
    let writer = f.auth.0.store.hold_writer().await;
    let first = tokio::spawn({
        let (auth, token) = (f.auth.clone(), token.clone());
        async move { auth.authenticate(&token).await.map(|_| ()) }
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while f.auth.0.touched.lock().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the first request claims the refresh");
    tokio::time::timeout(Duration::from_secs(1), f.auth.authenticate(&token))
        .await
        .expect("the second request does not wait for the writer")
        .expect("the authentication succeeds");
    drop(writer);
    first
        .await
        .expect("the first item exists")
        .expect("the value is present");
}

/// Cross-site starts are refused before the throttle counts them, so they
/// cannot spend a peer's sign-in allowance. Only a bearer token that
/// authenticates lets a ceremony go without the configured origin.
#[tokio::test]
async fn cross_site_starts_spend_no_sign_in_allowance() {
    use axum::{
        Router,
        body::Body,
        extract::ConnectInfo,
        http::{Request, StatusCode},
        routing::post,
    };
    use tower::ServiceExt;
    let f = Fixture::new().await;
    let (_, token) = f
        .account("alice", CredentialKind::Token, None, &Grants::admin())
        .await;
    let router = f
        .auth
        .clone()
        .guard(Router::new().route("/api/v1/auth/login/start", post(|| async { "ok" })));
    let start = |header: (&str, String)| {
        let request = Request::post("/api/v1/auth/login/start")
            .header(header.0, header.1)
            .extension(ConnectInfo(
                "192.0.2.1:1234"
                    .parse::<std::net::SocketAddr>()
                    .expect("the socket addr parses"),
            ))
            .body(Body::empty())
            .expect("the request builds");
        let router = router.clone();
        async move {
            router
                .oneshot(request)
                .await
                .expect("the request is served")
                .status()
        }
    };
    for _ in 0..31 {
        let foreign = ("origin", "https://attacker.example".to_owned());
        assert_eq!(start(foreign).await, StatusCode::FORBIDDEN);
        let unusable = ("authorization", format!("Bearer {}", "x".repeat(43)));
        assert_eq!(start(unusable).await, StatusCode::FORBIDDEN);
    }
    let bearer = ("authorization", format!("Bearer {token}"));
    assert_eq!(start(bearer).await, StatusCode::OK);
    let same_origin = ("origin", f.auth.origin().to_owned());
    assert_eq!(start(same_origin).await, StatusCode::OK);
}

#[tokio::test]
async fn https_cookies_use_host_prefix_and_ignore_unprefixed_names() {
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        routing::get,
    };
    use tower::ServiceExt;
    let f = Fixture::new().await;
    let (_, token) = f
        .account(
            "alice",
            CredentialKind::Browser,
            Some(now_secs() + DAY),
            &Grants::admin(),
        )
        .await;
    let auth = Auth::new(&f.auth.0.store, "https://piqueld.example").expect("the value is valid");
    assert_eq!(
        auth.cookie(AuthCookie::Session, "secret", 60),
        "__Host-piqueld_session=secret; Path=/; HttpOnly; SameSite=Strict; Max-Age=60; Secure"
    );
    assert_eq!(
        f.auth.cookie(AuthCookie::Session, "secret", 60),
        "piqueld_session_7845=secret; Path=/; HttpOnly; SameSite=Strict; Max-Age=60"
    );
    assert_eq!(
        Auth::new(&f.auth.0.store, "https://piqueld.example:8443")
            .expect("the value is valid")
            .cookie_name(AuthCookie::Session),
        "__Host-piqueld_session_8443"
    );
    let router = auth.guard(Router::new().route("/api/v1/private", get(signed_in)));
    for (cookie, expected) in [
        (format!("piqueld_session={token}"), StatusCode::UNAUTHORIZED),
        (format!("__Host-piqueld_session={token}"), StatusCode::OK),
    ] {
        let request = Request::builder()
            .uri("/api/v1/private")
            .header("cookie", cookie)
            .body(Body::empty())
            .expect("the request builds");
        assert_eq!(
            router
                .clone()
                .oneshot(request)
                .await
                .expect("the request is served")
                .status(),
            expected
        );
    }
}

#[tokio::test]
async fn device_inspection_reports_the_requester_until_approval() {
    let f = Fixture::new().await;
    let (_, identity) = f
        .signed_in(
            "alice",
            CredentialKind::Browser,
            Some(now_secs() + DAY),
            &Grants::admin(),
        )
        .await;
    let peer = "192.0.2.7".parse().expect("the value parses");
    let start = f
        .auth
        .device_start(Some(peer), None)
        .expect("device start succeeds");
    assert_eq!(start.requester.as_deref(), Some("192.0.2.7"));
    let request = f
        .auth
        .device_inspect(&format!(" {} ", start.user_code.to_lowercase()), &identity)
        .expect("device inspect succeeds");
    assert_eq!(request.user_code, start.user_code);
    assert_eq!(request.requester.as_deref(), Some("192.0.2.7"));
    assert!(request.age <= 1 && (599..=600).contains(&request.expires_in));
    assert_eq!(
        f.auth
            .device_poll(&start.device_code, None)
            .await
            .expect("device poll succeeds")
            .status,
        "authorization_pending"
    );
    f.auth
        .device_inspect("AAAA-AAAA", &identity)
        .expect_err("`device_inspect` should be rejected");
    f.auth
        .device_approve(&start.user_code, &identity)
        .expect("device approve succeeds");
    f.auth
        .device_inspect(&start.user_code, &identity)
        .expect_err("`device_inspect` should be rejected");
    let local = f
        .auth
        .device_start(None, None)
        .expect("device start succeeds");
    assert!(local.requester.is_none());
    assert!(
        f.auth
            .device_inspect(&local.user_code, &identity)
            .expect("device inspect succeeds")
            .requester
            .is_none()
    );
}

/// Security events in daemon history, oldest first, as `(kind, message)`.
async fn security_events(auth: &Auth) -> Vec<(String, String)> {
    let filter = piqueld_core::observability::EventFilter {
        scope: Some(piqueld_core::observability::EventScope::Daemon),
        ..Default::default()
    };
    let visible = piqueld_store::Visibility::ALL;
    let events = auth.0.store.filtered_events(&filter, &visible, None, 100);
    events
        .await
        .expect("the events load")
        .items
        .into_iter()
        .filter(|event| piqueld_store::SecurityEvent::parse(&event.kind).is_some())
        .map(|event| (event.kind, event.message.unwrap_or_default()))
        .collect()
}

/// An admin recovery link needs a claimed installation, replaces any earlier
/// one, creates exactly one administrator, and raises security events.
#[tokio::test]
async fn recovery_links_create_one_administrator_and_raise_security_events() {
    let f = Fixture::new().await;
    let root = HostOperator { uid: 0 };
    assert!(matches!(
        f.auth.recover_admin(root, None).await,
        Err(AuthError::SetupPending)
    ));
    let admin = f.identity("admin", &Grants::admin()).await;
    let secret = |link: &OperatorLink| {
        link.url
            .split_once("#invite=")
            .expect("the text splits")
            .1
            .to_owned()
    };
    let replaced = secret(
        &f.auth
            .recover_admin(root, None)
            .await
            .expect("recover admin succeeds"),
    );
    let current = secret(
        &f.auth
            .recover_admin(root, None)
            .await
            .expect("recover admin succeeds"),
    );
    assert!(
        f.auth
            .invitation(&replaced)
            .await
            .expect("invitation succeeds")
            .is_none()
    );
    assert!(matches!(
        f.auth
            .invitation(&current)
            .await
            .expect("invitation succeeds"),
        Some(Invitation::Account)
    ));
    let carol = carol();
    for redeemed in [true, false] {
        assert_eq!(f.redeem(&carol, &current).await, redeemed);
    }
    let directory = f.auth.directory(&admin).await.expect("directory succeeds");
    let recovered = directory.users.iter().find(|a| a.user.id == carol.id);
    assert_eq!(
        recovered.expect("recovered succeeds").grants,
        Grants::admin()
    );
    let issued = "An admin recovery link was issued over the Unix socket to uid 0";
    assert_eq!(
        security_events(&f.auth).await,
        [
            ("admin_recovery_issued".into(), issued.into()),
            ("admin_recovery_issued".into(), issued.into()),
            (
                "admin_granted".into(),
                "carol became an administrator by an admin recovery link".into()
            ),
        ]
    );
}

/// A host operator sign-in link works once and only until it expires, even
/// before setup, and opens a browser session as the operator, not an
/// account. Issuing one raises a security event attributed to the operator.
/// The session may change accounts but not issue links, and ends on logout.
#[tokio::test]
async fn operator_sign_in_links_work_once_and_act_as_the_operator() {
    let f = Fixture::new().await;
    let operator = HostOperator { uid: 1000 };
    let secret = |link: &OperatorLink| {
        link.url
            .split_once("#operator=")
            .expect("the text splits")
            .1
            .to_owned()
    };
    let expired = secret(
        &f.auth
            .sign_in_link(operator, None)
            .await
            .expect("sign in link succeeds"),
    );
    f.auth.0.store.expire_operator_sessions().await;
    let unusable = f.auth.operator_sign_in(&expired, None).await;
    assert!(matches!(unusable, Err(AuthError::Unauthorized)));
    let link = secret(
        &f.auth
            .sign_in_link(operator, None)
            .await
            .expect("sign in link succeeds"),
    );
    let (signed_in, token) = f
        .auth
        .operator_sign_in(&link, None)
        .await
        .expect("operator sign in succeeds");
    assert_eq!(signed_in, operator);
    let again = f.auth.operator_sign_in(&link, None).await;
    assert!(matches!(again, Err(AuthError::Unauthorized)));
    let writer = f.auth.0.store.hold_writer().await;
    let guess = f.auth.operator_sign_in(&link, None);
    let guess = tokio::time::timeout(Duration::from_secs(1), guess).await;
    assert!(matches!(guess, Ok(Err(AuthError::Unauthorized))));
    drop(writer);
    let identity = f
        .auth
        .authenticate(&token)
        .await
        .expect("authenticate succeeds");
    assert!(identity.user().is_none());
    assert_eq!(identity.grants, Grants::admin());

    let admin = f.identity("admin", &Grants::admin()).await;
    f.passkey(&admin).await;
    let bob = f.identity("bob", &developer()).await;
    let demote = set_grants(&bob, Grants::default());
    f.auth
        .manage(&identity, None, demote)
        .await
        .expect("manage succeeds");
    assert!(f.reload(&bob).await.grants.is_empty());
    let invite = Manage::CreateInvitation {
        grants: Grants::default(),
    };
    let refused = f.auth.manage(&identity, None, invite).await;
    assert!(matches!(refused, Err(AuthError::Invalid(_))));
    assert_eq!(
        f.auth
            .directory(&identity)
            .await
            .expect("directory succeeds")
            .operator_sessions
            .len(),
        1
    );
    f.auth
        .logout(&identity, None)
        .await
        .expect("logout succeeds");
    assert!(f.auth.authenticate(&token).await.is_err());

    let filter = piqueld_core::observability::EventFilter::default();
    let visible = piqueld_store::Visibility::ALL;
    let events = f.auth.0.store.filtered_events(&filter, &visible, None, 100);
    let issued = events
        .await
        .expect("the events load")
        .items
        .into_iter()
        .filter(|event| event.kind == "operator_sign_in_issued")
        .map(|event| (event.message.expect("the message is set"), event.actor))
        .collect::<Vec<_>>();
    let message = "A host operator sign-in link was issued over the Unix socket to uid 1000";
    let actor = piqueld_core::EventActor::Operator {
        operator,
        session_id: None,
    };
    assert_eq!(issued, vec![(message.to_owned(), Some(actor)); 2]);
}

/// Granting `admin` everywhere and creating a never-expiring or `admin`
/// token raise security events; ordinary changes do not.
#[tokio::test]
async fn privileged_grants_and_tokens_raise_security_events() {
    let f = Fixture::new().await;
    let admin = f.identity("admin", &Grants::admin()).await;
    f.passkey(&admin).await;
    let bob = f.identity("bob", &developer()).await;
    for grants in [developer(), Grants::admin()] {
        let change = set_grants(&bob, grants);
        f.auth
            .manage(&admin, None, change)
            .await
            .expect("manage succeeds");
    }
    for (name, days) in [("ci", Some(30)), ("forever", None)] {
        let token = create_token(developer(), name, days);
        f.auth
            .manage(&admin, None, token)
            .await
            .expect("manage succeeds");
    }
    assert_eq!(
        security_events(&f.auth).await,
        [
            (
                "admin_granted".into(),
                "bob became an administrator by an account change".into()
            ),
            (
                "privileged_token_created".into(),
                "admin created API token \"forever\" with no expiry".into()
            ),
        ]
    );
}

/// Without the daemon's tailnet node or a dashboard hostname, a token bound to
/// a tailnet identity could never be used, so creating one is refused.
#[tokio::test]
async fn tailnet_bindings_need_the_tailnet_node_or_dashboard_hostname() {
    use piqueld_config::DaemonConfig;
    assert!(!TokenPolicy::new(&DaemonConfig::default()).tailnet);
    let dashboard = DaemonConfig::from_toml(
        "[ingress]\nenabled = true\n\
         [ingress.private]\nenabled = true\ndashboard_hostname = 'dashboard.example.com'",
    )
    .expect("from toml succeeds");
    assert!(TokenPolicy::new(&dashboard).tailnet);
    let f = Fixture::new().await;
    let admin = f.identity("admin", &Grants::admin()).await;
    let token = Manage::CreateToken {
        grants: developer(),
        name: "ci".into(),
        days: Some(1),
        tailnet: Some(
            piqueld_core::tailnet::TailnetBinding::parse("tag:ci")
                .expect("the tailnet binding parses"),
        ),
    };
    assert!(matches!(
        f.auth.manage(&admin, None, token).await,
        Err(AuthError::Invalid(message)) if message.contains("tailscale.enabled")
    ));
}

/// A setup link path the daemon can't read as a file is reported, not
/// replaced, so the link the operator holds keeps working.
#[tokio::test]
async fn unreadable_setup_link_is_an_error_and_left_alone() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new().await;
    let path = f.dir.path().join("setup-link");
    f.auth
        .prepare_setup(&path)
        .await
        .expect("prepare setup succeeds");
    let link = std::fs::read_to_string(&path).expect("read to string succeeds");
    let (_, secret) = link.trim().split_once("#invite=").expect("the text splits");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
        .expect("set permissions succeeds");
    let result = f.auth.prepare_setup(&path).await;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .expect("set permissions succeeds");
    assert!(result.is_err());
    assert_eq!(
        link,
        std::fs::read_to_string(&path).expect("read to string succeeds")
    );
    std::fs::remove_file(&path).expect("remove file succeeds");
    std::fs::create_dir(&path).expect("create dir succeeds");
    assert!(f.auth.prepare_setup(&path).await.is_err());
    assert!(
        f.auth
            .invitation(secret)
            .await
            .expect("invitation succeeds")
            .is_some()
    );
}

/// A setup link that can't be written, here a stale one in a read-only
/// directory, leaves the stored secret alone.
#[tokio::test]
async fn unwritable_setup_link_keeps_the_stored_secret() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new().await;
    let directory = f.dir.path().join("setup");
    std::fs::create_dir(&directory).expect("create dir succeeds");
    let path = directory.join("setup-link");
    f.auth
        .prepare_setup(&path)
        .await
        .expect("prepare setup succeeds");
    let link = std::fs::read_to_string(&path).expect("read to string succeeds");
    let (_, secret) = link.trim().split_once("#invite=").expect("the text splits");
    std::fs::write(&path, "stale").expect("the file is written");
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o500))
        .expect("set permissions succeeds");
    let result = f.auth.prepare_setup(&path).await;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
        .expect("set permissions succeeds");
    assert!(result.is_err());
    assert!(
        f.auth
            .invitation(secret)
            .await
            .expect("invitation succeeds")
            .is_some()
    );
}
