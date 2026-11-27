use super::parse_value;
use serde_json::json;

#[test]
fn values_are_numbers_none_switches_or_strings() {
    assert_eq!(parse_value("30"), json!(30));
    assert_eq!(parse_value("none"), json!(null));
    assert_eq!(parse_value("false"), json!(false));
    assert_eq!(parse_value("local"), json!("local"));
    assert_eq!(parse_value("-1"), json!("-1"));
}
