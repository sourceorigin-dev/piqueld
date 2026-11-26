use super::*;
use crate::{Sha256Digest, manifest::ValidatedSource};
use std::collections::BTreeMap;

fn pulled(digest: char) -> ResolvedSource {
    ResolvedSource::parse_image(
        "example.com/web:1",
        format!("example.com/web@sha256:{}", digest.to_string().repeat(64)),
    )
    .unwrap()
}

fn built(id: char) -> ResolvedSource {
    ResolvedSource::Git {
        requested: ValidatedSource::Image {
            image: "unused".into(),
        },
        commit: "a".repeat(40),
        image_id: Sha256Digest::parse(format!("sha256:{}", id.to_string().repeat(64)))
            .unwrap()
            .into(),
    }
}

#[test]
fn missing_registry_images_are_pullable_and_missing_builds_unavailable() {
    let (web, api) = (
        ServiceName::parse("web").unwrap(),
        ServiceName::parse("api").unwrap(),
    );
    let sources = BTreeMap::from([(web.clone(), pulled('a')), (api.clone(), built('b'))]);
    let all = LocalImages::new(sources.values().map(ResolvedSource::image));
    assert_eq!(all.availability(&sources), ReleaseAvailability::Present);

    let built_only = LocalImages::new([built('b').image()]);
    assert_eq!(
        built_only.availability(&sources),
        ReleaseAvailability::Pullable {
            missing: vec![ServiceImage {
                service: web.clone(),
                image: pulled('a').image()
            }]
        }
    );

    let ReleaseAvailability::Unavailable { missing } =
        LocalImages::default().availability(&sources)
    else {
        panic!("a missing build makes the release unavailable");
    };
    assert_eq!(
        missing.iter().map(|m| &m.service).collect::<Vec<_>>(),
        [&api, &web]
    );

    let mut pushed = built('b');
    if let ResolvedSource::Git { image_id, .. } = &mut pushed {
        *image_id = pulled('c').repository_digest().unwrap().clone().into();
    }
    assert!(matches!(
        LocalImages::default().availability([(&api, &pushed)]),
        ReleaseAvailability::Pullable { .. }
    ));
}

/// A plan of a release that can't be deployed again is blocked, like
/// the promotion; one whose images can be pulled again is not.
#[test]
fn only_unavailable_releases_block_plans() {
    let web = ServiceName::parse("web").unwrap();
    let sources = BTreeMap::from([(web, built('b'))]);
    let diagnostics = LocalImages::default().availability(&sources).diagnostics();
    assert_eq!(
        diagnostics
            .iter()
            .map(|diagnostic| (diagnostic.code.as_str(), diagnostic.resource.as_str()))
            .collect::<Vec<_>>(),
        [(ApiErrorCode::ImageUnavailable.as_str(), "web")]
    );
    assert!(diagnostics[0].blocking);
    let pullable = ReleaseAvailability::Pullable {
        missing: Vec::new(),
    };
    assert_eq!(pullable.diagnostics(), []);
    assert_eq!(ReleaseAvailability::Present.diagnostics(), []);
}
