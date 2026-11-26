//! Privileged end-to-end qualification for the Docker adapter.

#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "Integration tests and their helpers fail the test by panicking."
)]

#[path = "support/docker_contract.rs"]
mod docker_contract;
#[expect(dead_code, reason = "this binary never calls failing_source")]
#[path = "support/git.rs"]
mod git_fixture;
#[path = "support/poll.rs"]
mod poll;
#[expect(dead_code, reason = "this binary never clones or bundles a repository")]
#[path = "support/repository.rs"]
mod repository;

use bollard::query_parameters::{
    InspectServiceOptions, ListContainersOptionsBuilder, UpdateServiceOptionsBuilder,
};
use docker_contract::Environment;
use git_fixture::GitBuildFixture;
use piqueld::application::RuntimeBoundary;
use piqueld_core::manifest::ValidatedHealthCheck;
use piqueld_core::resource::{
    DesiredNetwork, DesiredService, DesiredVolume, ObservedService, ResolvedSource,
};
use piqueld_core::wait;
use piqueld_core::{ApplicationId, EnvironmentId, InstanceId};
use piqueld_docker::{BollardDocker, DockerApi, DockerError, JobRuns, JobStatus};
use poll::eventually;
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

/// How often scenarios poll the Engine for work it finishes in the background.
const POLL: Duration = Duration::from_millis(250);

/// A `/bin/sh -c` script that runs until its container is stopped, then
/// lingers for two to three seconds (the trap waits for the current
/// `sleep 1`), so a removed task outlives its service. `sh` as PID 1 ignores
/// SIGTERM unless it traps it, and would stop only when Docker kills it after
/// the 10 second grace period.
const LINGERING: &str = "trap 'sleep 2; exit 0' TERM; while true; do sleep 1; done";

struct IsolatedDocker {
    socket: PathBuf,
    docker: BollardDocker,
}

impl IsolatedDocker {
    async fn from_env() -> Self {
        assert_eq!(
            std::env::var("PIQUELD_DOCKER_ISOLATED").as_deref(),
            Ok("1"),
            "refusing to mutate Docker without an explicit isolated-daemon attestation"
        );
        let socket = std::env::var("PIQUELD_DOCKER_SOCKET")
            .expect("PIQUELD_DOCKER_SOCKET must point at an isolated privileged daemon");
        let resolved = std::fs::canonicalize(&socket).unwrap_or_else(|_| PathBuf::from(&socket));
        for forbidden in ["/var/run/docker.sock", "/run/docker.sock"] {
            let forbidden =
                std::fs::canonicalize(forbidden).unwrap_or_else(|_| PathBuf::from(forbidden));
            assert_ne!(
                resolved, forbidden,
                "refusing to mutate the default host Docker socket"
            );
        }
        let docker = BollardDocker::connect(&resolved).expect("the bollard docker connects");
        docker
            .ensure_swarm(true)
            .await
            .expect("ensure swarm succeeds");
        Self {
            socket: resolved,
            docker,
        }
    }

    /// Ensures `desired`, retrying requests the Engine refuses for a while,
    /// like updates racing an earlier one.
    async fn ensure_service_eventually(&self, desired: &DesiredService) {
        let mut refused = None;
        eventually(
            Duration::from_secs(10),
            Duration::from_millis(100),
            async || match self.docker.ensure_service(desired).await {
                Ok(()) => Some(()),
                Err(error @ DockerError::Request(_)) => {
                    refused = Some(error);
                    None
                }
                Err(error) => panic!("failed to ensure Docker service: {error}"),
            },
        )
        .await
        .unwrap_or_else(|| panic!("failed to ensure Docker service: {refused:?}"));
    }

    /// A Bollard client of the same Engine, bypassing the adapter.
    fn raw(&self) -> bollard::Docker {
        bollard::Docker::connect_with_unix(
            self.socket.to_str().expect("the path is utf-8"),
            120,
            bollard::API_DEFAULT_VERSION,
        )
        .expect("connect with unix succeeds")
    }

    /// Applies `request`'s plans until nothing is left to do: each environment
    /// replans after every action, as the controller does. Projects `links` onto
    /// a reconcile request first.
    async fn converge(
        &self,
        request: &piqueld_core::PlanRequest,
        links: &[piqueld_core::links::Link],
    ) {
        use piqueld_core::{ActionKind, Plan, PlanRequest};
        let (id, ownership) = match request {
            PlanRequest::Reconcile { desired } => {
                (desired.id.clone(), desired.networks[0].labels.clone())
            }
            PlanRequest::Delete {
                environment_id,
                instance_id,
            } => (
                environment_id.clone(),
                piqueld_core::resource::Ownership::scope(instance_id, environment_id),
            ),
            PlanRequest::Preview { .. } => panic!("previews are never applied"),
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
        loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{id} did not converge"
            );
            let observed = self.docker.observe(&id).await.expect("observe succeeds");
            let request = match request {
                PlanRequest::Reconcile { desired } => PlanRequest::Reconcile {
                    desired: desired.clone().with_links(links, &observed),
                },
                other => other.clone(),
            };
            let plan = Plan::from_request(&request, &observed);
            assert!(!plan.is_blocked(), "{:?}", plan.diagnostics);
            let Some(action) = plan
                .actions
                .iter()
                .find(|action| !matches!(action.kind, ActionKind::RetainVolume { .. }))
            else {
                return;
            };
            match &action.kind {
                ActionKind::EnsureNetwork { network } => {
                    self.docker
                        .ensure_network(network)
                        .await
                        .expect("ensure network succeeds");
                }
                ActionKind::EnsureService { service } => {
                    self.ensure_service_eventually(service).await;
                }
                ActionKind::RemoveService { name } => {
                    self.docker
                        .remove_service(name, &ownership)
                        .await
                        .expect("remove service succeeds");
                }
                ActionKind::RemoveNetwork { name } => {
                    #[expect(
                        clippy::let_underscore_must_use,
                        reason = "the other side may still be leaving the network"
                    )]
                    let _ = self.docker.remove_network(name, &ownership).await;
                    // wall-clock: Docker lets the network go on its own and
                    // signals nothing; plan again from what it then reports.
                    wait::pause(Duration::from_millis(250)).await;
                }
                // wall-clock: the action waits on Docker, which converges on
                // its own and signals nothing; plan again from what it reports.
                _ => wait::pause(Duration::from_millis(250)).await,
            }
        }
    }

    /// Waits until `service` of `environment` logs `line`, and returns its logs.
    async fn logged(&self, environment: &EnvironmentId, service: &str, line: &str) -> Vec<String> {
        let instance = InstanceId::parse("integration-instance").expect("the instance id parses");
        let mut messages = Vec::new();
        eventually(
            Duration::from_mins(1),
            Duration::from_millis(500),
            async || {
                messages = self
                    .docker
                    .application_logs(&instance, environment, Some(service), 200, 300, None)
                    .await
                    .expect("application logs succeeds")
                    .items
                    .into_iter()
                    .map(|record| record.message)
                    .collect();
                messages
                    .iter()
                    .any(|message| message == line)
                    .then(|| messages.clone())
            },
        )
        .await
        .unwrap_or_else(|| panic!("{service} never logged {line}: {messages:?}"))
    }

    /// Detaches `desired`'s services from the link networks it no longer grants
    /// them, as link enforcement does, and returns those attachments. Nothing
    /// else about the services changes.
    async fn revoke_links(
        &self,
        desired: &piqueld_core::ResolvedApplication,
    ) -> Vec<(String, String)> {
        let observed = self
            .docker
            .observe(&desired.id)
            .await
            .expect("observe succeeds");
        let revoked = observed
            .revoked_links(
                &desired.instance_id,
                &desired.id,
                std::slice::from_ref(desired),
            )
            .into_iter()
            .map(|(service, network)| (service.to_owned(), network.to_owned()))
            .collect::<Vec<_>>();
        for (service, network) in &revoked {
            self.docker
                .detach_network(service, network, &desired.networks[0].labels)
                .await
                .expect("detach network succeeds");
        }
        let observed = self
            .docker
            .observe(&desired.id)
            .await
            .expect("observe succeeds");
        assert!(
            observed.services.iter().all(|service| desired
                .services
                .iter()
                .any(|wanted| service.matches(wanted))),
            "only the link attachments changed"
        );
        revoked
    }
}

