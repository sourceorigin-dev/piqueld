use super::*;

/// The built-in defaults pass the same validation as a file's.
#[test]
fn empty_document_uses_documented_defaults() {
    assert_eq!(
        DaemonConfig::from_toml("").expect("the config parses"),
        DaemonConfig::validated_default().expect("the default config validates")
    );
}

#[test]
fn unknown_sections_are_rejected() {
    assert!(matches!(
        DaemonConfig::from_toml("[extra]\nvalue = 'x'"),
        Err(ConfigError::Parse(_))
    ));
}

#[test]
fn host_paths_and_listener_settings_are_validated() {
    for document in [
        "[server]\nallowed_hosts = ['*.example.com']",
        "[server]\nallowed_hosts = ['http://example.com']",
        "[server]\nallowed_hosts = ['example.com:7845']",
        "[server]\nallowed_hosts = ['']",
        "[server]\nlisten_mode = 'all'",
        "[server]\ndata_dir = 'relative/state'",
        "[server]\ndata_dir = '/'",
        "[server]\nruntime_dir = 'relative/run'",
        "[server]\nruntime_dir = '/'",
        "[server]\ndata_dir = '/run/piqueld'",
        "[server]\ndata_dir = '/srv/x:/etc'",
        "[server]\ndata_dir = '/srv/a,b'",
        "[docker]\nsocket = 'relative.sock'",
    ] {
        assert!(
            matches!(
                DaemonConfig::from_toml(document),
                Err(ConfigError::Parse(_) | ConfigError::Invalid(_))
            ),
            "accepted {document}"
        );
    }
}

/// The schema's ranges are enforced when the configuration loads, in every
/// table, so the published schema and settings reference never accept a
/// value loading refuses.
#[test]
fn schema_ranges_are_enforced() {
    /// Checks the ranges of the settings in `schema`, the table at `path`
    /// (`ingress.acme`), and in its subtables, returning how many it
    /// checked. The entries of an `array` table, such as `[[dns.providers]]`,
    /// need settings this cannot fill in, so they must have no range.
    fn check(definitions: &Value, schema: &Value, path: &str, array: bool) -> usize {
        let schema = match schema["$ref"].as_str() {
            Some(reference) => &definitions[reference.trim_start_matches("#/definitions/")],
            None => schema,
        };
        let mut checked = ["allOf", "oneOf"]
            .into_iter()
            .flat_map(|key| schema[key].as_array().into_iter().flatten())
            .map(|member| check(definitions, member, path, array))
            .sum();
        for (setting, property) in schema["properties"].as_object().into_iter().flatten() {
            let (nested, array) = match property["type"].as_str() {
                Some("array") => (&property["items"], true),
                _ => (property, array),
            };
            if nested.get("$ref").is_some() || nested.get("oneOf").is_some() {
                let path = if path.is_empty() {
                    setting.clone()
                } else {
                    format!("{path}.{setting}")
                };
                checked += check(definitions, nested, &path, array);
                continue;
            }
            let load =
                |value: u64| DaemonConfig::from_toml(&format!("[{path}]\n{setting} = {value}"));
            let minimum = property["minimum"].as_u64().filter(|minimum| *minimum > 0);
            let maximum = property["maximum"].as_u64();
            for (bound, outside) in [
                (minimum, minimum.map(|minimum| minimum - 1)),
                (maximum, maximum.map(|maximum| maximum + 1)),
            ] {
                let (Some(bound), Some(outside)) = (bound, outside) else {
                    continue;
                };
                assert!(
                    !array,
                    "{path}.{setting}: ranges in array tables go unchecked"
                );
                assert!(load(bound).is_ok(), "{path}.{setting} rejects {bound}");
                assert!(load(outside).is_err(), "{path}.{setting} accepts {outside}");
                checked += 1;
            }
        }
        checked
    }

    let schema = DaemonConfig::json_schema();
    let checked = check(&schema["definitions"], &schema, "", false);
    assert!(checked > 5, "only {checked} bounds were found");
}

#[test]
fn tcp_defaults_to_off_with_or_without_a_server_table() {
    for source in ["", "[server]", "[server]\ndata_dir = '/tmp/p'"] {
        let config = DaemonConfig::from_toml(source).expect("the config parses");
        assert_eq!(config.server.listen_mode, ListenMode::Off);
        assert_eq!(config.server.port, 7845);
    }
    for mode in ["off", "localhost", "tailscale", "both"] {
        let config =
            DaemonConfig::from_toml(&format!("[server]\nlisten_mode = '{mode}'\nport = 8443"))
                .expect("the config parses");
        assert_eq!(config.server.listen_mode.to_string(), mode);
        assert_eq!(config.server.port, 8443);
    }
}

