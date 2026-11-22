//! The jobs of .github/workflows/ci.yml, as `just check-changed` runs them:
//! the filters that start each, as its `if:` reads the `changes` job's
//! outputs, and the checks it runs. Change this file together with ci.yml.

use std::collections::BTreeSet;
use std::fmt;

use super::filters::Filter;

/// A job in ci.yml.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Job {
    Checks,
    RustLint,
    RustTest,
    Msrv,
    WasmUi,
    DockerIntegration,
    BrowserPlaywright,
    MacosCli,
    Docs,
}

impl Job {
    /// Every job, in the order ci.yml lists them.
    pub(super) const ALL: [Self; 9] = [
        Self::Checks,
        Self::RustLint,
        Self::RustTest,
        Self::Msrv,
        Self::WasmUi,
        Self::DockerIntegration,
        Self::BrowserPlaywright,
        Self::MacosCli,
        Self::Docs,
    ];

    /// Its name, as GitHub shows it.
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Checks => "checks",
            Self::RustLint => "rust (lint)",
            Self::RustTest => "rust (test)",
            Self::Msrv => "msrv",
            Self::WasmUi => "wasm-ui",
            Self::DockerIntegration => "docker-integration",
            Self::BrowserPlaywright => "browser-playwright",
            Self::MacosCli => "cli (macOS)",
            Self::Docs => "docs",
        }
    }

    /// What starts the job on a pull request.
    pub(super) fn trigger(self) -> Trigger {
        Trigger::AnyOf(match self {
            Self::Checks => &[Filter::Rust, Filter::Deny],
            Self::RustLint | Self::RustTest | Self::Msrv => &[Filter::Rust],
            Self::WasmUi => &[Filter::Ui],
            Self::DockerIntegration => &[Filter::Docker],
            Self::BrowserPlaywright => &[Filter::Playwright],
            Self::MacosCli => &[Filter::Cli],
            Self::Docs => return Trigger::EveryChange,
        })
    }

    /// The recipes the job runs, in order.
    pub(super) fn recipes(self) -> &'static [Recipe] {
        match self {
            Self::Checks => &[Recipe::FmtCheck, Recipe::Audit, Recipe::Unused],
            Self::RustLint => &[
                Recipe::Lint,
                Recipe::Doc,
                Recipe::OpenapiCheck,
                Recipe::Boundary,
            ],
            Self::RustTest => &[Recipe::Test, Recipe::DocTest],
            Self::Msrv => &[Recipe::Msrv],
            Self::WasmUi => &[Recipe::UiCheck, Recipe::CheckWasm, Recipe::TestWasm],
            Self::DockerIntegration => &[Recipe::DockerTest],
            Self::BrowserPlaywright => &[Recipe::TestPlaywright, Recipe::DocsScreenshots],
            Self::MacosCli => &[Recipe::ValidateCli],
            Self::Docs => &[Recipe::Docs],
        }
    }
}

/// What starts a job on a pull request.
#[derive(Clone, Copy, Debug)]
pub(super) enum Trigger {
    EveryChange,
    /// A change matching any of these filters.
    AnyOf(&'static [Filter]),
}

impl Trigger {
    /// Whether a change matching `filters` starts the job.
    pub(super) fn fires(self, filters: &BTreeSet<Filter>) -> bool {
        match self {
            Self::EveryChange => true,
            Self::AnyOf(any) => any.iter().any(|filter| filters.contains(filter)),
        }
    }
}

impl fmt::Display for Trigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EveryChange => f.write_str("every change"),
            Self::AnyOf(filters) => {
                let names: Vec<_> = filters.iter().map(ToString::to_string).collect();
                f.write_str(&names.join(" or "))
            }
        }
    }
}