struct SwarmScenario {
    engine: IsolatedDocker,
    environment: Environment,
    network: DesiredNetwork,
    volume: DesiredVolume,
    service: DesiredService,
}

impl SwarmScenario {
    async fn new() -> Self {
        let engine = IsolatedDocker::from_env().await;
        let environment = Environment::new(&engine.docker).await;
        let app = &environment.app;
        let labels = &environment.labels;
        let network = environment.network();
        let volume = environment.volume();
        engine
            .docker
            .ensure_network(&network)
            .await
            .expect("ensure network succeeds");
        let observed = engine.docker.observe(app).await.expect("observe succeeds");
        assert!(
            observed.networks[0].runtime_configuration_matches,
            "a network created by piqueld must match Docker's inspected representation"
        );
        engine
            .docker
            .ensure_volume(&volume)
            .await
            .expect("ensure volume succeeds");
        let secret_name = environment.secret_name();
        engine
            .docker
            .ensure_secret(&secret_name, b"mounted-value", labels)
            .await
            .expect("ensure secret succeeds");
        let service = DesiredService {
            secrets: vec![piqueld_core::resource::SecretFile { secret_name: secret_name.clone(), target: "/run/secrets/token".into() }],
            files: vec![Self::motd(app)],
            arguments: vec![
                "-c".into(),
                "trap 'exit 0' TERM; test $(cat /run/secrets/token) = mounted-value || exit 1; test \"$(cat /etc/piqueld-motd)\" = 'hello ${HOME}' || exit 1; if echo changed 2>/dev/null >/etc/piqueld-motd; then exit 1; fi; until getent hosts web >/dev/null; do sleep 1; done; echo log-stdout; echo log-stderr >&2; while true; do sleep 1; done".into(),
            ],
            ..environment.service("web")
        };
        engine.ensure_service_eventually(&service).await;
        assert!(
            engine
                .docker
                .remove_secrets(std::slice::from_ref(&secret_name), labels)
                .await
                .is_err(),
            "Docker refuses removal while a service uses the secret"
        );

        Self {
            engine,
            environment,
            network,
            volume,
            service,
        }
    }

    /// A file whose content looks like shell and template syntax, which the
    /// web task expects verbatim at `/etc/piqueld-motd`.
    fn motd(app: &EnvironmentId) -> piqueld_core::DesiredFile {
        piqueld_core::DesiredFile {
            config_name: piqueld_core::DockerConfigName::for_file(
                app,
                "hello ${HOME}",
                piqueld_core::manifest::FileMode::default(),
            ),
            target: "/etc/piqueld-motd".into(),
            mode: piqueld_core::manifest::FileMode::default(),
            content: "hello ${HOME}".into(),
        }
    }

    /// How Docker runs `service` now, if it exists.
    async fn observed_service(&self, service: &DesiredService) -> Option<ObservedService> {
        self.engine
            .docker
            .observe(&self.environment.app)
            .await
            .expect("observe succeeds")
            .services
            .into_iter()
            .find(|observed| observed.name == service.name.as_str())
    }

    /// The web service, renamed to `name`, for changes to start beside it.
    fn derived_service(&self, name: &str) -> DesiredService {
        let named = self.environment.service(name);
        DesiredService {
            logical_name: named.logical_name,
            name: named.name,
            labels: named.labels,
            ..self.service.clone()
        }
    }

