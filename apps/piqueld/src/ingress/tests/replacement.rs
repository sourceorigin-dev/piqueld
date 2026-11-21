//! Steps of the routing scenario that replace, recover and resynchronize
//! the gateway.
use super::*;

impl Scenario {
    async fn reject_invalid_replacement_configuration(
        &self,
        table: &piqueld_store::ingress::RoutingTable,
        networks: &std::collections::BTreeSet<String>,
    ) {
        let mut invalid = self.ingress(true);
        invalid.issuer = Some(json!({"module":"not-a-real-issuer"}));
        let original = self.gateway.container_id().await;
        let error = invalid
            .replace_gateway(table, networks, &invalid.container_spec())
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("configuration validation failed"),
            "{error:#}"
        );
        assert_eq!(self.gateway.container_id().await, original);
        assert_eq!(self.body("one.example.test").await, "first backend");
    }

    pub(super) async fn replacement_preserves_unverified_attachment(&self) {
        let original = self.gateway.container_id().await;
        let table = self.store.routing_table().await.unwrap();
        // A retained route still works on the old attachment, but a replacement
        // cannot inherit it unless its network passed validation.
        self.gateway
            .replace_gateway(
                &table,
                &std::collections::BTreeSet::new(),
                &self.gateway.container_spec(),
            )
            .await
            .unwrap();
        assert_eq!(self.gateway.container_id().await, original);
        assert_eq!(self.body("one.example.test").await, "first backend");
    }

    pub(super) async fn replacement_failure_and_recovery(&self) {
        let original = self.gateway.container_id().await;
        let table = self.store.routing_table().await.unwrap();
        let (_, networks, _) = self.gateway.prepare_routes(&table).await.unwrap();
        let mut spec = self.gateway.container_spec();
        spec["Image"] = "piqueld-missing-test-image:never-pull".into();
        assert!(
            self.gateway
                .replace_gateway(&table, &networks, &spec)
                .await
                .is_err()
        );
        assert_eq!(self.gateway.container_id().await, original);
        assert_eq!(self.body("one.example.test").await, "first backend");

        self.reject_invalid_replacement_configuration(&table, &networks)
            .await;

        let mut spec = self.gateway.container_spec();
        spec["Cmd"] = json!(["/does-not-exist"]);
        let error = self
            .gateway
            .replace_gateway(&table, &networks, &spec)
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("previous gateway restored"),
            "{error:#}"
        );
        assert_eq!(self.gateway.container_id().await, original);
        assert_eq!(self.body("one.example.test").await, "first backend");

        // Simulate process termination after the old gateway was stopped. Recovery
        // uses Docker names and the persisted snapshot, not in-memory state.
        let configuration = self.gateway.caddy.get("/config/").await.unwrap();
        tokio::fs::write(
            self.directory
                .path()
                .join("ingress/config/caddy/rollback.json"),
            serde_json::to_vec(&configuration).unwrap(),
        )
        .await
        .unwrap();
        let previous = format!("{}-previous", self.gateway.name);
        let docker = self.gateway.external();
        docker.stop_container(&self.gateway.name, 1).await.unwrap();
        docker
            .rename_container(&self.gateway.name, &previous)
            .await
            .unwrap();
        self.gateway.recover_gateway().await.unwrap();
        assert_eq!(self.gateway.container_id().await, original);
        assert_eq!(self.body("one.example.test").await, "first backend");

        self.gateway
            .replace_gateway(&table, &networks, &self.gateway.container_spec())
            .await
            .unwrap();
        assert_ne!(self.gateway.container_id().await, original);
        assert_eq!(self.body("one.example.test").await, "first backend");
        assert!(
            self.gateway
                .docker
                .inspect_container(&previous)
                .await
                .unwrap()
                .is_none()
        );
    }

    /// A committed replacement is never reverted because cleanup failed.
    pub(super) async fn committed_replacement_survives_stale_cleanup(&self) {
        let previous = format!("{}-previous", self.gateway.name);
        let replaced = self.gateway.container_id().await;
        self.gateway
            .external()
            .create_container(&previous, &self.gateway.container_spec())
            .await
            .unwrap();
        self.gateway.recover_gateway().await.unwrap();
        assert_eq!(self.gateway.container_id().await, replaced);
        assert!(
            self.gateway
                .docker
                .inspect_container(&previous)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(self.body("one.example.test").await, "first backend");
    }

    async fn unavailable_application(&self) -> (EnvironmentId, NormalizedApplication) {
        let app = application("unavailable", "unavailable.example.test", "unavailable");
        let (unavailable, _) = save(&self.store, Actor::Daemon, &app, false).await;
        (unavailable, app)
    }

    pub(super) async fn unavailable_network_does_not_block_withdrawals(&self) {
        let (unavailable, app) = self.unavailable_application().await;
        self.store
            .stage_routes(&unavailable, &app.spec().routes, true, None)
            .await
            .unwrap();
        // Model an accepted route whose network subsequently disappeared.
        self.store
            .acknowledge_routes(&self.store.routing_table().await.unwrap())
            .await
            .unwrap();
        let mut pending = app.spec().routes.clone();
        pending[0].target = serde_json::from_value(json!({"service":"web","port":8081})).unwrap();
        self.store
            .stage_routes(&unavailable, &pending, true, None)
            .await
            .unwrap();
        let original = self.gateway.container_id().await;
        let mut upgraded = self.replica();
        upgraded
            .extra_hosts
            .push("upgrade.example.test:127.0.0.1".into());
        upgraded.synchronize_for(Some(&self.first)).await.unwrap();
        assert!(!upgraded.status().await.healthy);
        assert_eq!(
            self.store.applied_routes(&unavailable).await.unwrap(),
            app.spec().routes
        );
        assert_eq!(self.body("one.example.test").await, "first backend");
        assert_eq!(upgraded.container_id().await, original);
        let network = piqueld_core::DockerNetworkName::for_ingress(&unavailable).to_string();
        self.gateway
            .external()
            .create_bridge_network(&network, &json!({}))
            .await
            .unwrap();
        upgraded.synchronize_for(Some(&self.first)).await.unwrap();
        assert!(
            self.gateway.container().await.unwrap().unwrap()["NetworkSettings"]["Networks"]
                .get(&network)
                .is_none(),
            "a conflicting network must never be attached"
        );
        let routes = self.store.applied_routes(&self.first).await.unwrap();
        self.store
            .stage_routes(&self.first, &[], true, None)
            .await
            .unwrap();
        upgraded.synchronize_for(Some(&self.first)).await.unwrap();
        assert_eq!(
            self.store.applied_routes(&self.first).await.unwrap(),
            [] as [piqueld_core::manifest::ValidatedRoute; 0]
        );
        assert_eq!(self.plain("one.example.test", "").await.status(), 404);
        // Healthy applications can also restore/add routes while this app is broken.
        self.store
            .stage_routes(&self.first, &routes, true, None)
            .await
            .unwrap();
        upgraded.synchronize_for(Some(&self.first)).await.unwrap();
        assert_eq!(self.body("one.example.test").await, "first backend");
        assert_eq!(upgraded.container_id().await, original);
        self.store
            .stage_routes(&unavailable, &[], true, None)
            .await
            .unwrap();
        upgraded.synchronize().await.unwrap();
        assert!(upgraded.status().await.healthy);
        assert_ne!(upgraded.container_id().await, original);
        assert_eq!(self.body("one.example.test").await, "first backend");
        self.gateway
            .external()
            .remove_network(&network)
            .await
            .unwrap();
        // Restore the controller's specification before testing ordinary reloads.
        self.gateway.synchronize().await.unwrap();
    }
}
