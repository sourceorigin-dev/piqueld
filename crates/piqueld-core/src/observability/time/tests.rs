use super::{Age, Elapsed, Timestamp, Uptime, UtcTime};
use std::time::Duration;

#[test]
fn times_read_as_utc_calendar_dates() {
    for (ms, minute, day, exact) in [
        (
            0,
            "1970-01-01 00:00",
            "1970-01-01",
            "1970-01-01T00:00:00.000Z",
        ),
        (
            951_782_400_000,
            "2000-02-29 00:00",
            "2000-02-29",
            "2000-02-29T00:00:00.000Z",
        ),
        (
            1_792_672_425_063,
            "2026-10-22 12:33",
            "2026-10-22",
            "2026-10-22T12:33:45.063Z",
        ),
        (
            -60_000,
            "1969-12-31 23:59",
            "1969-12-31",
            "1969-12-31T23:59:00.000Z",
        ),
    ] {
        assert_eq!(UtcTime(ms).to_string(), minute);
        assert_eq!(format!("{:#}", UtcTime(ms)), day);
        assert_eq!(UtcTime(ms).rfc3339(), exact);
    }
}

#[test]
fn uptime_keeps_its_two_largest_units() {
    for (seconds, text) in [
        (0, "0 s"),
        (45, "45 s"),
        (720, "12 min"),
        (7_500, "2 h 5 min"),
        (273_600, "3 d 4 h"),
    ] {
        assert_eq!(Uptime(Duration::from_secs(seconds)).to_string(), text);
    }
}

#[test]
fn elapsed_uses_the_largest_fitting_units() {
    for (milliseconds, text) in [
        (-5, "0 ms"),
        (850, "850 ms"),
        (999, "999 ms"),
        (1_000, "1.0 s"),
        (12_340, "12.3 s"),
        (12_350, "12.4 s"),
        (59_949, "59.9 s"),
        (59_950, "1 min 0 s"),
        (252_000, "4 min 12 s"),
        (3_599_499, "59 min 59 s"),
        (3_599_500, "1 h 0 min"),
        (7_500_000, "2 h 5 min"),
    ] {
        assert_eq!(Elapsed::from_millis(milliseconds).to_string(), text);
    }
    assert_eq!(Elapsed::from_millis_f64(3_245.7).to_string(), "3.2 s");
    assert_eq!(Elapsed::from_millis_f64(f64::NAN).to_string(), "0 ms");
}

#[test]
fn timestamps_are_utc_dates_and_times() {
    for (milliseconds, text) in [
        (0, "1970-01-01 00:00:00Z"),
        (-1, "1969-12-31 23:59:59Z"),
        (1_767_225_599_999, "2025-12-31 23:59:59Z"),
        (1_792_839_646_134, "2026-10-24 11:00:46Z"),
    ] {
        assert_eq!(Timestamp(milliseconds).to_string(), text);
    }
    assert_eq!(Timestamp::from_secs(60).to_string(), "1970-01-01 00:01:00Z");
}

/// The label of a moment `seconds` before now.
fn ago(seconds: i64) -> Option<String> {
    Age::at(0, seconds * 1000).label()
}

#[test]
fn ages_switch_units_at_their_boundaries_and_minutes_round() {
    for (seconds, label) in [
        (0, "just now"),
        (44, "just now"),
        (45, "1 min ago"),
        (89, "1 min ago"),
        (90, "2 min ago"),
        (3_569, "59 min ago"),
        (3_570, "1 h ago"),
        (86_399, "23 h ago"),
        (86_400, "1 d ago"),
        (604_799, "6 d ago"),
    ] {
        assert_eq!(ago(seconds).as_deref(), Some(label), "{seconds} s");
    }
    assert_eq!(ago(604_800), None);
}

#[test]
fn ages_of_moments_to_come_read_forward() {
    assert_eq!(ago(-44).as_deref(), Some("in a moment"));
    assert_eq!(ago(-90).as_deref(), Some("in 2 min"));
    assert_eq!(ago(-2 * 86_400).as_deref(), Some("in 2 d"));
    assert_eq!(ago(-604_800), None);
    // The difference saturates rather than overflowing.
    assert_eq!(Age::at(i64::MIN, i64::MAX).label(), None);
}
