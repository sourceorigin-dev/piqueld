use super::*;

fn parse(toml: &str) -> Result<BuildConfig, toml::de::Error> {
    toml::from_str(toml)
}

#[test]
fn builds_run_one_at_a_time_unless_parallel_says_otherwise() {
    let parallel = |toml| parse(toml).expect("the setting is valid").parallel.get();
    assert_eq!(parallel(""), 1);
    assert_eq!(parallel("parallel = 4"), 4);
    for invalid in ["parallel = 0", "parallel = 256"] {
        assert!(parse(invalid).is_err(), "{invalid}");
    }
}

#[test]
fn a_build_machine_needs_its_builder_and_its_repository() {
    assert_eq!(
        parse("").expect("an empty build config parses"),
        BuildConfig::default()
    );
    let remote = parse(
        "builder = 'tcp://build.example.ts.net:1234'\n\
         repository = 'build.example.ts.net:5000/piqueld'",
    )
    .expect("the remote build config parses")
    .remote
    .expect("the build config is remote");
    assert_eq!(
        remote.builder.to_string(),
        "tcp://build.example.ts.net:1234"
    );
    assert_eq!(
        remote.repository.to_string(),
        "build.example.ts.net:5000/piqueld"
    );
    for invalid in [
        "builder = 'tcp://build.example.ts.net:1234'",
        "repository = 'build.example.ts.net:5000/piqueld'",
        "unavailable = 'local'",
        "builder = 'tcp://build.example.ts.net'\nrepository = 'registry.example/piqueld'",
        "builder = 'tcp://build.example:1234'\nrepository = 'piqueld'",
    ] {
        assert!(parse(invalid).is_err(), "{invalid}");
    }
}
