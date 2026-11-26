use super::*;

#[test]
fn isolates_peers_without_a_shared_cap_and_recovers_after_window() {
    let mut throttle = Throttle::default();
    let now = Instant::now();
    let a = Some("192.0.2.1".parse().expect("the value parses"));
    for _ in 0..PER_PEER {
        throttle.admit(a, now).expect("admit succeeds");
    }
    assert!(throttle.admit(a, now).is_err());
    for host in 2..10 {
        let peer = Some(format!("192.0.2.{host}").parse().expect("the value parses"));
        for _ in 0..PER_PEER {
            throttle.admit(peer, now).expect("admit succeeds");
        }
    }
    throttle.admit(None, now).expect("admit succeeds");
    throttle.admit(a, now + WINDOW).expect("admit succeeds");
    assert_eq!(throttle.peers.len(), 1);
}

#[test]
fn groups_ipv6_peers_by_prefix() {
    let mut throttle = Throttle::default();
    let now = Instant::now();
    for host in 0..PER_PEER {
        let peer = format!("2001:db8:0:1::{host:x}")
            .parse()
            .expect("the value parses");
        throttle.admit(Some(peer), now).expect("admit succeeds");
    }
    let same_prefix = "2001:db8:0:1:ffff::1".parse().expect("the value parses");
    assert!(throttle.admit(Some(same_prefix), now).is_err());
    let other_prefix = "2001:db8:0:2::1".parse().expect("the value parses");
    throttle
        .admit(Some(other_prefix), now)
        .expect("admit succeeds");
    let mapped: IpAddr = "::ffff:192.0.2.1".parse().expect("the value parses");
    assert_eq!(
        bucket(mapped),
        "192.0.2.1".parse::<IpAddr>().expect("the ip addr parses")
    );
}