    /// The web task checked its file's content and that it cannot write it
    /// before logging; pruning keeps the config while a service mounts it,
    /// and removes the rest, even when their listing is far larger than
    /// other Engine responses.
    async fn assert_files(&self) {
        use base64::Engine as _;
        use piqueld_core::manifest::ServiceFile;
        let mut labels = self.environment.labels.clone();
        labels.remove("io.piqueld.spec-hash");
        labels.insert("io.piqueld.file".into(), "true".into());
        for generation in 0..12 {
            let content = format!("{generation:x}").repeat(ServiceFile::MAX_BYTES);
            self.docker_api_with(
                "POST",
                "/configs/create",
                Some(serde_json::json!({
                    "Name": format!("piqueld-{}-file-unused-{generation}", self.environment.app),
                    "Labels": labels,
                    "Data": base64::engine::general_purpose::STANDARD
                        .encode(&content[..ServiceFile::MAX_BYTES]),
                })),
            )
            .await;
        }
        self.engine
            .docker
            .prune_files(&self.environment.labels)
            .await
            .expect("prune files succeeds");
        let remaining = self
            .docker_api("GET", "/configs")
            .await
            .as_array()
            .expect("the value is an array")
            .iter()
            .filter(|config| {
                config["Spec"]["Labels"]["io.piqueld.file"] == "true"
                    && config["Spec"]["Labels"]["io.piqueld.application"]
                        == self.environment.app.as_str()
            })
            .map(|config| {
                config["Spec"]["Name"]
                    .as_str()
                    .expect("the value is a string")
                    .to_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(remaining, [self.service.files[0].config_name.to_string()]);
        assert_eq!(
            self.engine
                .docker
                .observe(&self.environment.app)
                .await
                .expect("observe succeeds")
                .services[0]
                .files,
            self.service
                .files
                .iter()
                .map(piqueld_core::resource::ObservedFile::from)
                .collect::<Vec<_>>(),
            "the mounted file's config survives pruning"
        );
    }

    /// The container logs only after resolving its logical name, so this also
    /// proves the private-network alias.
    async fn assert_logs(&self) {
        let instance = self.environment.instance();
        let logs = eventually(Duration::from_secs(30), POLL, async || {
            let logs = self
                .engine
                .docker
                .application_logs(&instance, &self.environment.app, Some("web"), 20, 60, None)
                .await
                .expect("application logs succeeds");
            logs.items
                .iter()
                .any(|line| line.message == "log-stderr")
                .then_some(logs)
        })
        .await
        .expect("container output did not appear");
        assert!(
            logs.items
                .iter()
                .any(|line| line.message == "log-stdout" && line.stream == "stdout")
        );
        assert!(logs.items.iter().all(|line| line.service == "web"
            && !line.task_id.is_empty()
            && !line.timestamp.is_empty()));
        for stream in [
            piqueld_core::api::LogStream::Stdout,
            piqueld_core::api::LogStream::Stderr,
        ] {
            let logs = self
                .engine
                .docker
                .application_logs(
                    &instance,
                    &self.environment.app,
                    Some("web"),
                    1,
                    60,
                    Some(stream),
                )
                .await
                .expect("application logs succeeds");
            assert_eq!(logs.items.len(), 1);
            assert_eq!(logs.items[0].stream, stream.as_str());
        }
        assert!(
            self.engine
                .docker
                .application_logs(
                    &InstanceId::parse("another-instance").expect("the instance id parses"),
                    &self.environment.app,
                    None,
                    20,
                    60,
                    None
                )
                .await
                .expect("application logs succeeds")
                .items
                .is_empty()
        );
    }

    /// Creates `script` as a `/bin/sh -c` command in the running `web` task.
    async fn create_exec(
        &self,
        script: &str,
        tty: Option<piqueld_core::exec::TerminalSize>,
    ) -> piqueld_docker::Exec {
        use piqueld_core::exec::{ExecCommand, ExecRequest};
        let request = ExecRequest {
            service: piqueld_core::ServiceName::parse("web").expect("the service name parses"),
            command: ExecCommand::parse(vec!["/bin/sh".into(), "-c".into(), script.into()])
                .expect("the exec command parses"),
            stdin: true,
            tty,
        };
        self.engine
            .docker
            .create_exec(
                &self.environment.instance(),
                &self.environment.app,
                &request,
            )
            .await
            .expect("create exec succeeds")
            .expect("web has a running task")
    }

    /// Runs `script` with `stdin` and returns its stdout, stderr and exit code.
    async fn exec(
        &self,
        script: &str,
        stdin: &[u8],
        tty: Option<piqueld_core::exec::TerminalSize>,
    ) -> (Vec<u8>, Vec<u8>, i64) {
        use piqueld_core::exec::{ExecInput, ExecOutput};
        let exec = self.create_exec(script, tty).await;
        let (input, input_rx) = tokio::sync::mpsc::channel(4);
        let (output_tx, mut output) = tokio::sync::mpsc::channel(16);
        input
            .send(ExecInput::Stdin(stdin.to_vec()))
            .await
            .expect("the request is sent");
        input
            .send(ExecInput::CloseStdin)
            .await
            .expect("the request is sent");
        let io = piqueld_docker::ExecIo {
            input: input_rx,
            output: output_tx,
            stop: tokio_util::sync::CancellationToken::new(),
        };
        let drain = async {
            let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
            while let Some(frame) = output.recv().await {
                match frame {
                    ExecOutput::Stdout(data) => stdout.extend(data),
                    ExecOutput::Stderr(data) => stderr.extend(data),
                    frame => panic!("unexpected exec output {frame:?}"),
                }
            }
            (stdout, stderr)
        };
        let (code, (stdout, stderr)) = tokio::time::timeout(
            Duration::from_secs(60),
            futures_util::future::join(self.engine.docker.run_exec(&exec, io), drain),
        )
        .await
        .expect("exec finishes");
        (stdout, stderr, code.expect("the code is present"))
    }

    async fn assert_exec(&self) {
        let (stdout, stderr, code) = self.exec("cat; echo failed >&2; exit 3", b"in", None).await;
        assert_eq!(
            (stdout.as_slice(), stderr.as_slice(), code),
            (&b"in"[..], &b"failed\n"[..], 3)
        );
        let size = piqueld_core::exec::TerminalSize {
            width: 91,
            height: 17,
        };
        let (stdout, stderr, code) = self.exec("stty size", b"", Some(size)).await;
        assert_eq!(
            String::from_utf8(stdout)
                .expect("the bytes are utf-8")
                .trim(),
            "17 91"
        );
        assert_eq!(stderr, b"");
        assert_eq!(code, 0);
        let (stdout, _, code) = self
            .exec("head -c 8388608 /dev/zero", &vec![0; 8 << 20], None)
            .await;
        assert_eq!((stdout.len(), code), (8 << 20, 0));
        let exec = self.create_exec("sleep 300", None).await;
        let (input, input_rx) = tokio::sync::mpsc::channel(1);
        let (output, _output_rx) = tokio::sync::mpsc::channel(1);
        input
            .send(piqueld_core::exec::ExecInput::Stdin(vec![0; 8 << 20]))
            .await
            .expect("the request is sent");
        let stop = tokio_util::sync::CancellationToken::new();
        let io = piqueld_docker::ExecIo {
            input: input_rx,
            output,
            stop: stop.clone(),
        };
        let disconnect = async {
            // wall-clock: the write blocks inside Docker's exec stream, which
            // signals nothing; give it time to fill the pipe.
            wait::pause(Duration::from_secs(2)).await;
            stop.cancel();
        };
        let (stopped, ()) = tokio::time::timeout(
            Duration::from_secs(30),
            futures_util::future::join(self.engine.docker.run_exec(&exec, io), disconnect),
        )
        .await
        .expect("a stop ends the session");
        stopped.expect_err("`stopped` should be rejected");
    }

    async fn add_http_service(&self) -> DesiredService {
        let mut http_service = self.derived_service("http");
        let reference = "traefik/whoami:v1.11.0";
        let image = self
            .engine
            .docker
            .resolve_image(reference)
            .await
            .expect("resolve image succeeds");
        http_service.source =
            ResolvedSource::parse_image(reference, image).expect("the image source parses");
        http_service.command = Vec::new();
        http_service.arguments = vec!["--port".into(), "8080".into()];
        http_service.healthcheck = Some(ValidatedHealthCheck::Http {
            port: 8080,
            path: "/health".into(),
            interval_seconds: 1,
            timeout_seconds: 1,
            start_period_seconds: 0,
            retries: 3,
        });
        self.engine.ensure_service_eventually(&http_service).await;

        http_service
    }

    #[expect(
        clippy::too_many_lines,
        reason = "A long end-to-end scenario; rustfmt wraps its expect messages."
    )]
    async fn assert_healthchecks(&self, http_service: &DesiredService) {
        let web = self
            .observed_service(&self.service)
            .await
            .expect("observed service succeeds");
        assert_eq!(web.secrets, self.service.secrets);
        assert_eq!(
            web.healthcheck, self.service.healthcheck,
            "command health check survives complete service inspection"
        );
        assert_eq!(
            self.observed_service(http_service)
                .await
                .expect("observed service succeeds")
                .healthcheck,
            http_service.healthcheck,
            "HTTP health check survives complete service inspection"
        );

        for service in [&self.service, http_service] {
            let mut healthy = None;
            let observed = eventually(Duration::from_secs(30), POLL, async || {
                let observed = self
                    .observed_service(service)
                    .await
                    .expect("observed service succeeds");
                healthy = observed
                    .tasks
                    .iter()
                    .find(|task| task.desired_running)
                    .map(|task| task.healthy);
                (healthy == Some(Some(true))).then_some(observed)
            })
            .await
            .unwrap_or_else(|| {
                panic!(
                    "{} never reported a healthy task: {healthy:?}",
                    service.logical_name
                )
            });
            assert_eq!(observed.drift(service), [] as [&str; 0]);
        }

        let spec = self
            .docker_api("GET", &format!("/services/{}", http_service.name))
            .await;
        let configs = &spec["Spec"]["TaskTemplate"]["ContainerSpec"]["Configs"];
        let probe = configs
            .as_array()
            .expect("the value is an array")
            .iter()
            .find(|config| config["File"]["Name"] == "/run/piqueld/probe")
            .unwrap_or_else(|| panic!("no probe config in {configs}"));
        let current = probe["ConfigName"]
            .as_str()
            .expect("the value is a string")
            .to_owned();
        let instance = &http_service.labels[piqueld_core::resource::INSTANCE_LABEL];
        for (name, owner) in [
            ("piqueld-health-probe-stale", None),
            ("piqueld-health-probe-own-stale", Some(instance.as_str())),
            ("piqueld-health-probe-other", Some("instance-other")),
        ] {
            let mut labels = serde_json::json!({"io.piqueld.health-probe": "true"});
            if let Some(owner) = owner {
                labels[piqueld_core::resource::INSTANCE_LABEL] = owner.into();
            }
            self.docker_api_with(
                "POST",
                "/configs/create",
                Some(serde_json::json!({"Name": name, "Labels": labels, "Data": "c3RhbGU="})),
            )
            .await;
        }
        let instance = InstanceId::parse(instance.clone()).expect("the instance id parses");
        self.engine
            .docker
            .prune_health_probes(&instance)
            .await
            .expect("prune health probes succeeds");
        let names = self
            .docker_api("GET", "/configs")
            .await
            .as_array()
            .expect("the value is an array")
            .iter()
            .map(|config| {
                config["Spec"]["Name"]
                    .as_str()
                    .expect("the value is a string")
                    .to_owned()
            })
            .collect::<Vec<_>>();
        assert!(names.contains(&current), "{names:?}");
        assert!(
            names
                .iter()
                .any(|name| name == "piqueld-health-probe-other"),
            "{names:?}"
        );
        assert!(
            !names.iter().any(|name| name.ends_with("-stale")),
            "{names:?}"
        );
    }

