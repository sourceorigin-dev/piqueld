use super::{DiagnosticCode, OperationError};
use piqueld_docker::DockerError;
use std::error::Error;

#[tokio::test]
async fn command_diagnostics_keep_stage_and_exit_code_without_output() {
    let error = piqueld_docker::command::LoggedCommand::run_recorded(
        tokio::process::Command::new("sh").args(["-c", "printf private-token >&2; exit 7"]),
        "clone Git repository",
        None,
    )
    .await
    .expect_err("run recorded fails");
    let diagnostic = crate::application::BoundaryError::GitBuild(error.into()).diagnostic();
    assert_eq!(
        diagnostic.causes,
        [
            "Command stage: clone Git repository",
            "Command exit code: 7"
        ]
    );
    assert!(
        !serde_json::to_string(&diagnostic)
            .expect("the value serializes")
            .contains("private-token")
    );
}

#[test]
fn operation_errors_retain_causes_without_exposing_them_publicly() {
    let diagnostic = "token=internal-secret";
    let error = OperationError::from(DockerError::RequestSource {
        operation: "inspect service",
        source: Box::new(std::io::Error::other(diagnostic)),
    });
    assert_eq!(error.code(), "docker_request_failed");
    assert!(!error.message().contains(diagnostic));
    let cause = error
        .source()
        .expect("source succeeds")
        .source()
        .expect("source succeeds");
    assert!(cause.is::<std::io::Error>());
    assert_eq!(cause.to_string(), diagnostic);

    let journal = OperationError::from(piqueld_store::StoreError::DatabaseSource(
        sqlx::Error::PoolClosed,
    ));
    assert_eq!(journal.code(), "journal_unavailable");
    assert!(
        journal
            .source()
            .expect("source succeeds")
            .source()
            .expect("source succeeds")
            .is::<sqlx::Error>()
    );

    let git = OperationError::GitBuildFailed(anyhow::anyhow!(diagnostic));
    assert_eq!(git.code(), "git_build_failed");
    assert_eq!(
        git.source().expect("source succeeds").to_string(),
        diagnostic
    );
    assert!(!git.message().contains(diagnostic));
}

#[test]
fn only_a_missing_manifest_file_reads_as_not_found() {
    for (kind, code) in [
        (std::io::ErrorKind::NotFound, "manifest_not_found"),
        (std::io::ErrorKind::PermissionDenied, "manifest_invalid"),
    ] {
        let error = anyhow::Error::from(std::io::Error::from(kind)).context("manifest path");
        assert_eq!(OperationError::manifest_input(error).code(), code);
    }
}

/// `DockerError::diagnostic_code` drives retries, and
/// `docker_classification` the operation's diagnostic; they must agree.
#[test]
fn docker_errors_classify_alike_for_retries_and_diagnostics() {
    let rejected = |status_code| bollard::errors::Error::DockerResponseServerError {
        status_code,
        message: String::new(),
    };
    for error in [
        DockerError::Unavailable("op"),
        DockerError::UnavailableSource {
            operation: "op",
            source: "cause".into(),
        },
        DockerError::NotManager,
        DockerError::IncompatibleSwarm,
        DockerError::OwnershipConflict,
        DockerError::ConfigurationConflict,
        DockerError::Validation("op"),
        DockerError::ImageResolution("op"),
        DockerError::ImageResolutionSource {
            operation: "op",
            source: rejected(401),
        },
        DockerError::ImageResolutionSource {
            operation: "op",
            source: rejected(500),
        },
        DockerError::NetworkInUse(rejected(400)),
        DockerError::NetworkRemoving(rejected(500)),
        DockerError::Request("op"),
        DockerError::RequestSource {
            operation: "op",
            source: "cause".into(),
        },
    ] {
        assert_eq!(
            OperationError::docker_classification(&error).diagnostic_code(),
            error.diagnostic_code(),
            "{error:?}"
        );
    }
}

#[test]
fn registry_failures_keep_classification_and_original_status() {
    for (status_code, expected) in [
        (401, "image_resolution_rejected"),
        (408, "image_resolution_failed"),
        (429, "image_resolution_failed"),
        (500, "image_resolution_failed"),
    ] {
        let error = OperationError::from(DockerError::ImageResolutionSource {
            operation: "pull image",
            source: bollard::errors::Error::DockerResponseServerError {
                status_code,
                message: "internal registry diagnostic".into(),
            },
        });
        assert_eq!(error.code(), expected);
        assert!(
            error
                .source()
                .expect("source succeeds")
                .source()
                .expect("source succeeds")
                .is::<bollard::errors::Error>()
        );
        assert!(!error.message().contains("internal registry diagnostic"));
        let diagnostic = error.diagnostic();
        assert!(
            diagnostic
                .causes
                .contains(&format!("Docker returned HTTP status {status_code}"))
        );
        assert!(
            !serde_json::to_string(&diagnostic)
                .expect("the value serializes")
                .contains("internal registry diagnostic")
        );
    }
}

#[test]
fn preparation_diagnostics_preserve_safe_typed_causes() {
    let error = crate::application::BoundaryError::Runtime(DockerError::ImageResolutionSource {
        operation: "pull image",
        source: bollard::errors::Error::DockerResponseServerError {
            status_code: 401,
            message: "registry-secret".into(),
        },
    });
    let diagnostic = error.diagnostic();
    assert_eq!(diagnostic.code, "image_resolution_rejected");
    assert_eq!(
        diagnostic.causes,
        [
            "Docker operation: pull image",
            "Docker returned HTTP status 401"
        ]
    );
    assert!(
        !serde_json::to_string(&diagnostic)
            .expect("the value serializes")
            .contains("registry-secret")
    );
    let io = crate::application::BoundaryError::GitBuild(
        anyhow::Error::from(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "secret path",
        ))
        .context("repository URL with secret"),
    )
    .diagnostic();
    assert!(io.causes.contains(&"I/O failure: PermissionDenied".into()));
    assert!(
        !serde_json::to_string(&io)
            .expect("the value serializes")
            .contains("secret")
    );
    let storage = OperationError::Journal(piqueld_store::StoreError::DatabaseSource(
        sqlx::Error::PoolClosed,
    ))
    .diagnostic();
    assert!(
        storage
            .causes
            .contains(&"Database connection pool is closed".into())
    );
    let compilation =
        crate::application::BoundaryError::Compilation(vec![piqueld_core::CompileError {
            resource: "web".into(),
            code: piqueld_core::ApiErrorCode::SourceUnresolved.into(),
            message: "service source has not been resolved to an immutable image".into(),
        }])
        .diagnostic();
    assert!(compilation.causes[0].contains("web"));
    assert!(compilation.causes[0].contains(piqueld_core::ApiErrorCode::SourceUnresolved.as_str()));
}

#[test]
fn swarm_bootstrap_errors_keep_distinct_diagnostic_guidance() {
    for (error, expected_code, expected_action) in [
        (
            DockerError::Unavailable("checking Swarm"),
            DiagnosticCode::DockerUnavailable,
            "Check Docker Engine availability",
        ),
        (
            DockerError::NotManager,
            DiagnosticCode::SwarmManagerUnavailable,
            "single-node Swarm",
        ),
        (
            DockerError::IncompatibleSwarm,
            DiagnosticCode::SwarmTopologyUnsupported,
            "single-node Swarm",
        ),
    ] {
        let diagnostic = piqueld_core::observability::Diagnostic::new(
            "bootstrap".into(),
            error.diagnostic_code(),
            error.to_string(),
        );
        assert_eq!(diagnostic.code, expected_code.as_str());
        assert!(diagnostic.next_action.contains(expected_action));
    }
}
