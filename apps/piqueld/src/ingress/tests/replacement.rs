//! Steps of the routing scenario that replace, recover and resynchronize
//! the gateway.
use super::*;
use piqueld_core::ErrorChain as _;

impl Scenario {
    async fn reject_invalid_replacement_configuration(
        &self,
        table: &RoutingTable,
        networks: &BTreeSet<String>,
    ) {
        let mut invalid = self.ingress(true);
        invalid.issuer = Some(json!({"module":"not-a-real-issuer"}));
        let original = self.gateway.container_id().await;
        let error = invalid
            .replace_gateway(table, networks, &invalid.container_spec())
            .await
            .expect_err("replace gateway fails");
        assert!(
            error.chain().contains("configuration validation failed"),
            "{}",
            error.chain()
        );
        assert_eq!(self.gateway.container_id().await, original);
        assert_eq!(self.body("one.example.test").await, "first backend");
    }

    pub(super) async fn replacement_preserves_unverified_attachment(&self) {
        let original = self.gateway.container_id().await;
        let table = self
            .store
            .routing_table()
            .await
            .expect("the routing table loads");
        self.gateway
            .replace_gateway(&table, &BTreeSet::new(), &self.gateway.container_spec())
            .await
            .expect("replace gateway succeeds");
        assert_eq!(self.gateway.container_id().await, original);
        assert_eq!(self.body("one.example.test").await, "first backend");
    }