#[test]
fn socket_and_database_use_separate_directories() {
    for (server, socket, database) in [
        (
            "data_dir = '/srv/piqueld'",
            "/run/piqueld/piqueld.sock",
            "/srv/piqueld/piqueld.db",
        ),
        (
            "runtime_dir = '/tmp/piqueld-dev-run'",
            "/tmp/piqueld-dev-run/piqueld.sock",
            "/var/lib/piqueld/piqueld.db",
        ),
    ] {
        let config =
            DaemonConfig::from_toml(&format!("[server]\n{server}")).expect("the config parses");
        assert_eq!(config.server.socket_path(), PathBuf::from(socket));
        assert_eq!(config.server.database_path(), PathBuf::from(database));
    }
}

#[test]
fn parse_failures_keep_the_diagnostic_but_not_the_source_line() {
    let error = DaemonConfig::from_toml(
        "[server]\nport = 7845\n\n[auth]\n# token lifetimes are daemon settings\npublic_url = 'https://secret-token.example.com'\nunknown_key = true",
    )
    .expect_err("the config is invalid");
    let ConfigError::Parse(diagnostic) = &error else {
        panic!("expected a parse error, got {error:?}");
    };
    assert!(
        diagnostic.message.contains("unknown field") && diagnostic.message.contains("unknown_key"),
        "diagnostic should name the offending field: {diagnostic}"
    );
    assert_eq!(
        diagnostic.location,
        Some(piqueld_core::TomlLocation { line: 7, column: 1 })
    );
    let rendered = format!("{:?}", anyhow::Error::new(error));
    assert!(
        !rendered.contains("secret-token"),
        "diagnostic must not echo configuration source: {rendered}"
    );
}

/// A file an older version took may set daemon settings: each is refused,
/// with the way to change it now, whether it has a table of its own or
/// sits among host settings.
#[test]
fn daemon_settings_are_refused_with_how_to_change_them() {
    for (source, message) in [
        (
            "[retention]\nevent_days = 7",
            "[retention] holds daemon settings, which the configuration file no longer sets",
        ),
        ("[previews]", "[previews] holds daemon settings"),
        (
            "[auth]\npublic_url = 'https://piqueld.example.com'\nmax_token_days = 90",
            "auth.max_token_days is a daemon setting, which the configuration file no longer sets: remove it, and change it with `piquelctl settings set auth.max_token_days <value>`",
        ),
        (
            "[build]\nunavailable = 'local'",
            "build.unavailable is a daemon setting",
        ),
        (
            "[ingress]\ntraffic = false",
            "ingress.traffic is a daemon setting",
        ),
    ] {
        let error = DaemonConfig::from_toml(source).expect_err("the configuration is refused");
        assert!(
            matches!(
                error,
                ConfigError::DaemonSetting(_) | ConfigError::DaemonSettings(_)
            ),
            "{source}: {error:?}"
        );
        assert!(error.to_string().contains(message), "{source}: {error}");
    }
    assert!(matches!(
        DaemonConfig::from_toml("[auth]\nmax_token_day = 1"),
        Err(ConfigError::Parse(_))
    ));
    DaemonConfig::from_toml(
        "[auth]\npublic_url = 'https://piqueld.example.com'\n[ingress]\nenabled = false",
    )
    .expect("host settings in a table that also held daemon settings still parse");
}

#[test]
fn tailscale_node_settings_are_validated() {
    let config = DaemonConfig::from_toml(
        "[tailscale]\nhostname = 'piqueld-dev'\nauth_key_file = '/run/credentials/ts-auth-key'",
    )
    .expect("the tailscale settings parse");
    assert_eq!(config.tailscale.hostname, "piqueld-dev");
    assert_eq!(
        config.server.tailscale_dir(),
        PathBuf::from("/var/lib/piqueld/tailscale")
    );
    assert_eq!(config.public_url(), "http://localhost:7845");
    assert_eq!(config.tailscale.https_port.get(), 443);
    let shared = DaemonConfig::from_toml(
        "[tailscale]\nsocket = '/run/tailscale-shared/tailscaled.sock'\nhttps_port = 7846",
    )
    .expect("the shared tailscale settings parse");
    assert_eq!(shared.tailscale.https_port.get(), 7846);
    for document in [
        "[tailscale]\nhostname = ''",
        "[tailscale]\nhostname = 'piqueld.example'",
        "[tailscale]\nhostname = '-piqueld'",
        "[tailscale]\nsocket = 'tailscaled.sock'",
        "[tailscale]\nsocket = '/run/ts.sock'\nauth_key_file = '/run/credentials/ts-auth-key'",
    ] {
        assert!(
            DaemonConfig::from_toml(document).is_err(),
            "accepted {document}"
        );
    }
}