    async fn scale_and_reconnect(&mut self) {
        self.service.replicas = 2;
        self.engine.ensure_service_eventually(&self.service).await;
        self.engine.docker =
            BollardDocker::connect(&self.engine.socket).expect("the bollard docker connects");
        let scaled = eventually(Duration::from_mins(1), POLL, async || {
            self.observed_service(&self.service)
                .await
                .filter(|observed| observed.replicas == 2)
        })
        .await
        .expect("the service scales to two replicas");
        assert_eq!(scaled.healthcheck, self.service.healthcheck);
    }

    async fn assert_idempotence_and_repair_drift(&self) {
        let raw = self.engine.raw();
        let inspect = async || {
            raw.inspect_service(self.service.name.as_str(), None::<InspectServiceOptions>)
                .await
                .expect("inspect service succeeds")
        };
        let version = |service: &bollard::models::Service| {
            service.version.as_ref().and_then(|version| version.index)
        };
        let constraints = |service: &bollard::models::Service| {
            service
                .spec
                .as_ref()
                .and_then(|spec| spec.task_template.as_ref())
                .and_then(|task| task.placement.as_ref())
                .and_then(|placement| placement.constraints.clone())
        };
        let matching = inspect().await;
        let node_id = raw
            .info()
            .await
            .expect("info succeeds")
            .swarm
            .expect("the swarm is present")
            .node_id
            .expect("the node id is present");
        let expected_constraint = Some(vec![format!("node.id == {node_id}")]);
        assert_eq!(constraints(&matching), expected_constraint);
        let matching_version = version(&matching);
        self.engine.ensure_service_eventually(&self.service).await;
        assert_eq!(
            version(&inspect().await),
            matching_version,
            "a matching reconcile must not update the Docker service"
        );

        let mut drifted_spec = matching.spec.expect("the spec is present");
        drifted_spec
            .task_template
            .as_mut()
            .expect("as mut succeeds")
            .placement = None;
        drifted_spec
            .mode
            .as_mut()
            .expect("as mut succeeds")
            .replicated
            .as_mut()
            .expect("as mut succeeds")
            .replicas = Some(1);
        raw.update_service(
            self.service.name.as_str(),
            drifted_spec,
            UpdateServiceOptionsBuilder::default()
                .version(
                    i32::try_from(matching_version.expect("the matching version is present"))
                        .expect("the i32 converts"),
                )
                .build(),
            None,
        )
        .await
        .expect("update service succeeds");
        self.engine
            .docker
            .ensure_service(&self.service)
            .await
            .expect("ensure service succeeds");
        let repaired = inspect().await;
        assert!(
            version(&repaired)
                .is_some_and(|version| matching_version.is_some_and(|previous| version > previous))
        );
        assert_eq!(constraints(&repaired), expected_constraint);
        assert_eq!(
            repaired
                .spec
                .and_then(|spec| spec.mode)
                .and_then(|mode| mode.replicated)
                .and_then(|replicated| replicated.replicas),
            Some(i64::from(self.service.replicas))
        );
    }

