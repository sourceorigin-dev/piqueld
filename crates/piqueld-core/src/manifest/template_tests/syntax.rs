use super::*;

#[test]
fn references_allow_whitespace_and_escapes_while_shell_syntax_stays_text() {
    let segments = |text: &str| {
        Template::from(text)
            .segments()
            .expect("the template segments")
    };
    let domain = Segment::Reference(Reference::Variable("domain".into()));
    assert_eq!(segments("${{vars.domain}}"), std::slice::from_ref(&domain));
    assert_eq!(
        segments("${{  vars.domain \t}}"),
        std::slice::from_ref(&domain)
    );
    assert_eq!(
        segments("api.${{ vars.domain }}!"),
        [
            Segment::Text("api.".into()),
            domain,
            Segment::Text("!".into())
        ]
    );
    assert_eq!(
        segments("${HOME} $USER $${{ vars.domain }}"),
        [Segment::Text("${HOME} $USER ${{ vars.domain }}".into())]
    );
    assert_eq!(
        segments("${{ env.slug }}"),
        [Segment::Reference(Reference::System(
            SystemVariable::EnvSlug
        ))]
    );
    for (text, code) in [
        ("${{ vars.domain", ApiErrorCode::TemplateInvalid),
        ("${{ domain }}", ApiErrorCode::TemplateInvalid),
        ("${{ vars. }}", ApiErrorCode::TemplateInvalid),
        ("${{ secrets.token }}", ApiErrorCode::VariableReserved),
        ("${{ var.domain }}", ApiErrorCode::VariableNamespaceUnknown),
        ("${{ env.region }}", ApiErrorCode::VariableUnknown),
    ] {
        assert_eq!(
            Template::from(text)
                .segments()
                .expect_err("the template is rejected")
                .code(),
            code,
            "{text}"
        );
    }
    let literal = Template::literal("${{ vars.domain }}");
    assert_eq!(literal.as_str(), "$${{ vars.domain }}");
    assert_eq!(literal.as_literal().as_deref(), Some("${{ vars.domain }}"));
}

#[test]
fn variable_text_round_trips_blank_and_numeric_looking_strings() {
    for value in [
        Variable::String("".into()),
        Variable::String("  ".into()),
        Variable::String("3".into()),
        Variable::String("true".into()),
        Variable::String("\"quoted\"".into()),
        Variable::String("piquel.fr".into()),
        Variable::Integer(3),
        Variable::Boolean(false),
    ] {
        let text = value.to_text();
        assert!(!text.trim().is_empty(), "{value:?} would read as no value");
        assert_eq!(Variable::from_text(&text), value, "{text}");
    }
}

#[test]
fn parser_preserves_dollar_edges_and_adjacent_references() {
    let reference = Segment::Reference(Reference::Variable("a".into()));
    for (source, expected) in [
        ("", vec![]),
        ("$", vec![Segment::Text("$".into())]),
        ("a$$b}}", vec![Segment::Text("a$$b}}".into())]),
        (
            "${{vars.a}}${{vars.a}}",
            vec![reference.clone(), reference.clone()],
        ),
        (
            "$${{vars.a}}${{vars.a}}",
            vec![Segment::Text("${{vars.a}}".into()), reference],
        ),
    ] {
        assert_eq!(
            Template::from(source)
                .segments()
                .expect("the template segments"),
            expected,
            "{source}"
        );
    }
    assert_eq!(
        Template::from("prefix ${{vars.a").segments(),
        Err(TemplateError::Unterminated)
    );
    for source in [
        "",
        "$",
        "$$",
        "${HOME} $USER",
        "${{",
        "$${{",
        "$$${{vars.a}}",
        "é${{vars.a}}${{env.name}}}}",
    ] {
        assert_eq!(
            Template::literal(source).as_literal().as_deref(),
            Some(source)
        );
    }
    for source in ["${{vars.a}}", "${{bad}}", "${{"] {
        assert_eq!(Template::from(source).as_literal(), None);
    }
}

#[test]
fn identifiers_and_reference_errors_have_bounded_precise_diagnostics() {
    use super::super::variables::valid_identifier;
    for (name, valid) in [
        ("_", true),
        ("A_b-9", true),
        ("a-", true),
        ("", false),
        ("1a", false),
        ("-a", false),
        ("é", false),
        ("a.b", false),
        ("a b", false),
    ] {
        assert_eq!(valid_identifier(name), valid, "{name}");
        let parsed = Template::from(format!("${{{{vars.{name}}}}}")).segments();
        assert_eq!(parsed.is_ok(), valid, "{name}");
    }
    assert!(valid_identifier(&"a".repeat(63)));
    assert!(!valid_identifier(&"a".repeat(64)));
    for (source, error, message) in [
        (
            "${{",
            TemplateError::Unterminated,
            "`${{` must be closed by `}}`; write `$${{` for a literal `${{`",
        ),
        (
            "${{a}}",
            TemplateError::Malformed("a".into()),
            "`a` is not a `namespace.name` reference",
        ),
        (
            "${{secrets.x}}",
            TemplateError::Reserved("secrets.x".into()),
            "`secrets.x` is reserved for secret references, which are not supported yet",
        ),
        (
            "${{wat.x}}",
            TemplateError::UnknownNamespace("wat".into()),
            "unknown namespace `wat`; references use vars, app, env, git, or deployment",
        ),
        (
            "${{app.x}}",
            TemplateError::UnknownSystemVariable("app.x".into()),
            "unknown system variable `app.x`",
        ),
    ] {
        assert_eq!(
            Template::from(source)
                .segments()
                .expect_err("the template is rejected"),
            error
        );
        assert_eq!(error.to_string(), message);
    }
    let long = "é".repeat(100);
    assert_eq!(
        Template::from(format!("${{{{{long}}}}}")).segments(),
        Err(TemplateError::Malformed("é".repeat(64)))
    );
    for inner in ["vars.a.b", ".a", "1vars.a", "vars.a b", "", "vars.a + 1"] {
        assert_eq!(
            Template::from(format!("${{{{{inner}}}}}"))
                .segments()
                .expect_err("the template is rejected")
                .code(),
            ApiErrorCode::TemplateInvalid
        );
    }
    for variable in SystemVariable::ALL {
        let reference = Reference::System(variable);
        assert_eq!(reference.to_string(), variable.as_str());
        assert_eq!(
            Template::from(format!("${{{{ {} }}}}", variable.as_str()))
                .segments()
                .expect("the template segments"),
            [Segment::Reference(reference)]
        );
    }
    assert_eq!(
        Reference::Variable("a_b-9".into()).to_string(),
        "vars.a_b-9"
    );
}

