//! Build history: refreshing a list or an output that older pages extended,
//! and how a build's arguments read.
use piqueld_client::{BuildLogChunk, BuildRecord};
use std::cmp::Reverse;
use std::collections::BTreeMap;
use std::fmt::Display;

/// A build list, newest (highest ID) first.
pub trait BuildList {
    /// Applies a refreshed first `page`: builds it lists replace their
    /// listed versions and new ones are added, while builds of older pages
    /// stay listed.
    fn merge_newest(&mut self, page: Vec<BuildRecord>);
}

impl BuildList for Vec<BuildRecord> {
    fn merge_newest(&mut self, page: Vec<BuildRecord>) {
        for build in page {
            match self.iter_mut().find(|listed| listed.id == build.id) {
                Some(listed) => *listed = build,
                None => self.push(build),
            }
        }
        self.sort_by_key(|build| Reverse(build.id));
    }
}

/// A build's loaded output, oldest chunk first.
pub trait OutputChunks {
    /// Applies a refetched newest `page`: it replaces the chunks from its
    /// first offset on, while older chunks, loaded from earlier pages, stay.
    /// Returns whether any did, so their cursor still applies.
    fn merge_newest(&mut self, page: Vec<BuildLogChunk>) -> bool;
}

impl OutputChunks for Vec<BuildLogChunk> {
    fn merge_newest(&mut self, page: Vec<BuildLogChunk>) -> bool {
        let start = page.first().map_or(i64::MAX, |chunk| chunk.offset);
        self.retain(|chunk| chunk.offset < start);
        let kept = !self.is_empty();
        self.extend(page);
        kept
    }
}

/// Renders Docker build arguments as space-separated `KEY="VALUE"` pairs.
/// Values are quoted and escaped so spaces cannot read as extra arguments.
#[must_use]
pub fn build_arguments<V: Display>(args: &BTreeMap<String, V>) -> String {
    args.iter()
        .map(|(key, value)| format!("{key}={:?}", value.to_string()))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests;
