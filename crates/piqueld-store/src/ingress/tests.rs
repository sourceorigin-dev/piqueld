use super::*;
use crate::testing::template;
use crate::{Mutation, MutationResponse};
use piqueld_core::NormalizedApplication;
use piqueld_core::manifest::{ApplicationTemplate, Variable};
use std::fmt::Write as _;

fn app(name: &str, hostname: Option<&str>) -> NormalizedApplication {
    routed(name, hostname.as_slice())
}

/// An application routing each of `hostnames` to its service.
fn routed(name: &str, hostnames: &[&str]) -> NormalizedApplication {
    let mut text = format!(
        "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='{name}'\n[[spec.services]]\nname='web'\n[spec.services.source]\ntype='image'\nimage='nginx:alpine'"
    );
    for hostname in hostnames {
        write!(
            text,
            "\n[[spec.routes]]\nhostname='{hostname}'\nservice='web'\nport=80"
        )
        .unwrap();
    }
    piqueld_core::parse_toml(&text)
        .unwrap()
        .normalize(piqueld_core::ApplicationId::parse("input-app").unwrap())
}

async fn save(
    store: &Store,
    application: NormalizedApplication,
    deploy: bool,
) -> Result<piqueld_core::api::SavedApplication, StoreError> {
    let (response, _) = store
        .accept(
            crate::Actor::Daemon,
            Mutation::Save {
                application: Box::new(ApplicationTemplate::from(&application)),
                expected_application_id: None,
                deploy,
            },
            None,
            true,
            None,
        )
        .await?;
    let MutationResponse::Saved(saved) = response else {
        panic!("saved response")
    };
    Ok(saved)
}

