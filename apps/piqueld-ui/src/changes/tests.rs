use super::*;

fn change(before: Option<&str>, after: Option<&str>) -> ManifestChange {
    ManifestChange {
        field: "services.web.environment".into(),
        before: before.map(str::to_owned),
        after: after.map(str::to_owned),
    }
}

#[test]
fn scalars_show_side_by_side_and_structures_line_by_line() {
    assert_eq!(
        ChangeDisplay::of(&change(Some("1"), Some("2"))),
        ChangeDisplay::Scalar {
            before: Some("1".into()),
            after: Some("2".into()),
        }
    );
    assert_eq!(
        ChangeDisplay::of(&change(None, Some("\"nginx:1.27\""))),
        ChangeDisplay::Scalar {
            before: None,
            after: Some("\"nginx:1.27\"".into()),
        }
    );
    let ChangeDisplay::Lines(lines) = ChangeDisplay::of(&change(
        Some(r#"{"PORT":"80","ROLE":"web"}"#),
        Some(r#"{"PORT":"8080","ROLE":"web"}"#),
    )) else {
        panic!("objects are shown line by line");
    };
    let marked: Vec<(&str, Mark)> = lines
        .iter()
        .map(|line| (line.text.as_str(), line.mark))
        .collect();
    assert_eq!(
        marked,
        [
            ("{", Mark::Same),
            ("  \"PORT\": \"80\",", Mark::Removed),
            ("  \"PORT\": \"8080\",", Mark::Added),
            ("  \"ROLE\": \"web\"", Mark::Same),
            ("}", Mark::Same),
        ]
    );
}

#[test]
fn an_added_structure_is_all_additions() {
    let ChangeDisplay::Lines(lines) = ChangeDisplay::of(&change(None, Some("[\"a\"]"))) else {
        panic!("arrays are shown line by line");
    };
    assert!(lines.iter().all(|line| line.mark == Mark::Added));
    assert_eq!(lines.len(), 3);
}
