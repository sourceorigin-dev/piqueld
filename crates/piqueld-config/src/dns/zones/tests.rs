use super::*;

fn zone(name: &str) -> Zone {
    Zone {
        name: Hostname::parse(name).unwrap(),
        id: name.into(),
    }
}

fn host(name: &str) -> Hostname {
    Hostname::parse(name).unwrap()
}

#[test]
fn longest_zone_wins_and_shared_zones_conflict() {
    let first = [zone("piquel.fr"), zone("shared.com")];
    let second = [zone("dev.piquel.fr"), zone("shared.com")];
    let providers = Zones(vec![first.as_slice(), second.as_slice()]);
    for (hostname, expected) in [
        ("piquel.fr", Ok((0, "piquel.fr"))),
        ("admin.piquel.fr", Ok((0, "piquel.fr"))),
        ("x.dev.piquel.fr", Ok((1, "dev.piquel.fr"))),
        ("dev.piquel.fr", Ok((1, "dev.piquel.fr"))),
        (
            "app.shared.com",
            Err(ZoneError::Conflict(host("shared.com"))),
        ),
        ("other.com", Err(ZoneError::NoZone(host("other.com")))),
        ("notpiquel.fr", Err(ZoneError::NoZone(host("notpiquel.fr")))),
    ] {
        let found = providers
            .owner(&host(hostname))
            .map(|(index, zone)| (index, zone.name.as_str()));
        assert_eq!(found, expected, "{hostname}");
    }
    assert_eq!(providers.conflicts(0), ["shared.com"]);
    assert_eq!(providers.conflicts(1), ["shared.com"]);
}
