//! The path filters in `.github/filters.yml`, matched as CI's `changes` job
//! matches them with dorny/paths-filter.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use globset::{Glob, GlobBuilder, GlobSet, GlobSetBuilder};
use yaml_rust2::{Yaml, YamlLoader};

/// A filter in `.github/filters.yml`. The file names exactly these, so a
/// filter added there is added here too, and to the jobs it starts in
/// `ci.rs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Filter {
    Workspace,
    Crates,
    Daemon,
    Rust,
    Deny,
    Ui,
    Cli,
    Docker,
    Playwright,
}

impl Filter {
    const ALL: [Self; 9] = [
        Self::Workspace,
        Self::Crates,
        Self::Daemon,
        Self::Rust,
        Self::Deny,
        Self::Ui,
        Self::Cli,
        Self::Docker,
        Self::Playwright,
    ];

    /// Its name in the file, and in the `changes` job's outputs.
    fn name(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Crates => "crates",
            Self::Daemon => "daemon",
            Self::Rust => "rust",
            Self::Deny => "deny",
            Self::Ui => "ui",
            Self::Cli => "cli",
            Self::Docker => "docker",
            Self::Playwright => "playwright",
        }
    }
}

impl fmt::Display for Filter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// The filters of `.github/filters.yml`, each compiled to the globs it lists.
#[derive(Debug)]
pub(super) struct Filters(BTreeMap<Filter, GlobSet>);

impl Filters {
    /// Where the filters live, from the workspace root.
    pub(super) const PATH: &str = ".github/filters.yml";

    pub(super) fn load(root: &Path) -> Result<Self> {
        let path = root.join(Self::PATH);
        let yaml =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        Self::parse(&yaml).with_context(|| format!("parse {}", Self::PATH))
    }

    /// Parses filters as paths-filter does: a map from each filter's name to
    /// a glob or a list of them, where a list may hold another, as a YAML
    /// alias to another filter does.
    pub(super) fn parse(yaml: &str) -> Result<Self> {
        let documents = YamlLoader::load_from_str(yaml)?;
        let Some(Yaml::Hash(entries)) = documents.first() else {
            bail!("expected a map of filters");
        };
        let mut filters = BTreeMap::new();
        for (name, item) in entries {
            let name = name.as_str().context("a filter's name is not a string")?;
            let filter = Filter::ALL
                .into_iter()
                .find(|filter| filter.name() == name)
                .with_context(|| {
                    format!(
                        "unknown filter `{name}`: add it to `Filter` in \
                         tools/xtask/src/check_changed/filters.rs, and to the jobs it starts"
                    )
                })?;
            let mut globs = GlobSetBuilder::new();
            Self::add_globs(item, &mut globs).with_context(|| format!("filter `{name}`"))?;
            filters.insert(filter, globs.build()?);
        }
        if let Some(missing) = Filter::ALL
            .into_iter()
            .find(|filter| !filters.contains_key(filter))
        {
            bail!("no filter `{missing}`: remove it from `Filter` and the jobs it starts");
        }
        Ok(Self(filters))
    }

    /// Adds the globs `item` lists, flattening nested lists.
    fn add_globs(item: &Yaml, globs: &mut GlobSetBuilder) -> Result<()> {
        match item {
            Yaml::String(pattern) => {
                globs.add(Self::glob(pattern)?);
            }
            Yaml::Array(items) => {
                for item in items {
                    Self::add_globs(item, globs)?;
                }
            }
            // paths-filter also takes a map from change types to globs,
            // such as `added: '**'`, which this doesn't need yet.
            other => bail!("expected a glob or a list of them, found {other:?}"),
        }
        Ok(())
    }

    /// Compiles `pattern` as paths-filter does, with picomatch's `dot: true`:
    /// `*` matches within a path segment and `**` across them, and both match
    /// dotfiles. picomatch also matches a directory itself against `dir/**`,
    /// which is moot here: git lists only files.
    fn glob(pattern: &str) -> Result<Glob> {
        // picomatch negates a pattern starting with `!`; globset reads it as
        // a literal `!`, which would silently match nothing.
        ensure!(
            !pattern.starts_with('!'),
            "negated pattern `{pattern}`: check-changed doesn't support them"
        );
        GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .with_context(|| format!("glob `{pattern}`"))
    }

    /// Whether `path`, relative to the workspace root, matches `filter`.
    pub(super) fn matches(&self, filter: Filter, path: &str) -> bool {
        self.0
            .get(&filter)
            .is_some_and(|globs| globs.is_match(path))
    }

    /// The filters any of `paths` matches.
    pub(super) fn matching(&self, paths: &BTreeSet<String>) -> BTreeSet<Filter> {
        self.0
            .keys()
            .copied()
            .filter(|&filter| paths.iter().any(|path| self.matches(filter, path)))
            .collect()
    }
}
