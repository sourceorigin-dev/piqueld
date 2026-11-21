use super::*;

fn id(value: &str) -> ApplicationId {
    ApplicationId::parse(value).unwrap()
}

fn only(values: &[&str]) -> Scope {
    Scope::Only(values.iter().map(|value| id(value)).collect())
}

fn grants(list: &[(&str, Option<&[&str]>)]) -> Grants {
    list.iter()
        .map(|(permission, applications)| Grant {
            permission: permission.parse().unwrap(),
            applications: applications.map(|ids| ids.iter().map(|value| id(value)).collect()),
        })
        .collect::<Vec<_>>()
        .try_into()
        .unwrap()
}

/// Hidden applications do not leak; visible ones name the missing permission.
#[test]
fn grant_descriptions_name_scoped_applications_only() {
    let grants = grants(&[
        ("apps:read", None),
        ("apps:deploy", Some(&["app-blog0000", "app-shop0000"])),
        ("accounts:manage", None),
    ]);
    assert_eq!(
        grants.describe(|id| id.as_str().to_uppercase()),
        [
            "apps:read on every application",
            "apps:deploy on APP-BLOG0000, APP-SHOP0000",
            "accounts:manage",
        ]
    );
    assert_eq!(
        Grants::default().describe(ToString::to_string),
        Vec::<String>::new()
    );
}

#[test]
fn application_checks_hide_unreadable_applications() {
    let blog = id("app-blog0000");
    let shop = id("app-shop0000");
    let grants = grants(&[("apps:deploy", Some(&["app-blog0000"]))]);
    assert_eq!(grants.require_app(AppPermission::Read, &blog), Ok(()));
    assert_eq!(grants.require_app(AppPermission::Deploy, &blog), Ok(()));
    assert_eq!(
        grants.require_app(AppPermission::Delete, &blog),
        Err(Denied::Missing(Permission::App(AppPermission::Delete)))
    );
    assert_eq!(
        grants.require_app(AppPermission::Read, &shop),
        Err(Denied::Hidden)
    );
    assert_eq!(
        grants.require(Permission::App(AppPermission::LogsRead)),
        Err(Denied::Missing(Permission::App(AppPermission::LogsRead)))
    );
    // An unknown target is hidden unless the permission covers everything.
    let deploy = [AppPermission::Deploy];
    assert_eq!(
        grants.require_change(&deploy, Target::Unknown),
        Err(Denied::Hidden)
    );
    assert_eq!(
        Grants::admin().require_change(&deploy, Target::Unknown),
        Ok(())
    );
}

/// `admin` on applications covers application permissions there, but no
/// installation-wide permission; on every application it covers everything.
#[test]
fn admin_scope_bounds_its_abilities() {
    let blog = id("app-blog0000");
    let scoped = grants(&[("admin", Some(&["app-blog0000"]))]);
    assert_eq!(scoped.require_app(AppPermission::Delete, &blog), Ok(()));
    assert!(!scoped.has_global(GlobalPermission::AccountsManage));
    assert!(Grants::admin().has_global(GlobalPermission::AccountsManage));
    assert_eq!(Grants::admin().app_scope(AppPermission::Delete), Scope::All);
}

/// Handing out access requires holding it; `admin` needs `admin`.
#[test]
fn covers_prevents_escalation() {
    let developer = Preset::Developer.grants(&Scope::All);
    assert!(developer.covers(&Preset::Deploy.grants(&only(&["app-blog0000"]))));
    assert!(!developer.covers(&Preset::Admin.grants(&only(&["app-blog0000"]))));
    assert!(!developer.covers(&grants(&[("accounts:manage", None)])));
    let scoped = Preset::Developer.grants(&only(&["app-blog0000"]));
    assert!(!scoped.covers(&Preset::Deploy.grants(&Scope::All)));
    assert!(Grants::admin().covers(&developer));
}

/// A token limited by its owner holds only what both hold.
#[test]
fn intersection_keeps_shared_access() {
    let owner = grants(&[("admin", Some(&["app-blog0000"])), ("system:read", None)]);
    let token = grants(&[
        ("apps:deploy", None),
        ("system:read", None),
        ("accounts:manage", None),
    ]);
    let effective = owner.intersection(&token);
    assert_eq!(
        effective,
        grants(&[
            ("apps:deploy", Some(&["app-blog0000"])),
            ("system:read", None)
        ])
    );
    assert_eq!(
        Grants::admin().intersection(&Grants::admin()),
        Grants::admin()
    );
}

/// Creators get grants on their new application only for permissions they
/// hold on some, but not all, applications.
#[test]
fn creators_receive_matching_grants() {
    let new = id("app-new00000");
    let creator = grants(&[
        ("apps:write", Some(&["app-blog0000"])),
        ("apps:deploy", None),
        ("apps:create", None),
    ]);
    assert_eq!(
        creator.for_created_application(&new),
        grants(&[("apps:write", Some(&["app-new00000"]))])
    );
    assert!(Grants::admin().for_created_application(&new).is_empty());
}