#[test]
fn template_wire_and_comparisons_preserve_the_raw_spelling() {
    let raw = "$${{vars.a}}";
    let template = Template::from(raw.to_owned());
    assert_eq!(template.to_string(), raw);
    assert_eq!(template, raw);
    assert!(<Template as PartialEq<str>>::eq(&template, raw));
    assert_ne!(template, "${{vars.a}}");
    assert_eq!(
        serde_json::to_value(&template).expect("the value serializes"),
        json!(raw)
    );
    assert_eq!(
        serde_json::from_value::<Template>(json!(raw)).expect("the template deserializes"),
        template
    );
    serde_json::from_value::<Template>(json!(1))
        .expect_err("`serde_json::from_value` should be rejected");
    assert_eq!(Template::default().as_literal().as_deref(), Some(""));
    for (text, mentions) in [
        (raw, true),
        ("${{", true),
        ("$VAR ${VAR}", false),
        ("", false),
    ] {
        assert_eq!(Template::mentions_reference(text), mentions);
    }
}

#[test]
fn typed_parsing_and_slots_preserve_types_and_failed_assignments() {
    use super::super::variables::{Slot, TypedSlot};
    let mut number = Typed::from(7_u16);
    assert_eq!(number.literal(), Some(&7));
    assert_eq!(number.to_string(), "7");
    assert_eq!("7".parse::<Typed<u16>>().expect("the typed parses"), number);
    "bad"
        .parse::<Typed<u16>>()
        .expect_err("`parse` should be rejected");
    assert!(Slot::Typed(&mut number).template().is_none());
    for raw in ["${{ vars.n }}", "$${{vars.n}}", "${{"] {
        let mut typed: Typed<u16> = raw.parse().expect("the input parses");
        assert_eq!(typed.literal(), None);
        assert_eq!(typed.to_string(), raw);
        assert_eq!(
            serde_json::from_value::<Typed<u16>>(json!(raw)).expect("the typed deserializes"),
            typed
        );
        assert_eq!(
            serde_json::to_value(&typed).expect("the value serializes"),
            json!(raw)
        );
        assert_eq!(
            Slot::Typed(&mut typed)
                .template()
                .expect("the template is built")
                .as_str(),
            raw
        );
        assert!(typed.set(json!("7")).is_err());
        assert_eq!(typed.to_string(), raw);
        typed.set(json!(9)).expect("the value sets");
        assert_eq!(typed.literal(), Some(&9));
    }
    assert_eq!(
        serde_json::from_value::<Typed<u16>>(json!(7)).expect("the typed deserializes"),
        number
    );
    for value in [
        json!("7"),
        json!(true),
        json!(-1),
        json!(70000),
        Value::Null,
        json!([]),
        json!({}),
    ] {
        serde_json::from_value::<Typed<u16>>(value)
            .expect_err("`serde_json::from_value` should be rejected");
    }
    let mut text = Template::from("hello");
    assert_eq!(
        Slot::Text(&mut text)
            .template()
            .expect("the template is built")
            .as_str(),
        "hello"
    );
}

#[test]
fn variable_values_round_trip_native_types_and_interpolate_as_text() {
    for (value, wire, display) in [
        (VariableValue::Boolean(false), json!(false), "false"),
        (VariableValue::Integer(-42), json!(-42), "-42"),
        (VariableValue::String("42".into()), json!("42"), "42"),
    ] {
        assert_eq!(
            serde_json::to_value(&value).expect("the value serializes"),
            wire
        );
        assert_eq!(
            serde_json::from_value::<VariableValue>(wire).expect("the variable value deserializes"),
            value
        );
        assert_eq!(value.to_string(), display);
    }
    for value in [Value::Null, json!(1.2), json!([]), json!({})] {
        serde_json::from_value::<VariableValue>(value)
            .expect_err("`serde_json::from_value` should be rejected");
    }
}

#[test]
fn typed_template_schema_describes_reference_text() {
    use utoipa::PartialSchema;
    let schema = serde_json::to_value(Typed::<u16>::schema()).expect("the value serializes");
    assert!(schema.to_string().contains("TypedTemplate"));
    let mut schemas = Vec::new();
    <Typed<u16> as ToSchema>::schemas(&mut schemas);
    let schema = schemas
        .into_iter()
        .find(|(name, _)| name == "TypedTemplate")
        .expect("the item is found")
        .1;
    assert_eq!(
        serde_json::to_value(schema).expect("the value serializes")["pattern"],
        r"\$\{\{"
    );
}