    pub(super) async fn replacement_failure_and_recovery(&self) {
        let original = self.gateway.container_id().await;
        let table = self
            .store
            .routing_table()
            .await
            .expect("the routing table loads");
        let (_, networks, _) = self
            .gateway
            .prepare_routes(&table)
            .await
            .expect("prepare routes succeeds");
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
            .expect_err("replace gateway fails");
        assert!(
            error.chain().contains("previous gateway restored"),
            "{}",
            error.chain()
        );
        assert_eq!(self.gateway.container_id().await, original);
        assert_eq!(self.body("one.example.test").await, "first backend");

        let configuration = self
            .gateway
            .caddy
            .get("/config/")
            .await
            .expect("the entry exists");
        tokio::fs::write(
            self.directory
                .path()
                .join("ingress/config/caddy/rollback.json"),
            serde_json::to_vec(&configuration).expect("the value serializes"),
        )
        .await
        .expect("the file is written");
        let previous = format!("{}-previous", self.gateway.name);
        let docker = self.gateway.external();
        docker
            .stop_container(&self.gateway.name, 1)
            .await
            .expect("stop container succeeds");
        docker
            .rename_container(&self.gateway.name, &previous)
            .await
            .expect("rename container succeeds");
        self.gateway
            .recover_gateway()
            .await
            .expect("recover gateway succeeds");
        assert_eq!(self.gateway.container_id().await, original);
        assert_eq!(self.body("one.example.test").await, "first backend");

        self.gateway
            .replace_gateway(&table, &networks, &self.gateway.container_spec())
            .await
            .expect("replace gateway succeeds");
        assert_ne!(self.gateway.container_id().await, original);
        assert_eq!(self.body("one.example.test").await, "first backend");
        assert!(
            self.gateway
                .docker
                .inspect_container(&previous)
                .await
                .expect("inspect container succeeds")
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
            .expect("the container is created");
        self.gateway
            .recover_gateway()
            .await
            .expect("recover gateway succeeds");
        assert_eq!(self.gateway.container_id().await, replaced);
        assert!(
            self.gateway
                .docker
                .inspect_container(&previous)
                .await
                .expect("inspect container succeeds")
                .is_none()
        );
        assert_eq!(self.body("one.example.test").await, "first backend");
    }

    async fn unavailable_application(&self) -> (EnvironmentId, NormalizedApplication) {
        let app = application("unavailable", "unavailable.example.test", "unavailable");
        let (unavailable, _) = save(&self.store, Actor::Daemon, &app, false).await;
        (unavailable, app)
    }

    #[expect(
        clippy::too_many_lines,
        reason = "One scenario walks the whole withdrawal sequence; rustfmt puts each `expect` on its own line."
    )]
    pub(super) async fn unavailable_network_does_not_block_withdrawals(&self) {
        let (unavailable, app) = self.unavailable_application().await;
        self.store
            .stage_routes(&unavailable, &app.spec().routes, true, None)
            .await
            .expect("stage routes succeeds");
        self.store
            .acknowledge_routes(
                &self
                    .store
                    .routing_table()
                    .await
                    .expect("the routing table loads"),
            )
            .await
            .expect("acknowledge routes succeeds");
        let mut pending = app.spec().routes.clone();
        pending[0].target = serde_json::from_value(json!({"service":"web","port":8081}))
            .expect("the value deserializes");
        self.store
            .stage_routes(&unavailable, &pending, true, None)
            .await
            .expect("stage routes succeeds");
        let original = self.gateway.container_id().await;
        let mut upgraded = self.replica();
        upgraded
            .extra_hosts
            .push("upgrade.example.test:127.0.0.1".into());
        upgraded
            .synchronize_for(Some(&self.first))
            .await
            .expect("the ingress synchronizes");
        assert!(!upgraded.status().await.healthy);
        assert_eq!(
            self.store
                .applied_routes(&unavailable)
                .await
                .expect("the applied routes load"),
            app.spec().routes
        );
        assert_eq!(self.body("one.example.test").await, "first backend");
        assert_eq!(upgraded.container_id().await, original);
        let network = piqueld_core::DockerNetworkName::for_ingress(&unavailable).to_string();
        self.gateway
            .external()
            .create_bridge_network(&network, &json!({}))
            .await
            .expect("create bridge network succeeds");
        upgraded
            .synchronize_for(Some(&self.first))
            .await
            .expect("the ingress synchronizes");
        assert!(
            self.gateway
                .container()
                .await
                .expect("the container lookup succeeds")
                .expect("the container exists")["NetworkSettings"]["Networks"]
                .get(&network)
                .is_none(),
            "a conflicting network must never be attached"
        );
        let routes = self
            .store
            .applied_routes(&self.first)
            .await
            .expect("the applied routes load");
        self.store
            .stage_routes(&self.first, &[], true, None)
            .await
            .expect("stage routes succeeds");
        upgraded
            .synchronize_for(Some(&self.first))
            .await
            .expect("the ingress synchronizes");
        assert_eq!(
            self.store
                .applied_routes(&self.first)
                .await
                .expect("the applied routes load"),
            [] as [ValidatedRoute; 0]
        );
        assert_eq!(self.plain("one.example.test", "").await.status(), 404);
        self.store
            .stage_routes(&self.first, &routes, true, None)
            .await
            .expect("stage routes succeeds");
        upgraded
            .synchronize_for(Some(&self.first))
            .await
            .expect("the ingress synchronizes");
        assert_eq!(self.body("one.example.test").await, "first backend");
        assert_eq!(upgraded.container_id().await, original);
        self.store
            .stage_routes(&unavailable, &[], true, None)
            .await
            .expect("stage routes succeeds");
        upgraded
            .synchronize()
            .await
            .expect("the ingress synchronizes");
        assert!(upgraded.status().await.healthy);
        assert_ne!(upgraded.container_id().await, original);
        assert_eq!(self.body("one.example.test").await, "first backend");
        self.gateway
            .external()
            .remove_network(&network)
            .await
            .expect("remove network succeeds");
        self.gateway
            .synchronize()
            .await
            .expect("the ingress synchronizes");
    }
}

#[tokio::test]
async fn configuration_validation_runs_without_network_or_ports() {
    let engine = Engine::unused().await;
    let ingress = engine.ingress(true);
    let spec = ingress.container_spec();
    assert_eq!(spec["HostConfig"]["NetworkMode"], json!(ingress.name));
    let validation = Ingress::validation_spec(&spec);
    let host = &validation["HostConfig"];
    assert_eq!(host["NetworkMode"], "none");
    assert_eq!(host["PortBindings"], json!({}));
    assert_eq!(host["RestartPolicy"]["Name"], "no");
    assert!(validation.get("NetworkingConfig").is_none());
    assert!(validation.get("ExposedPorts").is_none());
    assert_eq!(host["CapDrop"], spec["HostConfig"]["CapDrop"]);
    assert_eq!(host["Binds"], spec["HostConfig"]["Binds"]);
}