/// Equal access has one representation however it was written.
#[test]
fn equal_access_compares_equal() {
    let blog = Some(&["app-blog0000"][..]);
    assert_eq!(
        grants(&[("apps:deploy", blog)]),
        grants(&[("apps:read", blog), ("apps:deploy", blog)])
    );
    assert_eq!(
        grants(&[("admin", blog), ("apps:delete", blog), ("logs:read", None)]),
        grants(&[("admin", blog), ("logs:read", None)])
    );
    assert_eq!(
        grants(&[("admin", None), ("system:read", None)]),
        Grants::admin()
    );
    assert_eq!(
        Preset::Developer
            .grants(&Scope::All)
            .intersection(&grants(&[("apps:deploy", blog)])),
        grants(&[("apps:deploy", blog)])
    );
}

/// The wire list round-trips canonically and rejects invalid scopes.
#[test]
fn wire_list_is_validated_and_canonical() {
    let merged = grants(&[
        ("apps:read", Some(&["app-blog0000"])),
        ("apps:read", Some(&["app-shop0000"])),
    ]);
    assert_eq!(
        serde_json::to_value(&merged).unwrap(),
        serde_json::json!([{"permission": "apps:read", "applications": ["app-blog0000", "app-shop0000"]}])
    );
    for invalid in [
        serde_json::json!([{"permission": "system:read", "applications": ["app-blog0000"]}]),
        serde_json::json!([{"permission": "apps:read", "applications": []}]),
        serde_json::json!([{"permission": "apps:fly"}]),
    ] {
        assert!(serde_json::from_value::<Grants>(invalid).is_err());
    }
}

/// A selection limited to applications keeps installation-wide
/// permissions everywhere, and scoped presets omit daemon status.
#[test]
fn selections_scope_only_application_permissions() {
    let blog = only(&["app-blog0000"]);
    let mut selected = Preset::Developer.grants(&blog);
    selected.grant_within("accounts:manage".parse().unwrap(), &blog);
    assert!(selected.has_global(GlobalPermission::AccountsManage));
    assert!(!selected.has_global(GlobalPermission::SystemRead));
    assert_eq!(selected.app_scope(AppPermission::Exec), blog);
    assert!(
        Preset::ReadOnly
            .grants(&Scope::All)
            .has_global(GlobalPermission::SystemRead)
    );
    assert!(Preset::Admin.grants(&Scope::NONE).is_empty());
}

/// Previews permissions never act on the whole application, read
/// previews' logs, and are included in `apps:deploy` and `apps:delete`.
#[test]
fn previews_permissions_stop_at_previews() {
    let blog = id("app-blog0000");
    let only_blog = Some(&["app-blog0000"][..]);
    let agent = grants(&[
        ("previews:deploy", only_blog),
        ("previews:delete", only_blog),
    ]);
    let (preview, whole) = (Target::Preview(&blog), Target::Id(&blog));
    let logs = [AppPermission::LogsRead];
    assert_eq!(agent.require_change(&logs, preview), Ok(()));
    for permission in [
        AppPermission::Deploy,
        AppPermission::LogsRead,
        AppPermission::Delete,
    ] {
        assert_eq!(
            agent.require_change(&[permission], whole),
            Err(Denied::Missing(Permission::App(permission)))
        );
    }
    assert_eq!(agent.require_app(AppPermission::Read, &blog), Ok(()));
    assert_eq!(
        agent.require_change(&[AppPermission::Exec], preview),
        Err(Denied::Missing(Permission::App(AppPermission::Exec)))
    );
    // Whole-application permissions include previews permissions, never
    // the reverse, whether handing them out or limiting a token.
    let deployer = grants(&[("apps:deploy", only_blog)]);
    let previews = grants(&[("previews:deploy", only_blog)]);
    assert!(deployer.covers(&previews));
    assert!(!previews.covers(&deployer));
    assert_eq!(deployer.intersection(&previews), previews);
    assert_eq!(previews.intersection(&deployer), previews);
    assert_eq!(
        grants(&[("apps:deploy", only_blog), ("previews:deploy", only_blog)]),
        deployer
    );
    assert_eq!(
        deployer.require_change(&[AppPermission::PreviewsDeploy], whole),
        Ok(())
    );
}

/// New grants escalate when they add `admin` on an application the account
/// lacked it on, or `accounts:manage` or `system:operate` it didn't hold.
#[test]
fn escalation_is_newly_held_admin_or_installation_power() {
    let blog = grants(&[("admin", Some(&["app-blog0000"]))]);
    let blog_and_shop = grants(&[("admin", Some(&["app-blog0000", "app-shop0000"]))]);
    assert!(blog_and_shop.escalates(&blog));
    assert!(!blog.escalates(&blog_and_shop));
    let operate = grants(&[("system:operate", None)]);
    assert!(operate.escalates(&Grants::default()));
    assert!(!Grants::default().escalates(&operate));
    // `admin` everywhere already holds `accounts:manage`.
    let mut managing = Grants::admin();
    managing
        .grant(
            Permission::Global(GlobalPermission::AccountsManage),
            &Scope::All,
        )
        .expect("accounts:manage applies to the whole installation");
    assert!(!managing.escalates(&Grants::admin()));
    // Ordinary access never escalates.
    let deploy = grants(&[("apps:deploy", None)]);
    assert!(!deploy.escalates(&Grants::default()));
}
