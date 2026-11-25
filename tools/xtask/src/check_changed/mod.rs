//! `just check-changed`: the checks CI's quick jobs run, for only what changed
//! since the merge base with `origin/main`, counting uncommitted and untracked
//! files. It reads `.github/filters.yml`, the path filters of CI's `changes`
//! job, so the two agree on what a change affects, and `ci.rs` maps the
//! filters to the jobs they start. Within the Rust jobs, the checks that take
//! crates run on the changed crates and the crates depending on them. The
//! slow jobs, which drafts skip too, are left to CI: it prints those the
//! change starts, with the command that runs each.

mod ci;
mod filters;
#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::fmt;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context, Result};

use crate::Workspace;
use crate::metadata::Metadata;
use ci::{Job, Recipe, Scoping};
use filters::{Filter, Filters};

/// Crates that build another without depending on it, which `cargo metadata`
/// doesn't show: the daemon's build script compiles the dashboard it embeds.
const BUILDS: [(&str, &str); 1] = [("piqueld", "piqueld-ui")];

/// Runs the checks the change since the merge base of HEAD and `base` needs,
/// and prints a summary of each check CI could run.
pub(crate) fn run(workspace: &Workspace, base: &str) -> Result<ExitCode> {
    let filters = Filters::load(workspace.root())?;
    let (merge_base, paths) = workspace.changed_paths(base)?;
    if paths.is_empty() {
        println!("Nothing changed since {merge_base}, the merge base with {base}.");
        return Ok(ExitCode::SUCCESS);
    }
    let change = Change::new(&filters, &workspace.metadata()?, workspace.root(), &paths);
    let matched: Vec<_> = change.filters.iter().map(ToString::to_string).collect();
    println!(
        "{} paths changed since {merge_base}, the merge base with {base}, matching: {}.",
        paths.len(),
        matched.join(", ")
    );
    println!("Crates: {}.", change.scope);

    let mut steps = Vec::new();
    for job in Job::ALL {
        for &recipe in job.recipes() {
            let mut step = change.plan(job, recipe);
            step.run(workspace)?;
            steps.push(step);
        }
    }

    println!("\nSummary:");
    for step in &steps {
        println!("{step}");
    }
    if steps
        .iter()
        .any(|step| matches!(step.outcome, Outcome::LeftToCi(_)))
    {
        println!(
            "\nCI runs the checks left to it once the pull request is ready for review. \
             Run one here, with its command above, when the change risks breaking it."
        );
    }
    Ok(
        if steps.iter().any(|step| step.outcome == Outcome::Failed) {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        },
    )
}

impl Workspace {
    /// The merge base of HEAD and `base`, and the paths changed since:
    /// committed, staged, unstaged, and untracked but not ignored. A renamed
    /// file counts under both paths, as paths-filter counts it.
    fn changed_paths(&self, base: &str) -> Result<(String, BTreeSet<String>)> {
        let merge_base =
            Self::git(&self.root, &["merge-base", base, "HEAD"]).with_context(|| {
                format!("find the merge base of HEAD and {base}: fetch it, or name another branch")
            })?;
        let diff = Self::git(
            &self.root,
            &["diff", "--name-only", "--no-renames", "-z", &merge_base],
        )?;
        let untracked = Self::git(
            &self.root,
            &["ls-files", "--others", "--exclude-standard", "-z"],
        )?;
        let paths = diff
            .split('\0')
            .chain(untracked.split('\0'))
            .filter(|path| !path.is_empty())
            .map(str::to_owned)
            .collect();
        Ok((merge_base, paths))
    }
}

/// What a change reaches: the filters its paths match, and the crates its
/// Rust inputs reach.
struct Change {
    filters: BTreeSet<Filter>,
    scope: Scope,
}

impl Change {
    /// The change of `paths`, relative to `root`.
    fn new(filters: &Filters, metadata: &Metadata, root: &Path, paths: &BTreeSet<String>) -> Self {
        let rust = paths
            .iter()
            .filter(|path| filters.matches(Filter::Rust, path));
        Self {
            filters: filters.matching(paths),
            scope: Scope::new(metadata, root, rust),
        }
    }

    /// What to do about `recipe`, which `job` runs.
    fn plan(&self, job: Job, recipe: Recipe) -> Step {
        let mut command: Vec<String> = recipe.command().iter().map(ToString::to_string).collect();
        let outcome = self.outcome(job, recipe, &mut command);
        Step {
            job,
            command,
            outcome,
        }
    }

