//! Zone ownership: a hostname belongs to the provider with the longest zone
//! containing it. A zone claimed by two providers is a conflict and not used.
use super::provider::Zone;
use piqueld_core::manifest::Hostname;
use thiserror::Error;

/// Why no provider can manage a hostname.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ZoneError {
    /// No discovered zone contains the hostname.
    #[error("no configured DNS provider zone contains {0}")]
    NoZone(Hostname),
    /// The hostname's longest matching zone is claimed by several providers.
    #[error("zone {0} is claimed by more than one DNS provider")]
    Conflict(Hostname),
}

/// Each provider's zones, in configuration order.
pub(super) struct Zones<'a>(pub(super) Vec<&'a [Zone]>);

impl<'a> Zones<'a> {
    /// The provider index and zone owning `hostname`.
    pub(super) fn owner(&self, hostname: &Hostname) -> Result<(usize, &'a Zone), ZoneError> {
        let mut matches = self.0.iter().enumerate().flat_map(|(index, zones)| {
            zones
                .iter()
                .filter(|zone| hostname.is_within(&zone.name))
                .map(move |zone| (index, zone))
        });
        let first = matches
            .next()
            .ok_or_else(|| ZoneError::NoZone(hostname.clone()))?;
        let mut best = first;
        let mut claimed_twice = false;
        for candidate in matches {
            let (longer, same) = (
                candidate.1.name.as_str().len() > best.1.name.as_str().len(),
                candidate.1.name == best.1.name,
            );
            if longer {
                best = candidate;
                claimed_twice = false;
            } else if same && candidate.0 != best.0 {
                claimed_twice = true;
            }
        }
        if claimed_twice {
            return Err(ZoneError::Conflict(best.1.name.clone()));
        }
        Ok(best)
    }

    /// Zones of provider `index` that another provider also claims.
    pub(super) fn conflicts(&self, index: usize) -> Vec<String> {
        self.0[index]
            .iter()
            .filter(|zone| {
                self.0.iter().enumerate().any(|(other, zones)| {
                    other != index && zones.iter().any(|z| z.name == zone.name)
                })
            })
            .map(|zone| zone.name.to_string())
            .collect()
    }
}

#[cfg(test)]
mod tests;