    /// The `migrate` job derived from the service, running `script`.
    fn job(&self, operation: &str, script: &str) -> piqueld_core::DesiredJobRun {
        self.job_with(
            operation,
            script,
            piqueld_core::manifest::JobVolumeAccess::ReadOnly,
        )
    }

    /// The `migrate` job with `volume_access` to the scenario's volume, mounted
    /// writable at `/data`.
    fn job_with(
        &self,
        operation: &str,
        script: &str,
        volume_access: piqueld_core::manifest::JobVolumeAccess,
    ) -> piqueld_core::DesiredJobRun {
        self.environment
            .job(&self.service, operation, script, volume_access, false)
    }

    /// Starts a service named `name`, mounting `mounts`, that runs
    /// [`LINGERING`], so a removed task stops a while after its service.
    /// Returns once its container runs, since Swarm drops a removed task that
    /// has none yet at once.
    async fn start_lingering_service(
        &self,
        name: &str,
        mounts: Vec<piqueld_core::DesiredMount>,
    ) -> DesiredService {
        let mut service = self.derived_service(name);
        service.command = vec!["/bin/sh".into(), "-c".into(), LINGERING.into()];
        service.arguments.clear();
        service.healthcheck = None;
        service.secrets.clear();
        service.mounts = mounts;
        self.engine.ensure_service_eventually(&service).await;
        let raw = self.engine.raw();
        let filters = std::collections::HashMap::from([(
            "label",
            vec![format!(
                "com.docker.swarm.service.name={}",
                service.name.as_str()
            )],
        )]);
        eventually(Duration::from_mins(1), POLL, async || {
            let options = ListContainersOptionsBuilder::default()
                .filters(&filters)
                .build();
            let running = raw
                .list_containers(Some(options))
                .await
                .expect("list containers succeeds");
            (!running.is_empty()).then_some(())
        })
        .await
        .expect("the lingering service's container starts");
        service
    }

    /// Runs one-shot jobs derived from the service: they read its secret,
    /// never answer for its alias, ignore image health checks, report exit
    /// codes and output, and are replaced or removed by operation.
    async fn run_jobs(&self) {
        let docker = &self.engine.docker;
        let first = self.job(
            "operation-1",
            "test \"$(cat /run/secrets/token)\" = mounted-value && echo out && echo err >&2",
        );
        assert_eq!(
            docker
                .job_status(&first)
                .await
                .expect("job status succeeds"),
            JobStatus::Missing
        );
        docker.start_job(&first).await.expect("start job succeeds");
        let spec = self
            .docker_api("GET", &format!("/services/{}", first.container.name))
            .await["Spec"]
            .take();
        assert!(spec["Mode"]["ReplicatedJob"].is_object(), "{spec}");
        let task = &spec["TaskTemplate"];
        assert_eq!(
            task["ContainerSpec"]["Healthcheck"]["Test"],
            serde_json::json!(["NONE"])
        );
        assert_eq!(task["RestartPolicy"]["Condition"], "none");
        let networks = task["Networks"].as_array().expect("the value is an array");
        assert!(
            !networks.is_empty()
                && networks
                    .iter()
                    .all(|network| network["Aliases"].as_array().is_none_or(Vec::is_empty)),
            "a job must not answer for the service's alias: {spec}"
        );
        assert_eq!(
            Self::wait_job(docker, &first).await,
            JobStatus::Finished {
                exit_code: Some(0),
                error: None
            }
        );
        let mut output = docker
            .job_output(&first)
            .await
            .expect("job output succeeds");
        assert!(!output.truncated);
        output
            .chunks
            .sort_by_key(|(stream, _)| *stream == piqueld_core::api::LogStream::Stderr);
        assert_eq!(
            output.chunks,
            [
                (piqueld_core::api::LogStream::Stdout, b"out\n".to_vec()),
                (piqueld_core::api::LogStream::Stderr, b"err\n".to_vec()),
            ]
        );

        let second = self.job("operation-2", "exit 3");
        assert_eq!(
            docker
                .job_status(&second)
                .await
                .expect("job status succeeds"),
            JobStatus::Missing
        );
        docker.start_job(&second).await.expect("start job succeeds");
        assert_eq!(
            docker
                .job_status(&first)
                .await
                .expect("job status succeeds"),
            JobStatus::Missing
        );
        let JobStatus::Finished { exit_code, error } = Self::wait_job(docker, &second).await else {
            panic!("the failing job must finish");
        };
        assert_eq!(exit_code, Some(3));
        assert!(error.is_some(), "Docker explains a failed task");
        docker
            .remove_jobs(&self.environment.labels, JobRuns::Except("operation-2"))
            .await
            .expect("remove jobs succeeds");
        assert_ne!(
            docker
                .job_status(&second)
                .await
                .expect("job status succeeds"),
            JobStatus::Missing
        );
        docker
            .remove_jobs(&self.environment.labels, JobRuns::All)
            .await
            .expect("remove jobs succeeds");
        assert_eq!(
            docker
                .job_status(&second)
                .await
                .expect("job status succeeds"),
            JobStatus::Missing
        );
    }

    /// An exclusive run and a service never share a volume: the run does not
    /// start while a service mounts it, starts once the removed service's
    /// task stopped, and blocks the service until the run is removed. Removed
    /// tasks run [`LINGERING`] a while longer, which starting the run doesn't
    /// wait for: callers wait first.
    async fn exclusive_jobs_hold_their_volume(&self) {
        let docker = &self.engine.docker;
        let writer = self
            .start_lingering_service("writer", vec![self.environment.data_mount()])
            .await;

        let job = self.job_with(
            "operation-exclusive",
            LINGERING,
            piqueld_core::manifest::JobVolumeAccess::Exclusive,
        );
        assert!(matches!(
            docker.start_job(&job).await,
            Err(DockerError::Request(_))
        ));
        docker
            .remove_service(writer.name.as_str(), &self.environment.labels)
            .await
            .expect("remove service succeeds");
        assert!(matches!(
            docker.start_job(&job).await,
            Err(DockerError::Request(_))
        ));
        docker
            .wait_services_stopped(&job)
            .await
            .expect("wait services stopped succeeds");
        docker.start_job(&job).await.expect("start job succeeds");
        assert!(matches!(
            docker.ensure_service(&writer).await,
            Err(DockerError::Request(_))
        ));
        docker
            .remove_jobs(&self.environment.labels, JobRuns::All)
            .await
            .expect("remove jobs succeeds");
        eventually(Duration::from_mins(1), POLL, async || {
            docker.ensure_service(&writer).await.ok()
        })
        .await
        .expect("the service starts once the run stopped");
        docker
            .remove_service(writer.name.as_str(), &self.environment.labels)
            .await
            .expect("remove service succeeds");
    }

