use super::*;

#[test]
fn switching_the_generator_resets_its_size_and_keeps_the_name() {
    let mut draft = DeclarationDraft {
        name: "signing-key".into(),
        rsa: false,
        size: "64".into(),
        encoding: SecretEncoding::Base64url,
    };
    draft.set_kind(true);
    assert_eq!(
        (draft.name.as_str(), draft.size.as_str()),
        ("signing-key", "3072")
    );
    draft.size = "4096".into();
    draft.set_kind(true);
    assert_eq!(draft.size, "4096");
    draft.set_kind(false);
    assert_eq!(
        draft,
        DeclarationDraft {
            name: "signing-key".into(),
            ..DeclarationDraft::default()
        }
    );
}

#[test]
fn a_size_that_does_not_parse_is_reported_for_its_secret_before_saving() {
    for size in ["", "3.5", "-1", "65536"] {
        let draft = DeclarationDraft {
            name: "token".into(),
            size: size.into(),
            ..DeclarationDraft::default()
        };
        assert_eq!(
            draft.declaration(),
            Err("The size of token must be a whole number".into()),
            "{size:?}"
        );
    }
}
