//! Sign-in routing, grant selection, and form parsing behind the dashboard's
//! authentication and account pages, independent of the browser.
use piqueld_client::{
    ApplicationId,
    access::{Grants, Permission, Scope},
    auth::{DeviceRequest, Manage, Session},
    tailnet::TailnetBinding,
};
use std::{collections::BTreeSet, num::NonZeroU32};

/// A secret link being redeemed on the sign-in page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Invite {
    /// `#invite=`: creates an account, or the first one during setup.
    Account(String),
    /// `#enroll=`: adds a passkey to an existing account.
    Enrollment(String),
    /// `#operator=`: signs the browser in as the host operator.
    Operator(String),
}

impl Invite {
    /// Reads the secret of an invitation, enrollment, or host operator sign-in
    /// link from its URL fragment (`#invite=…`).
    #[must_use]
    pub fn parse(fragment: &str) -> Option<Self> {
        if let Some(secret) = fragment.strip_prefix("#invite=") {
            Some(Self::Account(secret.to_owned()))
        } else if let Some(secret) = fragment.strip_prefix("#operator=") {
            Some(Self::Operator(secret.to_owned()))
        } else {
            Some(Self::Enrollment(
                fragment.strip_prefix("#enroll=")?.to_owned(),
            ))
        }
    }
}

/// What the `/dashboard/auth` page shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignInPage {
    /// Redeem a secret link, whoever is signed in.
    Redeem(Invite),
    /// First-run instructions, before any account exists.
    Setup,
    /// Passkey sign-in.
    SignIn,
    /// The CLI approval page while the host operator is signed in: CLI logins
    /// sign the CLI in as an account, so the operator cannot approve them.
    OperatorCannotApprove,
    /// CLI approval by `who`, reviewing `code` straight away unless empty.
    DeviceApproval {
        /// Name of the signed-in account.
        who: String,
        /// Code from the CLI's link, or empty to enter one.
        code: String,
    },
    /// Signed in as `who`, with a link to the dashboard.
    SignedIn {
        /// Name of the signed-in principal.
        who: String,
    },
}

impl SignInPage {
    /// Chooses the page from the link being redeemed, whether the first
    /// account exists, the current session, and the URL fragment, which marks
    /// the CLI approval page with `#device` or `#device=CODE`. The approval
    /// code stays in the fragment, so it survives the reload after sign-in.
    #[must_use]
    pub fn new(
        invitation: Option<Invite>,
        initialized: bool,
        session: Option<&Session>,
        fragment: &str,
    ) -> Self {
        let device = fragment
            .strip_prefix("#device")
            .and_then(|rest| match rest {
                "" => Some(String::new()),
                code => code.strip_prefix('=').map(str::to_owned),
            });
        match (invitation, session) {
            (Some(invite), _) => Self::Redeem(invite),
            (None, _) if !initialized => Self::Setup,
            (None, None) => Self::SignIn,
            (None, Some(session)) => {
                let who = session.principal.name().to_owned();
                match device {
                    Some(_) if session.principal.user().is_none() => Self::OperatorCannotApprove,
                    Some(code) => Self::DeviceApproval { who, code },
                    None => Self::SignedIn { who },
                }
            }
        }
    }
}

/// Labels for a pending CLI login on its approval page.
pub trait DeviceRequestLabels {
    /// Where the request originated, e.g. `network address 192.0.2.1`.
    fn requester_label(&self) -> String;
    /// Coarse age, e.g. `less than a minute ago` or `2 minutes ago`.
    fn age_label(&self) -> String;
}

impl DeviceRequestLabels for DeviceRequest {
    fn requester_label(&self) -> String {
        self.requester.as_deref().map_or_else(
            || "the daemon's local Unix socket".into(),
            |address| format!("network address {address}"),
        )
    }

    fn age_label(&self) -> String {
        match self.age / 60 {
            0 => "less than a minute ago".into(),
            1 => "1 minute ago".into(),
            minutes => format!("{minutes} minutes ago"),
        }
    }
}

/// The grant editor's selection: permissions, and one scope (every
/// application, or chosen ones) shared by all application permissions.
/// Installation-wide permissions always apply everywhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantSelection {
    /// Selected permissions.
    pub permissions: BTreeSet<Permission>,
    /// Whether application permissions apply to every application.
    pub everywhere: bool,
    /// Applications they apply to otherwise.
    pub applications: BTreeSet<ApplicationId>,
    /// Whether the grants it was read from limit application permissions to
    /// different applications, which one scope cannot show: changing the
    /// selection replaces them with it.
    pub mixed: bool,
}

impl From<&Grants> for GrantSelection {
    fn from(grants: &Grants) -> Self {
        let list = grants.to_list();
        let scopes: BTreeSet<_> = list
            .iter()
            .filter(|grant| grant.permission.scopable())
            .map(|grant| grant.applications.clone())
            .collect();
        Self {
            permissions: list.iter().map(|grant| grant.permission).collect(),
            everywhere: scopes.iter().all(Option::is_none),
            applications: scopes.iter().flatten().flatten().cloned().collect(),
            mixed: scopes.len() > 1,
        }
    }
}

impl GrantSelection {
    /// The selected grants: application permissions on the selected scope,
    /// installation-wide ones everywhere.
    #[must_use]
    pub fn grants(&self) -> Grants {
        let scope = if self.everywhere {
            Scope::All
        } else {
            Scope::Only(self.applications.clone())
        };
        let mut grants = Grants::default();
        for permission in &self.permissions {
            grants.grant_within(*permission, &scope);
        }
        grants
    }
}

/// The new API token form, as typed.
#[derive(Clone, Debug, Default)]
pub struct TokenForm {
    /// Token name.
    pub name: String,
    /// Lifetime in days; blank never expires.
    pub days: String,
    /// Tailnet login or tag the token is bound to; blank for none.
    pub tailnet: String,
}

impl TokenForm {
    /// The command creating this token with `grants`.
    ///
    /// # Errors
    ///
    /// Describes the first invalid field: the lifetime, then the tailnet
    /// binding.
    #[expect(
        clippy::map_err_ignore,
        reason = "the form states what to enter, which a parse error cannot improve on"
    )]
    pub fn command(self, grants: Grants) -> Result<Manage, &'static str> {
        let days = if self.days.is_empty() {
            None
        } else {
            Some(
                self.days
                    .parse::<NonZeroU32>()
                    .map_err(|_| "Enter a positive number of days, or leave blank")?
                    .get(),
            )
        };
        let tailnet = self.tailnet.trim();
        let tailnet = if tailnet.is_empty() {
            None
        } else {
            Some(TailnetBinding::parse(tailnet)?)
        };
        Ok(Manage::CreateToken {
            grants,
            name: self.name,
            days,
            tailnet,
        })
    }
}

#[cfg(test)]
mod tests;