    /// A run that stops its own service does not start while the service
    /// exists, nor until its removed task stopped.
    async fn jobs_wait_for_their_stopped_service(&self) {
        let docker = &self.engine.docker;
        let auth = self.start_lingering_service("auth", Vec::new()).await;
        let job = self.environment.job(
            &auth,
            "operation-stop-service",
            "echo migrated",
            piqueld_core::manifest::JobVolumeAccess::ReadOnly,
            true,
        );
        assert_eq!(job.stopped_service().as_ref(), Some(&auth.name));
        assert!(matches!(
            docker.start_job(&job).await,
            Err(DockerError::Request(_))
        ));
        docker
            .remove_service(auth.name.as_str(), &self.environment.labels)
            .await
            .expect("remove service succeeds");
        assert!(matches!(
            docker.start_job(&job).await,
            Err(DockerError::Request(_))
        ));
        docker
            .wait_services_stopped(&job)
            .await
            .expect("wait services stopped succeeds");
        docker.start_job(&job).await.expect("start job succeeds");
        assert_eq!(
            Self::wait_job(docker, &job).await,
            JobStatus::Finished {
                exit_code: Some(0),
                error: None
            }
        );
        docker
            .remove_jobs(&self.environment.labels, JobRuns::All)
            .await
            .expect("remove jobs succeeds");
    }

    /// Waiting for removed job runs covers a run's container until it
    /// stopped, even after an earlier attempt deleted its service. The run's
    /// [`LINGERING`] container outlives its service.
    async fn job_runs_are_waited_for_until_their_containers_stop(&self) {
        let docker = &self.engine.docker;
        let third = self.job("operation-3", LINGERING);
        docker.start_job(&third).await.expect("start job succeeds");
        let running = || async {
            self.docker_api(
                "GET",
                "/containers/json?filters=%7B%22label%22%3A%5B%22io.piqueld.job-operation%3Doperation-3%22%5D%7D",
            )
            .await
            .as_array()
            .expect("the value is an array")
            .len()
        };
        eventually(Duration::from_mins(1), POLL, async || {
            (running().await > 0).then_some(())
        })
        .await
        .expect("job container starts");
        self.docker_api("DELETE", &format!("/services/{}", third.container.name))
            .await;
        docker
            .remove_jobs(&self.environment.labels, JobRuns::All)
            .await
            .expect("remove jobs succeeds");
        docker
            .wait_jobs_stopped(&self.environment.labels, JobRuns::All)
            .await
            .expect("wait jobs stopped succeeds");
        assert_eq!(running().await, 0);
    }

    /// Sends one request to Docker's HTTP API and returns its JSON body.
    async fn docker_api(&self, method: &str, path: &str) -> serde_json::Value {
        self.docker_api_with(method, path, None).await
    }

    /// Sends one raw Engine request with an optional JSON `body` and expects
    /// a 2xx response.
    async fn docker_api_with(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> serde_json::Value {
        use std::io::{Read, Write};
        let socket = self.engine.socket.clone();
        let body = body.map_or_default(|body| body.to_string());
        let request = format!(
            "{method} {path} HTTP/1.0\r\nHost: docker\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let response = tokio::task::spawn_blocking(move || {
            let mut stream =
                std::os::unix::net::UnixStream::connect(socket).expect("the unix stream connects");
            stream
                .write_all(request.as_bytes())
                .expect("write all succeeds");
            let mut response = String::new();
            stream
                .read_to_string(&mut response)
                .expect("read to string succeeds");
            response
        })
        .await
        .expect("spawn blocking succeeds");
        let (head, body) = response
            .split_once("\r\n\r\n")
            .expect("the separator is present");
        assert!(
            [" 200 ", " 201 ", " 204 "]
                .iter()
                .any(|status| head.contains(status)),
            "{head}"
        );
        if body.is_empty() {
            return serde_json::Value::Null;
        }
        serde_json::from_str(body).expect("the json parses")
    }

    /// Polls a job run until it stops.
    async fn wait_job(docker: &BollardDocker, job: &piqueld_core::DesiredJobRun) -> JobStatus {
        eventually(Duration::from_mins(1), POLL, async || {
            match docker.job_status(job).await.expect("job status succeeds") {
                JobStatus::Running { .. } => None,
                status => Some(status),
            }
        })
        .await
        .expect("job finishes")
    }

    async fn delete_retaining_volume(&self, http_service: &DesiredService) {
        self.engine
            .docker
            .remove_service(http_service.name.as_str(), &self.environment.labels)
            .await
            .expect("remove service succeeds");
        self.engine
            .docker
            .remove_service(self.service.name.as_str(), &self.environment.labels)
            .await
            .expect("remove service succeeds");
        let removed = [self.service.name.as_str(), http_service.name.as_str()];
        eventually(Duration::from_mins(1), POLL, async || {
            let observed = self
                .engine
                .docker
                .observe(&self.environment.app)
                .await
                .expect("observe succeeds");
            (!observed
                .services
                .iter()
                .any(|service| removed.contains(&service.name.as_str())))
            .then_some(())
        })
        .await
        .expect("services are removed");
        let mut in_use = None;
        eventually(Duration::from_mins(1), POLL, async || {
            match self
                .engine
                .docker
                .remove_network(self.network.name.as_str(), &self.environment.labels)
                .await
            {
                Ok(()) => Some(()),
                Err(error) if error.releases_network() => {
                    in_use = Some(error);
                    None
                }
                Err(error) => panic!("network removal failed: {error:?}"),
            }
        })
        .await
        .unwrap_or_else(|| panic!("network task cleanup timed out: {in_use:?}"));
        eventually(Duration::from_mins(1), POLL, async || {
            let observed = self
                .engine
                .docker
                .observe(&self.environment.app)
                .await
                .expect("observe succeeds");
            (!observed
                .networks
                .iter()
                .any(|network| network.name == self.network.name.as_str()))
            .then_some(())
        })
        .await
        .expect("the network is removed");
        let retained = self
            .engine
            .docker
            .observe(&self.environment.app)
            .await
            .expect("observe succeeds");
        assert!(
            retained
                .volumes
                .iter()
                .any(|v| v.name == self.volume.name.as_str())
        );
        let raw = self.engine.raw();
        let secret_name = &self.service.secrets[0].secret_name;
        self.engine
            .docker
            .remove_secrets(std::slice::from_ref(secret_name), &self.environment.labels)
            .await
            .expect("remove secrets succeeds");
        assert!(matches!(
            raw.inspect_secret(secret_name).await,
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404,
                ..
            })
        ));
        raw.remove_volume(
            self.volume.name.as_str(),
            None::<bollard::query_parameters::RemoveVolumeOptions>,
        )
        .await
        .expect("remove volume succeeds");
    }
}

