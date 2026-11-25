use super::*;

#[test]
fn a_builder_is_a_tcp_host_and_port() {
    for (endpoint, address) in [
        (
            "tcp://build.example.ts.net:1234",
            "build.example.ts.net:1234",
        ),
        ("tcp://100.64.0.1:1234/", "100.64.0.1:1234"),
        ("tcp://[fd7a::1]:1234", "[fd7a::1]:1234"),
    ] {
        let parsed = BuilderEndpoint::try_from(endpoint.to_owned()).unwrap();
        assert_eq!(parsed.address(), address);
    }
    for invalid in [
        // No port, another scheme, credentials, a path, and a bare IPv6 host.
        "tcp://build.example.ts.net",
        "ssh://build.example.ts.net:22",
        "tcp://user@build.example:1234",
        "tcp://build.example:1234/buildkit",
        "tcp://fd7a::1:1234",
        "tcp://:1234",
    ] {
        assert!(
            BuilderEndpoint::try_from(invalid.to_owned()).is_err(),
            "{invalid}"
        );
    }
}

#[test]
fn a_build_repository_is_on_a_named_registry_without_a_tag() {
    let repository =
        BuildRepository::try_from("build.example.ts.net:5000/piqueld".to_owned()).unwrap();
    let digest = Sha256Digest::parse(format!("sha256:{}", "a".repeat(64))).unwrap();
    assert_eq!(
        repository.image(&digest).as_str(),
        format!("build.example.ts.net:5000/piqueld@{digest}")
    );
    assert_eq!(repository.registry(), "build.example.ts.net:5000");
    assert_eq!(repository.path(), "piqueld");
    assert_eq!(
        repository.digest_of(&repository.image(&digest)),
        Some(digest.clone())
    );
    // The same digest in another repository, or one this one prefixes.
    for other in [
        "build.example.ts.net:5000/other",
        "build.example.ts.net:5000/piqueld/sub",
    ] {
        let other = BuildRepository::try_from(other.to_owned()).unwrap();
        assert_eq!(repository.digest_of(&other.image(&digest)), None);
    }
    // Docker Hub's, a tag, and upper case.
    for invalid in [
        "piqueld",
        "registry.example/piqueld:v1",
        "registry.example/Piqueld",
    ] {
        assert!(
            BuildRepository::try_from(invalid.to_owned()).is_err(),
            "{invalid}"
        );
    }
}
