//! Nested Cargo builds, for the build scripts of piqueld's crates that embed
//! an artifact compiled for another target: the daemon's dashboard (wasm32)
//! and the Docker crate's health probe (musl).

use std::{
    env,
    error::Error as _,
    fmt, io,
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
};

/// `path` as UTF-8, as the generated `include_*!` calls that name it need.
///
/// # Errors
///
/// Returns an error if `path` is not valid UTF-8.
pub fn utf8(path: &Path) -> io::Result<&str> {
    path.to_str()
        .ok_or_else(|| io::Error::other(format!("path is not valid UTF-8: {}", path.display())))
}

/// Why a nested build could not run or failed, with the paths, package and
/// target a person needs to fix it.
#[derive(thiserror::Error)]
pub enum BuildError {
    /// Cargo did not set a variable it gives every build script.
    #[error("Cargo did not provide {0} to the build script")]
    MissingVariable(&'static str),
    /// `OUT_DIR` can't be resolved, such as when it was removed.
    #[error("failed to resolve the build script's OUT_DIR {}", .out_dir.display())]
    OutDir {
        /// `OUT_DIR` as Cargo gave it.
        out_dir: PathBuf,
        /// Why it can't be resolved.
        #[source]
        source: io::Error,
    },
    /// `OUT_DIR` is not under `<target dir>/<profile>/build`, so the target
    /// directory can't be derived from it.
    #[error(
        "the build script's OUT_DIR {} is not under <target dir>/<profile>/build, so the nested build has no target directory beside it",
        .0.display()
    )]
    OutDirLayout(PathBuf),
    /// The target directory derived from `OUT_DIR` does not exist.
    #[error("the Cargo target directory {}, derived from OUT_DIR, does not exist", .0.display())]
    MissingTargetDir(PathBuf),
    /// The nested build's directory has no `Cargo.lock` in or above it.
    #[error("no Cargo.lock in {} or any directory above it", .0.display())]
    NoLockfile(PathBuf),
    /// The nested Cargo could not start.
    #[error("failed to run cargo to build {package} from {}", .dir.display())]
    Spawn {
        /// The package it was to build.
        package: String,
        /// The directory it was to run in.
        dir: PathBuf,
        /// Why it could not start.
        #[source]
        source: io::Error,
    },
    /// The nested Cargo ran and failed; `output` is what it printed.
    #[error(
        "cargo failed to build {package} for {target} from {} ({status}); is the {target} target installed (`rustup target add {target}`)?\n{output}",
        .dir.display()
    )]
    Failed {
        /// The package it built.
        package: String,
        /// The target triple it built for.
        target: String,
        /// The directory it ran in.
        dir: PathBuf,
        /// How it exited.
        status: ExitStatus,
        /// Its standard output, then its standard error.
        output: String,
    },
}

// A build script's `main` prints the error it returns with `Debug`, so this
// shows the message and its causes, rather than escaped fields.
impl fmt::Debug for BuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self}")?;
        let mut source = self.source();
        while let Some(cause) = source {
            write!(formatter, "\ncaused by: {cause}")?;
            source = cause.source();
        }
        Ok(())
    }
}

/// A nested build into its own target directory beside the outer one.
///
/// The nested Cargo must never share this build's target directory: the outer
/// Cargo holds its build-directory lock until the build script returns, so
/// sharing would deadlock. The nested Cargo joins the outer build's jobserver
/// (`CARGO_MAKEFLAGS`), so both share one limit on parallel jobs.
///
/// The outer rustflags, rustc wrappers, and profile overrides are stripped
/// because they describe the host build: host rustflags such as
/// `-C target-cpu=native` or linker arguments break a wasm32 build, and would
/// also override the nested target's own rustflags
/// (`CARGO_TARGET_<TRIPLE>_RUSTFLAGS`, `target.<triple>.rustflags`), which
/// still apply; `cargo clippy` sets `RUSTC_WORKSPACE_WRAPPER`. The nested
/// profile alone defines the rest.
pub struct NestedCargo {
    /// `<outer target dir>-<suffix>`, e.g. `target-ui`.
    target_dir: PathBuf,
}

impl NestedCargo {
    /// Prepares a nested build into `<outer target dir>-<suffix>`, for the
    /// build script running it.
    ///
    /// # Errors
    /// Cargo gave the build script no `OUT_DIR`, or its target directory
    /// can't be derived from it.
    pub fn new(suffix: &str) -> Result<Self, BuildError> {
        let out_dir = env::var_os("OUT_DIR").ok_or(BuildError::MissingVariable("OUT_DIR"))?;
        Ok(Self {
            target_dir: Self::sibling_target_dir(Path::new(&out_dir), suffix)?,
        })
    }