/// Privileged lifecycle qualification; ordinary validation never mutates Docker.
#[tokio::test]
#[ignore = "requires an isolated privileged Docker Engine"]
async fn swarm_init_create_replica_drift_restart_jobs_delete_and_volume_retention() {
    let mut scenario = SwarmScenario::new().await;
    scenario.assert_logs().await;
    scenario.assert_files().await;
    scenario.assert_exec().await;
    let http_service = scenario.add_http_service().await;
    scenario.assert_healthchecks(&http_service).await;
    scenario.scale_and_reconnect().await;
    scenario.assert_idempotence_and_repair_drift().await;
    scenario.run_jobs().await;
    scenario
        .job_runs_are_waited_for_until_their_containers_stop()
        .await;
    scenario.exclusive_jobs_hold_their_volume().await;
    scenario.jobs_wait_for_their_stopped_service().await;
    scenario.delete_retaining_volume(&http_service).await;
}

#[tokio::test]
#[ignore = "requires an isolated privileged Docker Engine"]
#[expect(
    clippy::too_many_lines,
    reason = "A long end-to-end scenario; rustfmt wraps its expect messages."
)]
async fn git_build_runs_as_a_local_swarm_image() {
    let engine = IsolatedDocker::from_env().await;
    let docker = &engine.docker;
    let fixture = GitBuildFixture::new();
    let source = fixture.source.clone();
    let manifest = serde_json::json!({"api_version":"piqueld.dev/v1alpha1", "kind":"Application", "metadata":{"name":"git-local"}, "spec":{"services":[{"name":"web", "source":source}]}});
    let app = piqueld_core::parse_json(&manifest.to_string())
        .expect("parse json succeeds")
        .normalize(ApplicationId::parse("git-local-build").expect("the application id parses"));
    let environment =
        EnvironmentId::parse("env-git-local-build").expect("the environment id parses");
    let state = tempfile::tempdir().expect("the temporary directory is created");
    let store = piqueld_store::Store::open_unsynced(state.path().join("state.db"))
        .await
        .expect("open unsynced succeeds");
    let sources = piqueld::git::Sources::open(state.path())
        .await
        .expect("the sources open");
    let owner = store.instance_id().clone();
    let runtime = piqueld::application::ApplicationRuntime::new(
        std::sync::Arc::new(docker.clone()),
        &store,
        &sources,
        std::sync::Arc::new(tokio::sync::Notify::new()),
        Duration::from_mins(2),
        std::sync::Arc::default(),
        std::sync::Arc::default(),
    );
    let target = runtime
        .prepare(&environment, &app, &piqueld_core::ResolutionSet::default())
        .await
        .expect("the change prepares");
    docker
        .ensure_network(&target.networks[0])
        .await
        .expect("ensure network succeeds");
    let observed = docker
        .observe(&environment)
        .await
        .expect("observe succeeds");
    assert!(observed.networks[0].runtime_configuration_matches);
    docker
        .ensure_network(&target.networks[0])
        .await
        .expect("ensure network succeeds");
    engine.ensure_service_eventually(&target.services[0]).await;
    eventually(Duration::from_mins(1), POLL, async || {
        let observed = docker
            .observe(&environment)
            .await
            .expect("observe succeeds");
        observed
            .services
            .iter()
            .any(|service| service.convergence == piqueld_core::Convergence::Converged)
            .then_some(())
    })
    .await
    .expect("local Git-built image must converge in Swarm");
    docker
        .remove_service(target.services[0].name.as_str(), &target.services[0].labels)
        .await
        .expect("remove service succeeds");

    let id = piqueld_core::Sha256Digest::parse(target.services[0].image().as_str())
        .expect("the sha256 digest parses");
    let built = || async {
        docker
            .images()
            .await
            .expect("images succeeds")
            .into_iter()
            .find(|image| image.id == id)
    };
    let other = InstanceId::parse("another-installation").expect("the instance id parses");
    let image = built().await.expect("the build is listed by its ID");
    assert!(image.built_by(&owner) && !image.built_by(&other));
    let tags = std::process::Command::new("docker")
        .arg("--host")
        .arg(format!("unix://{}", engine.socket.display()))
        .args(["image", "inspect", "--format", "{{json .RepoTags}}"])
        .arg(id.as_str())
        .output()
        .expect("output succeeds");
    assert_eq!(
        String::from_utf8(tags.stdout)
            .expect("the bytes are utf-8")
            .trim(),
        format!(
            r#"["piqueld-build/{owner}:{}"]"#,
            id.as_str().trim_start_matches("sha256:")
        )
    );
    assert!(matches!(
        docker.remove_image(&other, &id).await,
        Err(DockerError::OwnershipConflict)
    ));
    eventually(Duration::from_mins(1), POLL, async || {
        (!built().await.is_some_and(|image| image.used)).then_some(())
    })
    .await
    .expect("the removed service's containers go");
    docker
        .remove_image(&owner, &id)
        .await
        .expect("remove image succeeds");
    assert!(built().await.is_none());
}

