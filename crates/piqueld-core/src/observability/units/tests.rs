use super::{Bytes, Count, Tenths};

#[test]
fn tenths_round_half_up_and_drop_a_whole_tenth_on_request() {
    for (value, unit, text, compact) in [
        (0, 10, "0.0", "0"),
        (14, 100, "0.1", "0.1"),
        (1_049, 1_000, "1.0", "1"),
        (1_050, 1_000, "1.1", "1.1"),
        (9_950, 1_000, "10.0", "10"),
    ] {
        let tenths = Tenths::of(value, unit);
        assert_eq!(tenths.to_string(), text, "{value} / {unit}");
        assert_eq!(format!("{tenths:#}"), compact, "{value} / {unit}");
    }
}

#[test]
fn bytes_use_the_largest_reached_binary_unit() {
    for (bytes, text) in [
        (0, "0 B"),
        (1023, "1023 B"),
        (1024, "1.0 KiB"),
        (1536, "1.5 KiB"),
        (1_048_524, "1023.9 KiB"),
        (1_048_525, "1.0 MiB"),
        (1_073_689_395, "1023.9 MiB"),
        (1_073_689_396, "1.0 GiB"),
        (1_099_457_940_684, "1023.9 GiB"),
        (1_099_457_940_685, "1.0 TiB"),
        (52_428_800, "50.0 MiB"),
        (127_026_911_232, "118.3 GiB"),
        (u64::MAX, "16777216.0 TiB"),
    ] {
        assert_eq!(Bytes(bytes).to_string(), text);
    }
    assert_eq!(Bytes::from_i64(-1).to_string(), "0 B");
}

#[test]
fn counts_group_thousands_and_compact_on_scales() {
    for (n, full, compact) in [
        (0, "0", "0"),
        (999, "999", "999"),
        (1_284, "1,284", "1,284"),
        (9_999, "9,999", "9,999"),
        (10_000, "10,000", "10K"),
        (12_940, "12,940", "12.9K"),
        (20_000, "20,000", "20K"),
        (999_949, "999,949", "999.9K"),
        (999_950, "999,950", "1M"),
        (4_210_000, "4,210,000", "4.2M"),
        (-1_000, "-1,000", "-1,000"),
        (-12_940, "-12,940", "-12.9K"),
        (-15_000, "-15,000", "-15K"),
        (-4_210_000, "-4,210,000", "-4.2M"),
        (i64::MIN, "-9,223,372,036,854,775,808", "-9223372036854.8M"),
    ] {
        assert_eq!(Count(n).to_string(), full);
        assert_eq!(format!("{:#}", Count(n)), compact);
    }
}
