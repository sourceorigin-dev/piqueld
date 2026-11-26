use super::*;
use crate::manifest::validation::{MAX_CPU_MILLIS, REPLICAS};
use serde_json::json;

/// The setting with `key`.
fn setting(key: &str) -> &'static SettingSpec {
    SettingSpec::find(key).expect("the setting exists")
}

/// The bounds of an integer setting.
fn bounds(spec: &SettingSpec) -> (u64, u64, bool) {
    match spec.values {
        Values::Integer {
            minimum,
            maximum,
            nullable,
        } => (minimum, maximum, nullable),
        values => panic!("{} takes {values:?}, not integers", spec.key),
    }
}

#[test]
fn every_setting_takes_its_whole_declared_range_and_nothing_outside_it() {
    for spec in SETTINGS {
        let mut settings = Settings::default();
        assert!(spec.check(&spec.default_value()).is_ok(), "{}", spec.key);
        assert!(spec.check(&json!([])).is_err(), "{}", spec.key);
        let Values::Integer { .. } = spec.values else {
            continue;
        };
        let (minimum, maximum, nullable) = bounds(spec);
        for bound in [minimum, maximum] {
            spec.apply(&mut settings, &json!(bound))
                .expect("the setting takes its bound");
            assert_eq!((spec.value)(&settings), json!(bound), "{}", spec.key);
        }
        assert_eq!(spec.apply(&mut settings, &Value::Null).is_ok(), nullable);
        assert!(spec.check(&json!("1")).is_err(), "{}", spec.key);
        if let Some(below) = minimum.checked_sub(1) {
            assert!(spec.check(&json!(below)).is_err(), "{}", spec.key);
        }
        if let Some(above) = maximum.checked_add(1) {
            assert!(spec.check(&json!(above)).is_err(), "{}", spec.key);
        }
    }
}

/// Every value a choice or switch lists applies, and reads back the same.
#[test]
fn every_choice_and_switch_value_applies() {
    for spec in SETTINGS {
        let values: Vec<Value> = match spec.values {
            Values::Choice(choices) => choices.iter().map(|&choice| json!(choice)).collect(),
            Values::Boolean => vec![json!(true), json!(false)],
            Values::Integer { .. } => continue,
        };
        let mut settings = Settings::default();
        for value in values {
            spec.apply(&mut settings, &value)
                .expect("the setting takes its value");
            assert_eq!((spec.value)(&settings), value, "{}", spec.key);
        }
    }
}

#[test]
fn choices_and_switches_take_only_their_values() {
    let unavailable = setting("build.unavailable");
    let mut settings = Settings::default();
    unavailable
        .apply(&mut settings, &json!("local"))
        .expect("the choice applies");
    assert_eq!(settings.build.unavailable, WhenUnavailable::Local);
    assert!(unavailable.check(&json!("later")).is_err());
    let traffic = setting("ingress.traffic");
    traffic
        .apply(&mut settings, &json!(false))
        .expect("the switch applies");
    assert!(!settings.ingress.traffic);
    assert!(traffic.check(&json!(0)).is_err());
}

#[test]
fn preview_bounds_are_those_a_manifest_could_set() {
    let cpu = bounds(setting("previews.default_cpu_millis"));
    assert_eq!((cpu.0, cpu.1), (1, u64::from(MAX_CPU_MILLIS)));
    let replicas = bounds(setting("previews.max_replicas"));
    assert_eq!(
        (replicas.0, replicas.1),
        (u64::from(*REPLICAS.start()), u64::from(*REPLICAS.end()))
    );
    let memory = bounds(setting("previews.default_memory_bytes"));
    assert_eq!(memory.1, i64::MAX.unsigned_abs());
}

/// A setting needing a build machine can't be changed without one, but can
/// be reset.
#[test]
fn a_setting_needing_a_build_machine_applies_only_with_one() {
    let unavailable = setting("build.unavailable");
    let mut layers = SettingLayers::default();
    let refused = SettingError::Unavailable {
        key: "build.unavailable",
        needs: Requirement::BuildMachine,
    };
    assert_eq!(
        layers.check(unavailable, Some(&json!("local"))),
        Err(refused.clone())
    );
    assert!(layers.view(unavailable).needs.is_some());
    layers.check(unavailable, None).expect("a reset is allowed");

    layers.met.insert(Requirement::BuildMachine);
    layers
        .check(unavailable, Some(&json!("local")))
        .expect("the change is allowed");
    assert_eq!(layers.view(unavailable).needs, None);
}

#[test]
fn a_pin_beats_a_stored_value_which_beats_the_default() {
    let layers = SettingLayers {
        pins: SettingsPins {
            retention: RetentionPins {
                event_days: Some(7),
                ..RetentionPins::default()
            },
            ..SettingsPins::default()
        },
        stored: BTreeMap::from([
            ("retention.event_days".into(), json!(30)),
            ("retention.audit_days".into(), json!(30)),
            ("reconciliation.scan_interval_seconds".into(), json!(0)),
            ("unknown.setting".into(), json!(1)),
        ]),
        met: BTreeSet::new(),
    };
    let settings = layers.effective();
    assert_eq!(settings.retention.event_days, 7);
    assert_eq!(settings.retention.audit_days, 30);
    assert_eq!(settings.reconciliation.scan_interval_seconds, 60);
    let sources: Vec<_> = [
        "retention.event_days",
        "retention.audit_days",
        "reconciliation.scan_interval_seconds",
    ]
    .map(|key| layers.view(setting(key)).source)
    .into();
    assert_eq!(
        sources,
        [
            SettingSource::Configuration,
            SettingSource::Changed,
            SettingSource::Default
        ]
    );

    let pinned = setting("retention.event_days");
    assert_eq!(
        layers.check(pinned, None),
        Err(SettingError::Pinned("retention.event_days"))
    );
    let tokens = setting("auth.max_token_days");
    layers
        .check(tokens, Some(&Value::Null))
        .expect("no limit is allowed");
    assert!(layers.check(tokens, Some(&json!(0))).is_err());
}

#[test]
fn descriptions_are_prose_and_keys_are_unique() {
    let spec = setting("retention.event_days");
    assert_eq!(
        spec.description(),
        "Days application events are kept; `0` keeps them forever. Deleting an application also removes its events."
    );
    let keys: BTreeSet<_> = SETTINGS.iter().map(|spec| spec.key).collect();
    assert_eq!(keys.len(), SETTINGS.len());
}