    /// Whether to run `recipe`, narrowing its `command` to the crates the
    /// change reaches if it does.
    fn outcome(&self, job: Job, recipe: Recipe, command: &mut Vec<String>) -> Outcome {
        let trigger = job.trigger();
        if !trigger.fires(&self.filters) {
            return Outcome::Skipped(format!("no change matches {trigger}"));
        }
        if let Some(why) = recipe.left_to_ci() {
            return Outcome::LeftToCi(why);
        }
        let Scope::Crates(crates) = &self.scope else {
            return Outcome::ToRun;
        };
        match recipe.scoping() {
            Scoping::Workspace => {}
            Scoping::Crates { libraries_only } => {
                let packages: Vec<_> = crates
                    .iter()
                    .filter(|krate| krate.library || !libraries_only)
                    .map(|krate| format!("--package={}", krate.name))
                    .collect();
                if packages.is_empty() {
                    return Outcome::Skipped(
                        "no crate the change reaches has a library".to_owned(),
                    );
                }
                command.extend(packages);
            }
            Scoping::Only(names) => {
                if !crates
                    .iter()
                    .any(|krate| names.contains(&krate.name.as_str()))
                {
                    return Outcome::Skipped(format!(
                        "the change reaches none of {}",
                        names.join(", ")
                    ));
                }
            }
        }
        Outcome::ToRun
    }
}

/// The crates a change reaches: those it changes, and those depending on
/// them.
#[derive(Debug, PartialEq, Eq)]
enum Scope {
    /// Every crate, since `path` is in none, such as Cargo.lock.
    Workspace {
        path: String,
    },
    Crates(BTreeSet<Crate>),
}

/// A crate a change reaches.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Crate {
    name: String,
    /// Whether it has a library, which doc tests need.
    library: bool,
}

impl Scope {
    /// The crates `paths`, relative to `root`, reach through the
    /// dependencies of every kind `metadata` lists, and [`BUILDS`].
    fn new<'a>(metadata: &Metadata, root: &Path, paths: impl Iterator<Item = &'a String>) -> Self {
        let mut reached = BTreeSet::new();
        for path in paths {
            match metadata.package_of(&root.join(path)) {
                Some(package) => reached.insert(package.name.as_str()),
                None => return Self::Workspace { path: path.clone() },
            };
        }
        while let Some(dependent) = metadata.packages.iter().find(|package| {
            !reached.contains(package.name.as_str())
                && (package
                    .dependencies
                    .iter()
                    .any(|dependency| reached.contains(dependency.name.as_str()))
                    || BUILDS.iter().any(|&(builder, built)| {
                        builder == package.name && reached.contains(built)
                    }))
        }) {
            reached.insert(dependent.name.as_str());
        }
        Self::Crates(
            metadata
                .packages
                .iter()
                .filter(|package| reached.contains(package.name.as_str()))
                .map(|package| Crate {
                    name: package.name.clone(),
                    library: package.has_library(),
                })
                .collect(),
        )
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Workspace { path } => write!(f, "all of them, as {path} is in none"),
            Self::Crates(crates) if crates.is_empty() => f.write_str("none"),
            Self::Crates(crates) => {
                let names: Vec<_> = crates.iter().map(|krate| krate.name.as_str()).collect();
                write!(
                    f,
                    "{}, changed or depending on one that is",
                    names.join(", ")
                )
            }
        }
    }
}

/// A check CI could run, as `just check-changed` plans and runs it.
#[derive(Debug)]
struct Step {
    job: Job,
    /// The command that runs it, narrowed to the crates the change reaches.
    command: Vec<String>,
    outcome: Outcome,
}

impl Step {
    /// Runs the step if it is to run, recording whether it passed.
    fn run(&mut self, workspace: &Workspace) -> Result<()> {
        let (Outcome::ToRun, Some((program, args))) = (&self.outcome, self.command.split_first())
        else {
            return Ok(());
        };
        let command = self.command.join(" ");
        println!("\n==> {}: {command}", self.job.name());
        let status = workspace
            .command(program)
            .args(args)
            .status()
            .with_context(|| format!("run {command}"))?;
        self.outcome = if status.success() {
            Outcome::Passed
        } else {
            Outcome::Failed
        };
        Ok(())
    }
}

impl fmt::Display for Step {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (label, why) = match &self.outcome {
            Outcome::ToRun => ("not run", None),
            Outcome::Passed => ("passed", None),
            Outcome::Failed => ("FAILED", None),
            Outcome::Skipped(why) => ("skipped", Some(why.as_str())),
            Outcome::LeftToCi(why) => ("left to CI", Some(*why)),
        };
        write!(
            f,
            "  {label:<10}  {:<18}  {}",
            self.job.name(),
            self.command.join(" ")
        )?;
        if let Some(why) = why {
            write!(f, "  ({why})")?;
        }
        Ok(())
    }
}

/// Whether a check ran, and how it went.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// The change needs it here.
    ToRun,
    Passed,
    Failed,
    /// The change doesn't need it, for this reason.
    Skipped(String),
    /// The change needs it, but CI runs it, for this reason.
    LeftToCi(&'static str),
}