/// A check a CI job runs, named for its `just` recipe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Recipe {
    FmtCheck,
    Audit,
    Unused,
    Lint,
    Doc,
    OpenapiCheck,
    Boundary,
    Test,
    DocTest,
    Msrv,
    UiCheck,
    CheckWasm,
    TestWasm,
    DockerTest,
    TestPlaywright,
    DocsScreenshots,
    ValidateCli,
    Docs,
}

impl Recipe {
    /// The command that runs it on the whole workspace.
    pub(super) fn command(self) -> &'static [&'static str] {
        match self {
            Self::FmtCheck => &["just", "fmt-check"],
            Self::Audit => &["just", "audit"],
            Self::Unused => &["just", "unused"],
            Self::Lint => &["just", "lint"],
            Self::Doc => &["just", "doc"],
            Self::OpenapiCheck => &["just", "openapi-check"],
            Self::Boundary => &["just", "boundary"],
            Self::Test => &["just", "test"],
            Self::DocTest => &["just", "doc-test"],
            // xtask inherits the workspace's `rust-version`.
            Self::Msrv => &[
                "cargo",
                concat!("+", env!("CARGO_PKG_RUST_VERSION")),
                "check",
                "--locked",
                "--package",
                "piqueld",
                "--package",
                "piquelctl",
                "--package",
                "piqueld-client",
            ],
            Self::UiCheck => &["just", "ui-check"],
            Self::CheckWasm => &["just", "check-wasm"],
            Self::TestWasm => &["just", "test-wasm"],
            Self::DockerTest => &["just", "docker-test"],
            Self::TestPlaywright => &["just", "test-playwright"],
            Self::DocsScreenshots => &["just", "docs-screenshots", "--update-snapshots=none"],
            Self::ValidateCli => &["just", "validate-cli"],
            Self::Docs => &["just", "docs"],
        }
    }

    /// How the check narrows to the crates a change reaches.
    pub(super) fn scoping(self) -> Scoping {
        match self {
            Self::Lint | Self::Doc | Self::Test => Scoping::Crates {
                libraries_only: false,
            },
            Self::DocTest => Scoping::Crates {
                libraries_only: true,
            },
            // It checks the files generated from these crates.
            Self::OpenapiCheck => Scoping::Only(&["piqueld", "piquelctl"]),
            Self::FmtCheck
            | Self::Audit
            | Self::Unused
            | Self::Boundary
            | Self::Msrv
            | Self::UiCheck
            | Self::CheckWasm
            | Self::TestWasm
            | Self::DockerTest
            | Self::TestPlaywright
            | Self::DocsScreenshots
            | Self::ValidateCli
            | Self::Docs => Scoping::Workspace,
        }
    }

    /// Why `just check-changed` leaves the check to CI, if it does.
    pub(super) fn left_to_ci(self) -> Option<&'static str> {
        match self {
            Self::Msrv => Some("needs the oldest supported Rust"),
            Self::TestWasm => Some("needs wasm-bindgen-test-runner and a WebDriver"),
            Self::DockerTest | Self::DocsScreenshots => Some("slow, so drafts skip it too"),
            Self::TestPlaywright => {
                Some("slow, so drafts skip it too; needs `just setup-playwright` first")
            }
            Self::ValidateCli => Some("runs on macOS, which drafts skip too"),
            Self::FmtCheck
            | Self::Audit
            | Self::Unused
            | Self::Lint
            | Self::Doc
            | Self::OpenapiCheck
            | Self::Boundary
            | Self::Test
            | Self::DocTest
            | Self::UiCheck
            | Self::CheckWasm
            | Self::Docs => None,
        }
    }
}

/// How a check narrows to the crates a change reaches.
#[derive(Clone, Copy, Debug)]
pub(super) enum Scoping {
    /// It runs on the whole workspace, whatever changed.
    Workspace,
    /// It takes the crates as `--package=<crate>` arguments, only those with
    /// a library when `libraries_only`.
    Crates { libraries_only: bool },
    /// It runs only when the change reaches one of these crates.
    Only(&'static [&'static str]),
}
