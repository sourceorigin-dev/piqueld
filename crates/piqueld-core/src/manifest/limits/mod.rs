//! How the installation-wide preview limits, the daemon's `[previews]`
//! settings, bound what a preview deploys.

use super::{NormalizedApplication, ValidatedResourceLimits};
pub use crate::settings::PreviewLimits;
use crate::{ApiErrorCode, DiagnosticSeverity, PlanDiagnostic};

impl PreviewLimits {
    /// Bounds a preview's rendered services: each CPU or memory limit a
    /// service leaves unset gets its default, and replicas above
    /// `max_replicas` are capped. Explicit limits are kept. Returns a
    /// warning for every service it changed.
    ///
    /// ```text
    /// web: replicas 3, no limits -> replicas 1, 500 millicores, 536870912 bytes
    ///   preview_limits_defaulted, preview_replicas_capped
    /// db:  replicas 1, 2000 millicores -> unchanged CPU, default memory
    ///   preview_limits_defaulted
    /// ```
    pub(crate) fn bound(&self, application: &mut NormalizedApplication) -> Vec<PlanDiagnostic> {
        let mut warnings = Vec::new();
        for service in &mut application.spec.services {
            let warning = |code: ApiErrorCode, message: String| {
                PlanDiagnostic::new(
                    code,
                    DiagnosticSeverity::Warning,
                    service.name.as_str(),
                    message,
                )
            };
            let resources = service.resources.get_or_insert(ValidatedResourceLimits {
                cpu_millis: None,
                memory_bytes: None,
            });
            let mut defaulted = Vec::new();
            if resources.cpu_millis.is_none() {
                resources.cpu_millis = Some(self.default_cpu_millis);
                defaulted.push(format!("{} millicores of CPU", self.default_cpu_millis));
            }
            if resources.memory_bytes.is_none() {
                resources.memory_bytes = Some(self.default_memory_bytes);
                defaulted.push(format!("{} bytes of memory", self.default_memory_bytes));
            }
            if !defaulted.is_empty() {
                warnings.push(warning(
                    ApiErrorCode::PreviewLimitsDefaulted,
                    format!(
                        "{} runs with the [previews] default of {}, since its manifest sets no limit",
                        service.name,
                        defaulted.join(" and ")
                    ),
                ));
            }
            if service.replicas > self.max_replicas {
                warnings.push(warning(
                    ApiErrorCode::PreviewReplicasCapped,
                    format!(
                        "{} asks for {} replicas; previews run at most {} ([previews] max_replicas)",
                        service.name, service.replicas, self.max_replicas
                    ),
                ));
                service.replicas = self.max_replicas;
            }
        }
        warnings
    }
}

#[cfg(test)]
mod tests;