/// Compiles the application `name` with image `services`, resolved through
/// `images` (reference to digest reference). Docker watches each service's
/// new containers for one second instead of the default 30, so converging an
/// update that replaces one does not wait that long.
fn compile_images(
    name: &str,
    mut services: serde_json::Value,
    images: &BTreeMap<&str, String>,
) -> piqueld_core::ResolvedApplication {
    for service in services.as_array_mut().expect("as array mut succeeds") {
        service["rollout"] = serde_json::json!({"monitor_seconds": 1});
    }
    let manifest = serde_json::json!({"api_version": "piqueld.dev/v1alpha1", "kind": "Application", "metadata": {"name": name}, "spec": {"services": services}});
    let id = ApplicationId::parse(format!("app-{name}")).expect("the application id parses");
    let app = piqueld_core::parse_json(&manifest.to_string())
        .expect("parse json succeeds")
        .normalize(id.clone());
    let sources = app
        .spec()
        .services
        .iter()
        .map(|service| {
            let piqueld_core::manifest::ValidatedSource::Image { image } = &service.source else {
                panic!("image sources only")
            };
            let digest = images[image.as_str()].clone();
            (
                service.name.clone(),
                ResolvedSource::parse_image(image.as_str(), digest)
                    .expect("the image source parses"),
            )
        })
        .collect();
    piqueld_core::compile_application(
        &app,
        &EnvironmentId::parse(format!("env-{name}")).expect("the environment id parses"),
        InstanceId::parse("integration-instance").expect("the instance id parses"),
        &piqueld_core::ResolutionSet {
            sources,
            secret_names: BTreeMap::new(),
        },
    )
    .expect("compile application succeeds")
}

/// An importer reaches the exported service by its link name and nothing else
/// of the exporter; an unlinked application resolves neither; removing the
/// link detaches the exporter and removes the network.
#[tokio::test]
#[ignore = "requires an isolated privileged Docker Engine"]
#[expect(
    clippy::too_many_lines,
    reason = "A long end-to-end scenario; rustfmt wraps its expect messages."
)]
async fn links_reach_only_the_imported_service_until_removed() {
    use piqueld_core::links::Link;
    use piqueld_core::{PlanRequest, ServiceName};
    let engine = IsolatedDocker::from_env().await;
    let suffix = &uuid::Uuid::now_v7().simple().to_string()[..12];
    let exporter_name = format!("auth-{suffix}");
    let whoami = engine
        .docker
        .resolve_image("traefik/whoami:v1.10")
        .await
        .expect("resolve image succeeds");
    let alpine = engine
        .docker
        .resolve_image("alpine:3.20")
        .await
        .expect("resolve image succeeds");
    let images = BTreeMap::from([("traefik/whoami:v1.10", whoami), ("alpine:3.20", alpine)]);
    let compile = |name: &str, services: serde_json::Value| compile_images(name, services, &images);
    let exporter = compile(
        &exporter_name,
        serde_json::json!([
            {"name": "api", "source": {"type": "image", "image": "traefik/whoami:v1.10"}},
            {"name": "admin", "source": {"type": "image", "image": "traefik/whoami:v1.10"}},
        ]),
    );
    let admin = exporter.services[0].name.to_string();
    let shell = |script: String| serde_json::json!([{"name": "web", "source": {"type": "image", "image": "alpine:3.20"}, "command": ["/bin/sh", "-c", format!("trap 'exit 0' TERM; {script}")]}]);
    let importer = compile(
        &format!("notes-{suffix}"),
        shell(format!(
            "until wget -q -T 2 -O /dev/null http://api.{exporter_name}/; do sleep 1; done; echo reached-api; \
             for name in admin.{exporter_name} api {admin}; do getent hosts $name >/dev/null && echo $name-visible; done; \
             echo checked; while true; do sleep 1; done"
        )),
    );
    let unlinked = compile(
        &format!("other-{suffix}"),
        shell(format!(
            "while true; do getent hosts api.{exporter_name} >/dev/null && echo api-visible || echo api-hidden; sleep 1; done"
        )),
    );
    let link = Link {
        importer: importer.id.clone(),
        exporter: exporter.id.clone(),
        exporter_application: exporter.name.clone(),
        services: [ServiceName::parse("api").expect("the service name parses")].into(),
    };
    let reconcile = |desired: &piqueld_core::ResolvedApplication| PlanRequest::Reconcile {
        desired: desired.clone(),
    };

    engine.converge(&reconcile(&exporter), &[]).await;
    engine.converge(&reconcile(&unlinked), &[]).await;
    engine
        .converge(&reconcile(&importer), std::slice::from_ref(&link))
        .await;
    engine
        .converge(&reconcile(&exporter), std::slice::from_ref(&link))
        .await;
    let reached = engine.logged(&importer.id, "web", "checked").await;
    assert!(reached.contains(&"reached-api".to_owned()));
    assert!(
        !reached.iter().any(|line| line.ends_with("-visible")),
        "only api.{exporter_name} resolves in the importer: {reached:?}"
    );
    let unlinked_logs = engine.logged(&unlinked.id, "web", "api-hidden").await;
    assert!(!unlinked_logs.contains(&"api-visible".to_owned()));

    let network = piqueld_core::DockerNetworkName::for_link(&importer.id, &exporter.id);
    engine.converge(&reconcile(&importer), &[]).await;
    let api = exporter
        .services
        .iter()
        .find(|service| service.logical_name.as_str() == "api")
        .expect("the item exists");
    assert_eq!(
        engine.revoke_links(&exporter).await,
        [(api.name.to_string(), network.to_string())]
    );
    while engine
        .docker
        .observe(&importer.id)
        .await
        .expect("observe succeeds")
        .networks
        .iter()
        .any(|observed| observed.name == network.as_str())
    {
        // wall-clock: Docker reports the exporter's old tasks as attached a
        // while longer, and signals nothing when it lets them go.
        wait::pause(Duration::from_millis(500)).await;
        engine.converge(&reconcile(&importer), &[]).await;
    }
    let observed = engine
        .docker
        .observe(&exporter.id)
        .await
        .expect("observe succeeds");
    assert_eq!(observed.linked_networks, []);
    assert!(observed.services.iter().all(|service| {
        service
            .networks
            .iter()
            .all(|attachment| attachment.network != network.as_str())
    }));

    for desired in [&importer, &exporter, &unlinked] {
        let delete = PlanRequest::Delete {
            environment_id: desired.id.clone(),
            instance_id: desired.instance_id.clone(),
        };
        engine.converge(&delete, &[]).await;
    }
}

/// Runs the contract scenario `$scenario` against the isolated Engine.
macro_rules! contract_test {
    ($scenario:ident) => {
        #[tokio::test]
        #[ignore = "requires an isolated privileged Docker Engine"]
        async fn $scenario() {
            let engine = crate::IsolatedDocker::from_env().await;
            crate::docker_contract::Contract::new(engine.docker)
                .await
                .$scenario()
                .await;
        }
    };
}

/// The fake behaves like the real Engine: see [`docker_contract`].
mod contract {
    crate::docker_contract::scenarios!(contract_test);
}
