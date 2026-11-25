//! `cargo metadata`, reduced to what xtask reads of the workspace's crates.

use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::Deserialize;

use crate::Workspace;

/// The workspace's crates, as `cargo metadata --no-deps` lists them.
#[derive(Deserialize)]
pub(crate) struct Metadata {
    pub packages: Vec<Package>,
}

/// A workspace crate.
#[derive(Deserialize)]
pub(crate) struct Package {
    pub name: String,
    pub manifest_path: PathBuf,
    /// Of every kind: normal, development and build.
    pub dependencies: Vec<Dependency>,
    pub targets: Vec<Target>,
}

/// A crate a package depends on, named as its package is.
#[derive(Deserialize)]
pub(crate) struct Dependency {
    pub name: String,
}

/// A package's library, binary, test, example, benchmark or build script.
#[derive(Deserialize)]
pub(crate) struct Target {
    /// Such as `lib`, `rlib`, `bin` or `test`.
    pub kind: Vec<String>,
}

impl Workspace {
    pub(crate) fn metadata(&self) -> Result<Metadata> {
        self.cargo_json(&["metadata", "--locked", "--no-deps", "--format-version", "1"])
    }
}

impl Metadata {
    /// The crate whose directory holds `file`, an absolute path: the deepest,
    /// since a crate's directory may contain another's.
    pub(crate) fn package_of(&self, file: &Path) -> Option<&Package> {
        self.packages
            .iter()
            .filter_map(|package| Some((package, package.manifest_path.parent()?)))
            .filter(|(_, dir)| file.starts_with(dir))
            .max_by_key(|(_, dir)| dir.components().count())
            .map(|(package, _)| package)
    }
}

impl Package {
    /// Whether the crate has a library, which doc tests need.
    pub(crate) fn has_library(&self) -> bool {
        self.targets
            .iter()
            .flat_map(|target| &target.kind)
            .any(|kind| {
                matches!(
                    kind.as_str(),
                    "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro"
                )
            })
    }
}
