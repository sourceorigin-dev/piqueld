//! The jobs of .github/workflows/ci.yml, as `just check-changed` runs them:
//! the filters that start each, as its `if:` reads the `changes` job's
//! outputs, and the checks it runs. Change this file together with ci.yml.

use std::collections::BTreeSet;
use std::fmt;

use super::filters::Filter;

/// A job in ci.yml.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Job {
    /// The `checks` job: formatting, workflow lints, the dependency audit and unused dependencies.
    Checks,
    /// The `standard` job: the engineering standard's rules.
    Standard,
    /// The `rust (lint)` job: clippy, rustdoc, the generated-file check and the boundaries.
    RustLint,
    /// The `rust (test)` job: unit and doc tests.
    RustTest,
    /// The `msrv` job: a check on the oldest supported Rust.
    Msrv,
    /// The `wasm-ui` job: the dashboard and client built for wasm32.
    WasmUi,
    /// The `docker-integration` job: the Docker integration tests.
    DockerIntegration,
    /// The `browser-playwright` job: the browser suite and the docs screenshots.
    BrowserPlaywright,
    /// The `cli (macOS)` job: the CLI's validation on macOS.
    MacosCli,
    /// The `docs` job: the documentation book.
    Docs,
}

impl Job {
    /// Every job, in the order ci.yml lists them.
    pub(super) const ALL: [Self; 10] = [
        Self::Checks,
        Self::Standard,
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
            Self::Standard => "standard",
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
            Self::Checks => &[Filter::Rust, Filter::Deny, Filter::Scripts],
            Self::RustLint | Self::RustTest | Self::Msrv => &[Filter::Rust],
            Self::WasmUi => &[Filter::Ui],
            Self::DockerIntegration => &[Filter::Docker],
            Self::BrowserPlaywright => &[Filter::Playwright],
            Self::MacosCli => &[Filter::Cli],
            Self::Standard | Self::Docs => return Trigger::EveryChange,
        })
    }

    /// The recipes the job runs, in order.
    pub(super) fn recipes(self) -> &'static [Recipe] {
        match self {
            Self::Checks => &[
                Recipe::FmtCheck,
                Recipe::LintCi,
                Recipe::Audit,
                Recipe::Unused,
            ],
            Self::Standard => &[Recipe::Standard],
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
    /// Every pull request starts the job.
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
    /// `just fmt-check`: formatting.
    FmtCheck,
    /// `just lint-ci`: lints of the workflows and shell scripts.
    LintCi,
    /// `just audit`: advisories, licenses, bans and sources.
    Audit,
    /// `just unused`: dependencies a crate declares but never names.
    Unused,
    /// `just standard`: the engineering standard's rules.
    Standard,
    /// `just lint`: clippy with warnings denied.
    Lint,
    /// `just doc`: rustdoc, private items included.
    Doc,
    /// `just openapi-check`: the generated files against freshly generated ones.
    OpenapiCheck,
    /// `just boundary`: the crates' dependency boundaries.
    Boundary,
    /// `just test`: the native tests.
    Test,
    /// `just doc-test`: the doc tests.
    DocTest,
    /// `cargo check` on the oldest supported Rust.
    Msrv,
    /// `just ui-check`: the dashboard and client type-checked for wasm32.
    UiCheck,
    /// `just check-wasm`: the browser transport compiled.
    CheckWasm,
    /// `just test-wasm`: the transport tests in a headless browser.
    TestWasm,
    /// `just docker-test`: the Docker integration tests.
    DockerTest,
    /// `just test-playwright`: the browser suite.
    TestPlaywright,
    /// `just docs-screenshots`: the dashboard screenshots, checked against the committed ones.
    DocsScreenshots,
    /// `just validate-cli`: the CLI's validation.
    ValidateCli,
    /// `just docs`: the documentation book, built and link-checked.
    Docs,
}

impl Recipe {
    /// The command that runs it on the whole workspace.
    pub(super) fn command(self) -> &'static [&'static str] {
        match self {
            Self::FmtCheck => &["just", "fmt-check"],
            Self::LintCi => &["just", "lint-ci"],
            Self::Audit => &["just", "audit"],
            Self::Unused => &["just", "unused"],
            Self::Standard => &["just", "standard"],
            Self::Lint => &["just", "lint"],
            Self::Doc => &["just", "doc"],
            Self::OpenapiCheck => &["just", "openapi-check"],
            Self::Boundary => &["just", "boundary"],
            Self::Test => &["just", "test"],
            Self::DocTest => &["just", "doc-test"],
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
            Self::OpenapiCheck => Scoping::Only(&["piqueld", "piquelctl"]),
            Self::FmtCheck
            | Self::LintCi
            | Self::Audit
            | Self::Unused
            | Self::Standard
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
            | Self::LintCi
            | Self::Audit
            | Self::Unused
            | Self::Standard
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
    Crates {
        /// Whether only the crates with a library count.
        libraries_only: bool,
    },
    /// It runs only when the change reaches one of these crates.
    Only(&'static [&'static str]),
}
