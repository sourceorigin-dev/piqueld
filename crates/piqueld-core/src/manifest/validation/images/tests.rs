//! One corpus of image references that the parser, its helpers and the
//! `OpenAPI` patterns must all agree on.

use super::ImageParts;
use crate::{ImageReference, ImmutableImage, RepositoryDigest};
use fancy_regex::Regex;
use utoipa::PartialSchema;
use utoipa::openapi::{RefOr, schema::Schema};

/// A 64-digit SHA-256 digest.
const SHA256: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Each reference with the canonical repository it names, or `None` when it
/// is invalid. `{sha256}` stands for [`SHA256`]. A first component with a `.`
/// or `:` is a registry, so `a.b_c/app` is an invalid registry, not a path.
const CORPUS: &[(&str, Option<&str>)] = &[
    ("nginx", Some("docker.io/library/nginx")),
    ("nginx:1.27", Some("docker.io/library/nginx")),
    ("user/app:v_1.2-3", Some("docker.io/user/app")),
    ("index.docker.io/nginx", Some("docker.io/library/nginx")),
    (
        "DoCkEr.Io/library/alpine:3",
        Some("docker.io/library/alpine"),
    ),
    ("GHCR.io/org/app:v1", Some("ghcr.io/org/app")),
    ("LOCALHOST/notes:1", Some("localhost/notes")),
    ("localhost:5000/app@{sha256}", Some("localhost:5000/app")),
    ("registry:5000/team/app", Some("registry:5000/team/app")),
    ("nginx:1.27@{sha256}", Some("docker.io/library/nginx")),
    (
        "team/a.b_c__d---e:_tag",
        Some("docker.io/team/a.b_c__d---e"),
    ),
    (
        "app@sha512:0123456789abcdef0123456789abcdef",
        Some("docker.io/library/app"),
    ),
    ("", None),
    ("Nginx", None),
    ("app:", None),
    (":tag", None),
    ("app name", None),
    ("https://ghcr.io/app", None),
    ("a//b", None),
    ("ghcr.io/", None),
    ("app?tag=1", None),
    ("app#latest", None),
    ("-app", None),
    ("app-", None),
    ("a..b", None),
    ("a.b_c/app", None),
    ("a___b", None),
    ("app:-tag", None),
    ("app@sha256:short", None),
    ("app@SHA256:0123456789abcdef0123456789abcdef", None),
    ("app@{sha256}@{sha256}", None),
    ("ghcr.io:0/app", None),
    ("ghcr.io:65536/app", None),
    ("-ghcr.io/app", None),
    ("[::1]:5000/app", None),
    ("user:pass@ghcr.io/app", None),
];

/// The corpus, with [`SHA256`] substituted, plus references at each length
/// limit and just past it.
fn corpus() -> Vec<(String, Option<String>)> {
    let tag = |len: usize| format!("app:{}", "t".repeat(len));
    let name = |len: usize| format!("a/{}", "b".repeat(len - 2));
    let digest = |len: usize| format!("app@sha512:{}", "a".repeat(len - "app@sha512:".len()));
    CORPUS
        .iter()
        .map(|(value, repository)| {
            (
                value.replace("{sha256}", SHA256),
                repository.map(str::to_owned),
            )
        })
        .chain([
            (tag(128), Some("docker.io/library/app".to_owned())),
            (tag(129), None),
            (name(255), Some(format!("docker.io/{}", name(255)))),
            (name(256), None),
            (digest(512), Some("docker.io/library/app".to_owned())),
            (digest(513), None),
        ])
        .collect()
}

#[test]
fn the_parser_and_its_helpers_agree_with_the_corpus() {
    for (value, repository) in corpus() {
        let parts = ImageParts::parse(&value);
        assert_eq!(
            parts.map(|parts| parts.repository()),
            repository,
            "{value:?}"
        );
        assert_eq!(
            ImageReference::parse(&value).is_ok(),
            parts.is_some(),
            "{value:?}"
        );
        assert_eq!(
            RepositoryDigest::parse(&value).is_ok(),
            parts.is_some_and(|parts| parts.digest == Some(SHA256)),
            "{value:?}"
        );
    }
}

/// The patterns the `OpenAPI` document publishes accept exactly what the
/// parser does.
#[test]
fn the_openapi_patterns_match_the_parser() {
    let pattern = |schema: RefOr<Schema>| {
        let RefOr::T(Schema::Object(object)) = schema else {
            panic!("an image schema is an inline object")
        };
        Regex::new(&object.pattern.expect("an image schema has a pattern"))
            .expect("the pattern compiles")
    };
    let reference = pattern(ImageReference::schema());
    let digest = pattern(RepositoryDigest::schema());
    let immutable = pattern(ImmutableImage::schema());
    let values = corpus()
        .into_iter()
        .map(|(value, _)| value)
        .chain([SHA256.to_owned(), "sha256:abc".to_owned()]);
    for value in values {
        let matches = |regex: &Regex| regex.is_match(&value).expect("the pattern runs");
        assert_eq!(
            matches(&reference),
            ImageReference::parse(&value).is_ok(),
            "{value:?}"
        );
        assert_eq!(
            matches(&digest),
            RepositoryDigest::parse(&value).is_ok(),
            "{value:?}"
        );
        assert_eq!(
            matches(&immutable),
            ImmutableImage::parse(&value).is_ok(),
            "{value:?}"
        );
    }
}