#[test]
fn the_dashboard_hostname_needs_private_ingress_and_becomes_the_public_url() {
    let private = "[ingress]\nenabled = true\n[ingress.private]\nenabled = true\n";
    let config = DaemonConfig::from_toml(&format!(
        "{private}dashboard_hostname = 'dashboard.example.com'"
    ))
    .expect("the dashboard hostname parses");
    assert_eq!(config.public_url(), "https://dashboard.example.com");
    let explicit = DaemonConfig::from_toml(&format!(
        "[auth]\npublic_url = 'https://piqueld.example.com'\n{private}dashboard_hostname = 'dashboard.example.com'"
    ))
    .expect("the explicit public url parses");
    assert_eq!(explicit.public_url(), "https://piqueld.example.com");
    for document in [
        "[ingress.private]\nenabled = true\ndashboard_hostname = 'dashboard.example.com'".into(),
        "[ingress]\nenabled = true\n[ingress.private]\ndashboard_hostname = 'dashboard.example.com'".into(),
        format!("{private}dashboard_hostname = '*.example.com'"),
        "[ingress]\nenabled = true\nwebhook_hostname = 'dashboard.example.com'\n\
         [ingress.private]\nenabled = true\ndashboard_hostname = 'dashboard.example.com'"
            .into(),
    ] {
        assert!(
            DaemonConfig::from_toml(&document).is_err(),
            "accepted {document}"
        );
    }
}

#[test]
fn credential_files_are_read_and_never_shown() {
    let directory = tempfile::tempdir().expect("the temp dir is created");
    let headers = directory.path().join("otlp-headers");
    let auth_key = directory.path().join("ts-auth-key");
    std::fs::write(&headers, "authorization: Bearer secret\n").expect("the file is written");
    let source = format!(
        "[tailscale]\nauth_key_file = '{}'\n\
         [telemetry]\nendpoint = 'unix:///run/otel/otlp.sock'\nheaders_file = '{}'",
        auth_key.display(),
        headers.display()
    );
    let config = DaemonConfig::from_toml(&source).expect("the config parses");
    assert_eq!(
        config
            .telemetry
            .headers
            .as_ref()
            .expect("the headers are set")
            .expose(),
        "authorization: Bearer secret"
    );
    let view = format!("{:?} {config:?}", config.view());
    assert!(
        view.contains(&format!("from {}", headers.display())),
        "{view}"
    );
    assert!(
        view.contains(&format!("from {}", auth_key.display())),
        "{view}"
    );
    assert!(!view.contains("secret"), "{view}");
}

