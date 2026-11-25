//! `just boundary`: checks the workspace's dependency boundaries.
//! `piqueld-core` holds the shared contracts, so it must not depend on the
//! server, Docker, database, or browser frameworks. `piqueld-client` builds
//! for wasm32 too, so it must not depend on any other workspace crate but
//! `piqueld-core` and `piqueld-probe`, nor on those frameworks, except for the
//! server framework its native tests use.
//! The daemon's own crates build in parallel only while they don't depend on
//! each other.

use std::process::ExitCode;

use anyhow::{Context, Result, bail};

use crate::Workspace;
use crate::process::checked_stdout;

/// A package that must not depend on any of `forbidden`, through the
/// dependency kinds of `edges`, on any target.
struct Boundary {
    package: &'static str,
    /// `cargo tree --edges` kinds.
    edges: &'static str,
    forbidden: &'static [&'static str],
    /// Why, as the error says when the boundary is crossed.
    reason: &'static str,
}

const BOUNDARIES: [Boundary; 6] = [
    Boundary {
        package: "piqueld-core",
        edges: "all",
        forbidden: &[
            "axum",
            "axum-core",
            "bollard",
            "bollard-stubs",
            "leptos",
            "sqlx",
            "sqlx-core",
        ],
        reason: "holds the shared contracts, without server, Docker, database, or browser frameworks",
    },
    Boundary {
        package: "piqueld-client",
        edges: "all",
        forbidden: &[
            "bollard",
            "bollard-stubs",
            "leptos",
            "sqlx",
            "sqlx-core",
            "piquelctl",
            "piqueld",
            "piqueld-build",
            "piqueld-config",
            "piqueld-docker",
            "piqueld-generated",
            "piqueld-store",
            "piqueld-ui",
            "xtask",
        ],
        reason: "is the published client the CLI and dashboard share, without the other workspace crates or Docker, database, or browser frameworks",
    },
    Boundary {
        package: "piqueld-client",
        edges: "normal,build",
        forbidden: &["axum", "axum-core"],
        reason: "is the published client, which builds for wasm32, so only its tests may use a server framework",
    },
    Boundary {
        package: "piqueld-config",
        edges: "normal,build",
        forbidden: &["bollard", "leptos", "sqlx", "piqueld", "piqueld-store"],
        reason: "builds alongside the daemon's Docker and store crates, before its server",
    },
    Boundary {
        package: "piqueld-store",
        edges: "all",
        forbidden: &["axum", "bollard", "leptos", "piqueld", "piqueld-config"],
        reason: "builds alongside the daemon's configuration and Docker crates, without a server, Docker, or browser framework",
    },
    Boundary {
        package: "piqueld-docker",
        edges: "normal,build",
        forbidden: &[
            "axum",
            "leptos",
            "sqlx",
            "piqueld",
            "piqueld-config",
            "piqueld-store",
        ],
        reason: "builds alongside the daemon's configuration and store crates, without a server, database, or browser framework",
    },
];

pub(crate) fn check(workspace: &Workspace) -> Result<ExitCode> {
    let mut crossed = false;
    for boundary in &BOUNDARIES {
        let output = workspace
            .cargo()
            .args(["tree", "--locked", "--package", boundary.package])
            .args([
                "--edges",
                boundary.edges,
                "--target",
                "all",
                "--prefix",
                "none",
            ])
            .output()
            .context("run cargo tree")?;
        let tree = checked_stdout("cargo tree", output)?;
        let forbidden: Vec<&str> = tree
            .lines()
            .filter(|line| {
                boundary
                    .forbidden
                    .contains(&line.split(' ').next().unwrap_or_default())
            })
            .collect();
        if forbidden.is_empty() {
            println!(
                "{} dependency boundary is intact ({} dependencies)",
                boundary.package, boundary.edges
            );
        } else {
            eprintln!(
                "{} {}, but depends on:\n{}",
                boundary.package,
                boundary.reason,
                forbidden.join("\n")
            );
            crossed = true;
        }
    }
    if crossed {
        bail!("a dependency boundary is crossed");
    }
    Ok(ExitCode::SUCCESS)
}