    /// Builds `package` with `profile` for `target` from `dir`, and returns the
    /// directory holding its artifacts. `config` is a Cargo configuration file
    /// passed with `--config`, such as one that defines `profile`.
    ///
    /// Reruns the build script when `dir`, `config`, or the manifest and
    /// lockfile it builds against change. These are the workspace's when `dir`
    /// is a workspace member, and the package's own when it is a published
    /// crate.
    ///
    /// # Errors
    /// The nested Cargo can't run or fails, such as when the target isn't
    /// installed.
    #[expect(
        clippy::print_stdout,
        reason = "a build script talks to Cargo on stdout"
    )]
    pub fn build(
        &self,
        dir: &Path,
        package: &str,
        profile: &str,
        target: &str,
        config: Option<&Path>,
    ) -> Result<PathBuf, BuildError> {
        const HOST_SETTINGS: [&str; 6] = [
            "RUSTFLAGS",
            "RUSTDOCFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "CARGO_BUILD_RUSTFLAGS",
            "RUSTC_WRAPPER",
            "RUSTC_WORKSPACE_WRAPPER",
        ];
        let root = dir
            .ancestors()
            .find(|ancestor| ancestor.join("Cargo.lock").is_file())
            .ok_or_else(|| BuildError::NoLockfile(dir.to_owned()))?;
        let inputs = [
            dir.to_owned(),
            root.join("Cargo.toml"),
            root.join("Cargo.lock"),
        ];
        for input in inputs.iter().map(PathBuf::as_path).chain(config) {
            println!("cargo:rerun-if-changed={}", input.display());
        }
        let cargo = env::var_os("CARGO").ok_or(BuildError::MissingVariable("CARGO"))?;
        println!(
            "cargo:warning=building embedded {package} (nested target dir: {})",
            self.target_dir.display()
        );
        let mut command = Command::new(cargo);
        command
            .current_dir(dir)
            .args(["build", "--locked", "--package", package])
            .args(["--profile", profile, "--target", target])
            .env("CARGO_TARGET_DIR", &self.target_dir);
        if let Some(config) = config {
            command.arg("--config").arg(config);
        }
        for (key, _) in env::vars_os() {
            if key.to_str().is_some_and(|key| {
                HOST_SETTINGS.contains(&key) || key.starts_with("CARGO_PROFILE_")
            }) {
                command.env_remove(key);
            }
        }
        let output = command.output().map_err(|source| BuildError::Spawn {
            package: package.to_owned(),
            dir: dir.to_owned(),
            source,
        })?;
        if !output.status.success() {
            return Err(BuildError::Failed {
                package: package.to_owned(),
                target: target.to_owned(),
                dir: dir.to_owned(),
                status: output.status,
                output: format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                ),
            });
        }
        Ok(self.target_dir.join(target).join(profile))
    }

    /// Derives `<target dir>-<suffix>` from the canonical `OUT_DIR`, which is
    /// `<target>/<profile>/build/<pkg>/<hash>/out` for the pinned Cargo and
    /// `<target>/<profile>/build/<pkg>-<hash>/out` for older ones, with a
    /// `<triple>/` before `<profile>` when building for a `--target`: the
    /// directory two levels above the nearest `build` directory is the target
    /// directory, or `<target>/<triple>`, so no package that builds nested may
    /// be named `build`.
    fn sibling_target_dir(out_dir: &Path, suffix: &str) -> Result<PathBuf, BuildError> {
        let canonical = out_dir
            .canonicalize()
            .map_err(|source| BuildError::OutDir {
                out_dir: out_dir.to_owned(),
                source,
            })?;
        let target_dir = canonical
            .ancestors()
            .find(|ancestor| ancestor.file_name() == Some("build".as_ref()))
            .and_then(|build| build.ancestors().nth(2))
            .ok_or_else(|| BuildError::OutDirLayout(canonical.clone()))?;
        if !target_dir.is_dir() {
            return Err(BuildError::MissingTargetDir(target_dir.to_owned()));
        }
        let mut name = target_dir
            .file_name()
            .ok_or_else(|| BuildError::OutDirLayout(canonical.clone()))?
            .to_owned();
        name.push(format!("-{suffix}"));
        Ok(target_dir.with_file_name(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The nested target directory sits beside the outer one, named after it,
    /// with either `OUT_DIR` layout.
    #[test]
    fn nests_beside_the_outer_target_directory() {
        for layout in [
            "target/debug/build/piqueld/0123/out",
            "target/debug/build/piqueld-0123/out",
        ] {
            let root = tempfile::tempdir().unwrap();
            let out_dir = root.path().join(layout);
            std::fs::create_dir_all(&out_dir).unwrap();
            let nested = NestedCargo::sibling_target_dir(&out_dir, "ui").unwrap();
            assert_eq!(
                nested,
                root.path().canonicalize().unwrap().join("target-ui"),
                "{layout}"
            );
        }
    }

    /// An `OUT_DIR` with no `build` directory above it has no target directory.
    #[test]
    fn rejects_an_out_dir_outside_a_build_directory() {
        let error = NestedCargo::sibling_target_dir(Path::new("/"), "ui").unwrap_err();
        assert!(matches!(error, BuildError::OutDirLayout(_)), "{error:?}");
    }

    /// What a failing build script prints names the problem and its cause.
    #[test]
    fn debug_output_reads_like_the_message_and_its_causes() {
        let error = BuildError::Spawn {
            package: "piqueld-probe".into(),
            dir: "/src/probe".into(),
            source: io::Error::new(io::ErrorKind::NotFound, "No such file or directory"),
        };
        assert_eq!(
            format!("{error:?}"),
            "failed to run cargo to build piqueld-probe from /src/probe\n\
             caused by: No such file or directory"
        );
    }
}
