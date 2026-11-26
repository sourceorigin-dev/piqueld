use super::super::HealthCheck;
use super::*;

/// The probe config belongs exactly when the health check runs the probe,
/// and only as this daemon's version for its installation mounted at the
/// container path.
#[test]
fn the_probe_config_must_match_the_health_check() {
    let http = HealthCheck::Http {
        port: 8080,
        path: "/health".into(),
        interval_seconds: 10,
        timeout_seconds: 3,
        start_period_seconds: 0,
        retries: 3,
    };
    let command = HealthCheck::Command {
        command: vec!["true".into()],
        interval_seconds: 10,
        timeout_seconds: 3,
        start_period_seconds: 0,
        retries: 3,
    };
    let matches = |health: &HealthCheck, configs: Vec<TaskSpecContainerSpecConfigs>| {
        HealthProbe::matches(
            &TaskSpecContainerSpec {
                health_check: Some(BollardDocker::health_config(health)),
                configs: Some(configs),
                ..Default::default()
            },
            Some("instance-1"),
        )
    };
    let probe = HealthProbe::reference("config-id".into(), "instance-1");
    let another_installations = HealthProbe::reference("config-id".into(), "instance-2");
    let older = TaskSpecContainerSpecConfigs {
        config_name: Some("piqueld-health-probe-0000000000000000".into()),
        ..probe.clone()
    };
    let mut writable = probe.clone();
    writable.file.as_mut().expect("the field is set").mode = Some(0o777);

    let file = TaskSpecContainerSpecConfigs {
        config_name: Some("piqueld-app-file-0000".into()),
        file: Some(TaskSpecContainerSpecFile1 {
            name: Some("/etc/app.conf".into()),
            ..probe.file.clone().expect("the probe has a file")
        }),
        ..probe.clone()
    };

    assert!(matches(&http, vec![probe.clone()]));
    assert!(matches(&http, vec![file.clone(), probe.clone()]));
    assert!(matches(&command, vec![file]));
    assert!(!matches(&http, Vec::new()));
    assert!(!matches(&http, vec![older]));
    assert!(!matches(&http, vec![another_installations]));
    assert!(!matches(&http, vec![writable]));
    assert!(!matches(
        &http,
        vec![HealthProbe::reference(String::new(), "instance-1")]
    ));
    assert!(!matches(&http, vec![probe.clone(), probe.clone()]));
    assert!(!matches(&command, vec![probe]));
    assert!(matches(&command, Vec::new()));
}

#[test]
fn the_embedded_probe_fits_a_swarm_config() {
    assert!(BINARY.len() <= 500 * 1024, "{} bytes", BINARY.len());
    assert!(BINARY.starts_with(b"\x7fELF"));
    let name = HealthProbe::name(&"a".repeat(64));
    assert!(name.starts_with("piqueld-health-probe-") && name.len() <= 63);
    assert_ne!(name, HealthProbe::name("instance-1"));
}
