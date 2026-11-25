use super::*;
use piqueld_client::{
    access::{AppPermission, GlobalPermission},
    auth::{HostOperator, Principal, User},
};

fn app(id: &str) -> ApplicationId {
    ApplicationId::parse(id).unwrap()
}

fn session(principal: Principal) -> Session {
    Session {
        principal,
        grants: Grants::default(),
        scoped: false,
    }
}

fn alice() -> Session {
    session(Principal::User(User {
        id: "alice-id".into(),
        username: "alice".into(),
        display_name: String::new(),
    }))
}

fn grants(list: &[(Permission, Scope)]) -> Grants {
    let mut grants = Grants::default();
    for (permission, scope) in list {
        grants.grant(*permission, scope).unwrap();
    }
    grants
}

const READ: Permission = Permission::App(AppPermission::Read);
const DEPLOY: Permission = Permission::App(AppPermission::Deploy);
const MANAGE: Permission = Permission::Global(GlobalPermission::AccountsManage);

#[test]
fn invite_links_are_recognized_only_by_their_exact_prefix() {
    assert_eq!(
        Invite::parse("#operator=s3cret"),
        Some(Invite::Operator("s3cret".into()))
    );
    assert_eq!(
        Invite::parse("#enroll=abc"),
        Some(Invite::Enrollment("abc".into()))
    );
    for fragment in ["", "#invite", "#device=ABCD-EFGH", "#enrollment=abc"] {
        assert_eq!(Invite::parse(fragment), None, "{fragment}");
    }
}

#[test]
fn a_secret_link_wins_over_setup_and_an_existing_session() {
    let invite = Invite::Operator("secret".into());
    let page = SignInPage::new(Some(invite.clone()), false, Some(&alice()), "");
    assert_eq!(page, SignInPage::Redeem(invite));
    assert_eq!(SignInPage::new(None, false, None, ""), SignInPage::Setup);
}

#[test]
fn the_device_fragment_opens_approval_once_signed_in_as_an_account() {
    let alice = alice();
    let page = |fragment| SignInPage::new(None, true, Some(&alice), fragment);
    let approval = |code: &str| SignInPage::DeviceApproval {
        who: "alice".into(),
        code: code.into(),
    };
    assert_eq!(page("#device"), approval(""));
    assert_eq!(page("#device="), approval(""));
    assert_eq!(page("#device=ABCD-EFGH"), approval("ABCD-EFGH"));
    let signed_in = SignInPage::SignedIn {
        who: "alice".into(),
    };
    assert_eq!(page("#devices"), signed_in);
    assert_eq!(page(""), signed_in);
    // Signed out, the page asks for a passkey first.
    assert_eq!(
        SignInPage::new(None, true, None, "#device=ABCD-EFGH"),
        SignInPage::SignIn
    );
}

#[test]
fn the_host_operator_cannot_approve_cli_logins() {
    let operator = session(Principal::Operator(HostOperator { uid: 0 }));
    assert_eq!(
        SignInPage::new(None, true, Some(&operator), "#device=ABCD-EFGH"),
        SignInPage::OperatorCannotApprove
    );
    assert_eq!(
        SignInPage::new(None, true, Some(&operator), ""),
        SignInPage::SignedIn {
            who: "Host operator".into()
        }
    );
}

#[test]
fn device_request_age_rounds_down_to_whole_minutes() {
    let request = |age| DeviceRequest {
        user_code: "ABCD-EFGH".into(),
        requester: None,
        age,
        expires_in: 600,
        grants: None,
        confirms: None,
    };
    for (age, label) in [
        (59, "less than a minute ago"),
        (60, "1 minute ago"),
        (119, "1 minute ago"),
        (120, "2 minutes ago"),
    ] {
        assert_eq!(request(age).age_label(), label, "{age}");
    }
}

#[test]
fn a_single_scope_selection_round_trips() {
    let only_blog = grants(&[
        (READ, Scope::Only([app("app-blog")].into())),
        (DEPLOY, Scope::Only([app("app-blog")].into())),
        (MANAGE, Scope::All),
    ]);
    let selection = GrantSelection::from(&only_blog);
    assert!(!selection.everywhere && !selection.mixed);
    assert_eq!(selection.applications, [app("app-blog")].into());
    assert_eq!(selection.grants(), only_blog);
    // Installation-wide permissions alone count as every application.
    assert!(GrantSelection::from(&grants(&[(MANAGE, Scope::All)])).everywhere);
}

#[test]
fn mixed_scopes_are_flagged_and_replaced_by_their_union() {
    let mixed = grants(&[
        (READ, Scope::Only([app("app-blog")].into())),
        (DEPLOY, Scope::Only([app("app-shop")].into())),
    ]);
    let selection = GrantSelection::from(&mixed);
    assert!(selection.mixed && !selection.everywhere);
    let both = Scope::Only([app("app-blog"), app("app-shop")].into());
    assert_eq!(
        selection.grants(),
        grants(&[(READ, both.clone()), (DEPLOY, both)])
    );
}

#[test]
fn no_chosen_application_keeps_only_installation_wide_permissions() {
    let selection = GrantSelection {
        permissions: [READ, MANAGE].into(),
        everywhere: false,
        applications: BTreeSet::new(),
        mixed: false,
    };
    assert_eq!(selection.grants(), grants(&[(MANAGE, Scope::All)]));
}

#[test]
fn token_form_validates_lifetime_before_tailnet() {
    let form = |days: &str, tailnet: &str| {
        TokenForm {
            name: "ci".into(),
            days: days.into(),
            tailnet: tailnet.into(),
        }
        .command(Grants::default())
    };
    let Ok(Manage::CreateToken { days, tailnet, .. }) = form("", "  tag:ci ") else {
        panic!("blank lifetime and a padded tag are valid");
    };
    assert_eq!(
        (days, tailnet),
        (None, Some(TailnetBinding::parse("tag:ci").unwrap()))
    );
    let days_error = "Enter a positive number of days, or leave blank";
    assert_eq!(form("-1", "not a binding").unwrap_err(), days_error);
    assert_eq!(form("1.5", "").unwrap_err(), days_error);
    assert_eq!(form("0", "").unwrap_err(), days_error);
    form("30", "not a binding").expect_err("`form` should be rejected");
    let Ok(Manage::CreateToken { days, .. }) = form("30", "") else {
        panic!("a positive lifetime is valid");
    };
    assert_eq!(days, Some(30));
}