#[test]
fn dns_providers_accept_credentials_only_from_files() {
    let directory = tempfile::tempdir().expect("the temp dir is created");
    for name in ["cf", "ak", "as", "ck"] {
        std::fs::write(directory.path().join(name), format!("{name}-value\n"))
            .expect("the file is written");
    }
    let path = |name: &str| directory.path().join(name).display().to_string();
    let source = format!(
        "[ingress.acme]\nemail = 'admin@example.com'\n\
         [ingress]\npublic_addresses = ['203.0.113.10', '2001:db8::10']\n\
         [[dns.providers]]\nkind = 'cloudflare'\napi_token_file = '{}'\nmanage_records = true\n\
         [[dns.providers]]\nkind = 'ovh'\nendpoint = 'ovh-eu'\napplication_key_file = '{}'\n\
         application_secret_file = '{}'\nconsumer_key_file = '{}'",
        path("cf"),
        path("ak"),
        path("as"),
        path("ck")
    );
    let config = DaemonConfig::from_toml(&source).expect("the config parses");
    assert_eq!(
        config
            .dns
            .providers
            .iter()
            .map(|config| (config.provider.kind(), config.manage_records))
            .collect::<Vec<_>>(),
        [("cloudflare", true), ("ovh", false)]
    );
    let view = config.view();
    let dns = &view.groups["DNS providers"];
    assert_eq!(
        dns["1. cloudflare"],
        format!("API token from {}; manages route records", path("cf"))
    );
    assert!(
        dns["2. ovh"].starts_with("ovh-eu, application key from")
            && dns["2. ovh"].ends_with("; route records are manual"),
        "{dns:?}"
    );
    assert_eq!(view.groups["Ingress"]["ACME email"], "admin@example.com");
    assert_eq!(
        view.groups["Ingress"]["Public addresses"],
        "203.0.113.10, 2001:db8::10"
    );
    let shown = format!("{view:?} {config:?}");
    assert!(!shown.contains("-value"), "{shown}");

    for invalid in [
        "[[dns.providers]]\nkind = 'cloudflare'\napi_token = 'inline'",
        "[[dns.providers]]\nkind = 'route53'",
        "[[dns.providers]]\nkind = 'cloudflare'\napi_token_file = '/x'\nmanage_record = true",
        "[ingress]\npublic_addresses = ['example.com']",
        "[[dns.providers]]\nkind = 'ovh'\nendpoint = 'ovh-mars'",
        "[ingress.acme]\ndirectory = 'http://acme.example.com/directory'",
        "[ingress.acme]\nemail = 'not an email'",
    ] {
        assert!(
            DaemonConfig::from_toml(invalid).is_err(),
            "accepted {invalid}"
        );
    }
}

/// The example the repository ships, which the daemon suggests with
/// `--config examples/piqueld.toml`, stays valid.
#[test]
fn the_shipped_example_is_valid() {
    DaemonConfig::check(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/piqueld.toml"
    ))
    .expect("the shipped example config is valid");
}

#[test]
fn checks_locate_credential_files_without_reading_them() {
    let directory = tempfile::tempdir().expect("the temp dir is created");
    let path = directory.path().join("piqueld.toml");
    let check = |document: &str| {
        std::fs::write(&path, document).expect("the file is written");
        DaemonConfig::check(&path)
    };
    let credentials = "[telemetry]\nendpoint = 'unix:///run/otel/otlp.sock'\nheaders_file = 'otlp-headers'\n\
         [ingress.private]\nauth_key_file = 'ts-apps-auth-key'\n\
         [ingress.tunnel]\nenabled = true\ncredentials_file = 'cloudflared-tunnel'\n\
         [[dns.providers]]\nkind = 'cloudflare'\napi_token_file = '/missing/token'";
    check(credentials).expect("the provider credentials are valid");
    DaemonConfig::load(&path).expect_err("`DaemonConfig::load` should be rejected");
    for invalid in [
        "[server]\nport = 0",
        "[ingress.tunnel]\nenabled = true",
        "[telemetry]\nheaders_file = 'otlp-headers'",
        "[tailscale]\nsocket = '/run/ts.sock'\nauth_key_file = 'ts-auth-key'",
    ] {
        assert!(check(invalid).is_err(), "accepted {invalid}");
    }
    std::fs::write(
        &path,
        "[[dns.providers]]\nkind = 'cloudflare'\napi_token_file = '/missing/token'",
    )
    .expect("the provider config is written");
    let error = format!(
        "{:#}",
        anyhow::Error::new(DaemonConfig::load(&path).expect_err("the config is invalid"))
    );
    assert!(
        error.contains("could not read `api_token_file` /missing/token"),
        "{error}"
    );
    assert!(matches!(
        DaemonConfig::check(directory.path().join("missing.toml")),
        Err(ConfigError::Read(_))
    ));
}

#[test]
fn public_urls_are_https_origins_or_localhost() {
    for origin in [
        "https://piqueld.example",
        "http://localhost:7845",
        "http://worktree-1a2b3c4d.localhost:7846",
    ] {
        AuthConfig::validate_origin(origin).expect("`AuthConfig::validate_origin` should succeed");
    }
    for origin in [
        "http://piqueld.example",
        "http://localhost.example",
        "https://127.0.0.1",
        "https://user:secret@example.com",
        "https://example.com/path",
        "https://example.com?query",
    ] {
        AuthConfig::validate_origin(origin)
            .expect_err("`AuthConfig::validate_origin` should be rejected");
    }
}