#[tokio::test]
async fn installation_hostnames_and_subdomains_are_never_routed() {
    let (_directory, store) = Store::temporary().await;
    let early = app("early", Some("app.piqueld.example.com"));
    let saved = save(&store, early.clone(), true).await.unwrap();
    let id = store.saved_production(&saved).await;
    store
        .stage_routes(&id, &early.spec().routes, true, None)
        .await
        .unwrap();

    // Routes saved before the website hostname was configured are withheld.
    let website = Hostname::parse("piqueld.example.com").unwrap();
    let conflicts = store
        .reserve_installation_hostnames(std::slice::from_ref(&website))
        .await
        .unwrap();
    assert_eq!(conflicts, [early.spec().routes[0].hostname.clone()]);
    let table = store.routing_table().await.unwrap();
    assert_eq!(table[&id], []);
    // The gateway can still acknowledge the table without them.
    store.acknowledge_routes(&table).await.unwrap();

    for hostname in ["piqueld.example.com", "api.piqueld.example.com"] {
        assert!(matches!(
            save(&store, app("late", Some(hostname)), false).await,
            Err(StoreError::HostnameConflict { .. })
        ));
    }
    save(
        &store,
        app("sibling", Some("notpiqueld.example.com")),
        false,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn wildcards_keep_the_names_they_serve_from_other_applications() {
    let (_directory, store) = Store::temporary().await;
    // An exact name claimed first stays its owner's, and wins.
    save(
        &store,
        app("exact", Some("early.tenants.example.com")),
        false,
    )
    .await
    .unwrap();
    save(&store, app("tenants", Some("*.tenants.example.com")), false)
        .await
        .unwrap();
    assert!(matches!(
        save(&store, app("other", Some("late.tenants.example.com")), false).await,
        Err(StoreError::HostnameConflict { hostname }) if hostname == "late.tenants.example.com"
    ));
    // Names it does not match are free, and its own application may
    // route exact names below it.
    save(
        &store,
        app("other", Some("a.late.tenants.example.com")),
        false,
    )
    .await
    .unwrap();
    let both = ["*.tenants.example.com", "admin.tenants.example.com"];
    save(&store, routed("tenants", &both), false).await.unwrap();
}

#[tokio::test]
async fn environments_of_one_application_cannot_share_a_hostname() {
    let (_directory, store) = Store::temporary().await;
    let saved = save(&store, app("one", Some("site.example.com")), false)
        .await
        .unwrap();
    let staging = Mutation::CreateEnvironment {
        application: piqueld_core::ApplicationId::parse(&saved.application_id).unwrap(),
        name: piqueld_core::EnvironmentName::parse("staging").unwrap(),
        source: None,
    };
    assert!(matches!(
        store.force(staging).await,
        Err(StoreError::SharedHostnameConflict { hostname, environment })
            if hostname == "site.example.com" && environment.as_str() == "production"
    ));
    assert_eq!(
        store
            .environments(&piqueld_core::ApplicationId::parse(saved.application_id).unwrap())
            .await
            .unwrap()
            .len(),
        1,
        "failed creation rolls back the environment"
    );
}

#[tokio::test]
async fn environments_render_their_own_hostnames_from_variables() {
    let (_directory, store) = Store::temporary().await;
    let template = template(
        "notes",
        "[spec.variables]\ndomain='piquel.fr'\n[spec.environments.production]\n[spec.environments.staging.variables]\ndomain='staging.piquel.fr'\n[[spec.services]]\nname='web'\n[spec.services.source]\ntype='image'\nimage='nginx:alpine'\n[[spec.routes]]\nhostname='${{ vars.domain }}'\nservice='web'\nport=80",
    );
    let saved = store.save_template(template, false).await;
    let application = piqueld_core::ApplicationId::parse(&saved.application_id).unwrap();
    // The manifest declares both environments.
    let staging = store
        .environments(&application)
        .await
        .unwrap()
        .into_iter()
        .find(|environment| environment.name.as_str() == "staging")
        .unwrap();
    let production = store.production(&application).await;
    let mut deployed = Vec::new();
    for environment in [&production, &staging.id] {
        let (MutationResponse::Operation(operation), _) = store
            .accept(
                crate::Actor::Daemon,
                Mutation::deploy(environment.clone()),
                None,
                true,
                None,
            )
            .await
            .unwrap()
        else {
            panic!("operation response")
        };
        let rendering = store
            .deployment_snapshot(&operation.operation_id)
            .await
            .unwrap()
            .rendering
            .unwrap();
        deployed.push(rendering.application.spec().routes[0].hostname.to_string());
    }
    assert_eq!(deployed, ["piquel.fr", "staging.piquel.fr"]);
    assert!(matches!(
        save(&store, app("other", Some("staging.piquel.fr")), false).await,
        Err(StoreError::HostnameConflict { .. })
    ));

    // Environments that render the same hostname still conflict.
    let same = piqueld_core::edit::Variables {
        defaults: [("domain".into(), Variable::String("piquel.fr".into()))].into(),
        ..Default::default()
    };
    let edit = Mutation::Edit {
        id: application,
        edit: Box::new(piqueld_core::edit::ApplicationEdit::Variables(same)),
        deploy: false,
    };
    assert!(matches!(
        store.force(edit).await,
        Err(StoreError::SharedHostnameConflict { hostname, environment })
            if hostname == "piquel.fr" && environment.as_str() == "production"
    ));
}

#[tokio::test]
async fn replacing_pending_deployment_releases_only_superseded_hostnames() {
    let (_directory, store) = Store::temporary().await;
    save(&store, app("one", Some("old.example.com")), true)
        .await
        .unwrap();
    save(&store, app("one", Some("new.example.com")), true)
        .await
        .unwrap();
    // Commit validates the final snapshot, after the new deployment has
    // replaced the old captured input, rather than the intermediate save.
    save(&store, app("two", Some("old.example.com")), false)
        .await
        .unwrap();
    assert!(matches!(
        save(&store, app("three", Some("new.example.com")), false).await,
        Err(StoreError::HostnameConflict { .. })
    ));
}

#[tokio::test]
async fn hostname_conflict_rolls_back_deployment_and_replay_receipt() {
    let (_directory, store) = Store::temporary().await;
    save(&store, app("one", Some("taken.example.com")), false)
        .await
        .unwrap();
    let saved = save(&store, app("two", None), false).await.unwrap();
    let id = store.saved_production(&saved).await;
    let result = store
        .accept(
            crate::Actor::Daemon,
            Mutation::Save {
                application: Box::new(ApplicationTemplate::from(&app(
                    "two",
                    Some("taken.example.com"),
                ))),
                expected_application_id: None,
                deploy: true,
            },
            None,
            true,
            Some("conflicting-deploy"),
        )
        .await;
    assert!(matches!(result, Err(StoreError::HostnameConflict { .. })));
    assert_eq!(
        store
            .get(&id)
            .await
            .unwrap()
            .manifest()
            .unwrap()
            .spec()
            .routes,
        [] as [piqueld_core::manifest::Route; 0]
    );
    assert!(
        store
            .latest_operation_for_environment(&id)
            .await
            .unwrap()
            .is_none()
    );
    // The rejected request must not consume the replay key.
    store
        .accept(
            crate::Actor::Daemon,
            Mutation::Save {
                application: Box::new(ApplicationTemplate::from(&app(
                    "two",
                    Some("free.example.com"),
                ))),
                expected_application_id: None,
                deploy: true,
            },
            None,
            true,
            Some("conflicting-deploy"),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn concurrent_saves_cannot_claim_the_same_hostname() {
    let (_directory, store) = Store::temporary().await;
    let (one, two) = tokio::join!(
        save(&store, app("one", Some("site.example.com")), false),
        save(&store, app("two", Some("SITE.EXAMPLE.COM.")), false)
    );
    assert!(matches!(
        (&one, &two),
        (Ok(_), Err(StoreError::HostnameConflict { .. }))
            | (Err(StoreError::HostnameConflict { .. }), Ok(_))
    ));
    assert_eq!(
        store.list(None, 10).await.unwrap().items.len(),
        1,
        "failed reservation rolls back the entire save"
    );
}

/// A stopped environment's routes are withdrawn from the gateway, while
/// it keeps their hostnames, certificates and DNS records, which follow
/// the complete table. Deploying it serves them again.
#[tokio::test]
async fn stopped_environments_keep_their_routes_unserved() {
    let (_directory, store) = Store::temporary().await;
    let input = app("one", Some("site.example.com"));
    let saved = save(&store, input.clone(), true).await.unwrap();
    let id = store.saved_production(&saved).await;
    let routes = &input.spec().routes;
    store.stage_routes(&id, routes, true, None).await.unwrap();
    let accept = async |mutation| {
        let accepted = store.accept(crate::Actor::Daemon, mutation, None, true, None);
        accepted.await.unwrap();
    };

    accept(Mutation::Stop { id: id.clone() }).await;
    assert_eq!(store.served_table().await.unwrap()[&id], []);
    assert_eq!(&store.routing_table().await.unwrap()[&id], routes);
    store
        .acknowledge_routes(&store.served_table().await.unwrap())
        .await
        .unwrap();
    assert!(matches!(
        save(&store, app("two", Some("site.example.com")), false).await,
        Err(StoreError::HostnameConflict { .. })
    ));

    accept(Mutation::deploy(id.clone())).await;
    assert_eq!(&store.served_table().await.unwrap()[&id], routes);
}

/// One hostname's routes read alone agree with the full tables.
#[tokio::test]
async fn one_hostnames_routes_match_the_tables() {
    let (_directory, store) = Store::temporary().await;
    let input = app("one", Some("site.example.com"));
    let saved = save(&store, input.clone(), false).await.unwrap();
    let id = store.saved_production(&saved).await;
    let hostname = RouteHostname::parse("site.example.com").unwrap();
    assert_eq!(
        store.hostname_routes(&hostname).await.unwrap(),
        HostnameRoutes::default()
    );
    let routes = &input.spec().routes;
    store.stage_routes(&id, routes, true, None).await.unwrap();
    let desired = HostnameRoutes {
        desired: true,
        ..HostnameRoutes::default()
    };
    assert_eq!(store.hostname_routes(&hostname).await.unwrap(), desired);
    store
        .acknowledge_routes(&store.routing_table().await.unwrap())
        .await
        .unwrap();
    store.track_dns_records(&hostname, false).await.unwrap();
    let applied = HostnameRoutes {
        applied: Some(routes[0].visibility),
        desired: true,
        tracked: true,
    };
    assert_eq!(store.hostname_routes(&hostname).await.unwrap(), applied);
    // The installation's own hostnames are never desired routes.
    let installation = Hostname::parse("example.com").unwrap();
    store
        .reserve_installation_hostnames(std::slice::from_ref(&installation))
        .await
        .unwrap();
    assert!(!store.hostname_routes(&hostname).await.unwrap().desired);
}

#[tokio::test]
async fn only_changed_routes_are_acknowledged() {
    let (_directory, store) = Store::temporary().await;
    let input = app("one", Some("site.example.com"));
    let saved = save(&store, input.clone(), false).await.unwrap();
    let id = store.saved_production(&saved).await;
    store
        .stage_routes(&id, &input.spec().routes, true, None)
        .await
        .unwrap();
    let table = store.routing_table().await.unwrap();
    assert!(store.acknowledge_routes(&table).await.unwrap());
    assert!(!store.acknowledge_routes(&table).await.unwrap());
    assert_eq!(store.applied_table().await.unwrap(), table);
    store.stage_routes(&id, &[], true, None).await.unwrap();
    let withdrawn = store.routing_table().await.unwrap();
    assert!(store.acknowledge_routes(&withdrawn).await.unwrap());
    assert_eq!(store.applied_table().await.unwrap(), withdrawn);
}

/// Concurrent acknowledgements run one at a time, in order, so the later
/// table ends up applied whole: the earlier one can't compare against
/// stale routes and leave half of each.
#[tokio::test]
async fn concurrent_acknowledgements_end_with_the_later_table() {
    let (_directory, store) = Store::temporary().await;
    let mut routes = Vec::new();
    for (name, hostname) in [("one", "one.example.com"), ("two", "two.example.com")] {
        let input = app(name, Some(hostname));
        let saved = save(&store, input.clone(), false).await.unwrap();
        let id = store.saved_production(&saved).await;
        store.stage_routes(&id, &[], true, None).await.unwrap();
        routes.push((id, input.spec().routes.clone()));
    }
    let [(one, one_routes), (two, two_routes)] = <[_; 2]>::try_from(routes).unwrap();
    let empty = RoutingTable::from([(one.clone(), vec![]), (two.clone(), vec![])]);
    assert_eq!(store.applied_table().await.unwrap(), empty);
    let earlier = RoutingTable::from([(one.clone(), one_routes), (two.clone(), vec![])]);
    let later = RoutingTable::from([(one, vec![]), (two, two_routes)]);

    // Both queue behind a held acknowledgement, the earlier first: each
    // is polled until it waits for the lock.
    let held = store.acknowledgements.lock().await;
    let mut first = std::pin::pin!(store.acknowledge_routes(&earlier));
    assert!(futures_util::poll!(first.as_mut()).is_pending());
    let mut second = std::pin::pin!(store.acknowledge_routes(&later));
    assert!(futures_util::poll!(second.as_mut()).is_pending());
    drop(held);
    let (first, second) = tokio::join!(first, second);
    assert!(first.unwrap() && second.unwrap());
    assert_eq!(store.applied_table().await.unwrap(), later);
}

#[tokio::test]
async fn deployed_names_survive_disabled_ingress_and_failed_gateway_withdrawal() {
    let (_directory, store) = Store::temporary().await;
    let input = app("one", Some("site.example.com"));
    let saved = save(&store, input.clone(), false).await.unwrap();
    let id = store.saved_production(&saved).await;
    store
        .stage_routes(&id, &input.spec().routes, true, None)
        .await
        .unwrap();
    store
        .acknowledge_routes(&store.routing_table().await.unwrap())
        .await
        .unwrap();
    save(&store, app("one", None), false).await.unwrap();
    assert!(matches!(
        save(&store, app("two", Some("site.example.com")), false).await,
        Err(StoreError::HostnameConflict { .. })
    ));
    store.stage_routes(&id, &[], true, None).await.unwrap();
    assert!(
        matches!(
            save(&store, app("two", Some("site.example.com")), false).await,
            Err(StoreError::HostnameConflict { .. })
        ),
        "last accepted configuration still reserves its host"
    );
    store
        .acknowledge_routes(&store.routing_table().await.unwrap())
        .await
        .unwrap();
    save(&store, app("two", Some("site.example.com")), false)
        .await
        .unwrap();
}

#[tokio::test]
async fn captured_and_fetched_deployments_reserve_hostnames() {
    let (_directory, store) = Store::temporary().await;
    let saved = save(&store, app("one", Some("site.example.com")), true)
        .await
        .unwrap();
    save(&store, app("one", None), false).await.unwrap();
    assert!(matches!(
        save(&store, app("two", Some("site.example.com")), false).await,
        Err(StoreError::HostnameConflict { .. })
    ));
    save(&store, app("two", Some("taken.example.com")), false)
        .await
        .unwrap();
    let operation = store.operation(&saved.operation_id.unwrap()).await.unwrap();
    store.mark_running(&operation.id).await;
    let environment = store.get(&operation.environment_id).await.unwrap();
    let fetched = ApplicationTemplate::from(&app("one", Some("taken.example.com")))
        .with_id(environment.application.application.id().clone());
    let rendering = environment.render(&fetched, &operation.id).unwrap();
    assert!(matches!(
        store
            .save_deployment_input(&operation, (&fetched, None), &rendering, (None, None), &[])
            .await,
        Err(StoreError::HostnameConflict { .. })
    ));
}

#[tokio::test]
async fn superseded_operations_cannot_publish_routes() {
    let (_directory, store) = Store::temporary().await;
    let input = app("one", Some("site.example.com"));
    let saved = save(&store, input.clone(), true).await.unwrap();
    let id = store.saved_production(&saved).await;
    let operation = saved.operation_id.unwrap();
    store.mark_running(&operation).await;
    save(&store, app("one", Some("new.example.com")), true)
        .await
        .unwrap();
    assert!(matches!(
        store
            .stage_routes(&id, &input.spec().routes, true, Some(&operation))
            .await,
        Err(StoreError::IllegalTransition)
    ));
    assert!(store.routing_table().await.unwrap().is_empty());
}

#[tokio::test]
async fn withdrawals_precede_cutover_and_repointing_waits_for_readiness() {
    let (_directory, store) = Store::temporary().await;
    let input = app("one", Some("site.example.com"));
    let saved = save(&store, input.clone(), false).await.unwrap();
    let id = store.saved_production(&saved).await;
    store
        .stage_routes(&id, &input.spec().routes, true, None)
        .await
        .unwrap();
    let mut changed = input.spec().routes.clone();
    changed[0].target =
        serde_json::from_value(serde_json::json!({"service":"web","port":3000})).unwrap();
    store
        .stage_routes(&id, &changed, false, None)
        .await
        .unwrap();
    assert_eq!(
        store.routing_table().await.unwrap()[&id][0]
            .target
            .to_string(),
        "web:80"
    );
    store.stage_routes(&id, &changed, true, None).await.unwrap();
    assert_eq!(
        store.routing_table().await.unwrap()[&id][0]
            .target
            .to_string(),
        "web:3000"
    );
    store.stage_routes(&id, &[], false, None).await.unwrap();
    assert_eq!(
        store.routing_table().await.unwrap()[&id],
        [] as [ValidatedRoute; 0]
    );
}

#[tokio::test]
async fn changing_visibility_withdraws_the_route_until_backends_are_ready() {
    let (_directory, store) = Store::temporary().await;
    let input = app("one", Some("site.example.com"));
    let saved = save(&store, input.clone(), false).await.unwrap();
    let id = store.saved_production(&saved).await;
    let mut public = input.spec().routes.clone();
    public[0].visibility = piqueld_core::manifest::Visibility::Public;
    store.stage_routes(&id, &public, true, None).await.unwrap();
    // Public -> private leaves the public listener as the deployment
    // starts; the private route joins once backends are ready.
    let private = input.spec().routes.clone();
    store
        .stage_routes(&id, &private, false, None)
        .await
        .unwrap();
    assert_eq!(
        store.routing_table().await.unwrap()[&id],
        [] as [ValidatedRoute; 0]
    );
    store.stage_routes(&id, &private, true, None).await.unwrap();
    assert_eq!(store.routing_table().await.unwrap()[&id], private);
}

#[tokio::test]
async fn route_statuses_are_limited_to_readable_applications() {
    let (_directory, store) = Store::temporary().await;
    let mut statuses = Vec::new();
    let mut applications = Vec::new();
    for (name, hostname) in [("one", "one.example.com"), ("two", "two.example.com")] {
        let input = app(name, Some(hostname));
        let saved = save(&store, input.clone(), false).await.unwrap();
        let application = piqueld_core::ApplicationId::parse(saved.application_id).unwrap();
        let route = &input.spec().routes[0];
        statuses.push(RouteStatus {
            name: None,
            environment_id: store.production(&application).await.to_string(),
            hostname: hostname.into(),
            visibility: route.visibility,
            dns: piqueld_core::api::DnsRecords::ServerAddresses {
                addresses: Vec::new(),
            },
            dns_state: piqueld_core::api::DnsRecordState::Manual,
            target: route.target.clone(),
            state: "ready".into(),
            message: String::new(),
        });
        applications.push(application);
    }
    let hostnames = |routes: Vec<RouteStatus>| {
        routes
            .into_iter()
            .map(|route| route.hostname)
            .collect::<Vec<_>>()
    };
    let all = store.readable_routes(&Scope::All, statuses.clone()).await;
    assert_eq!(
        hostnames(all.unwrap()),
        ["one.example.com", "two.example.com"]
    );
    let one = Scope::one(applications[0].clone());
    let limited = store.readable_routes(&one, statuses.clone()).await;
    assert_eq!(hostnames(limited.unwrap()), ["one.example.com"]);
    let none = store.readable_routes(&Scope::NONE, statuses).await;
    assert_eq!(hostnames(none.unwrap()), [] as [String; 0]);
}
