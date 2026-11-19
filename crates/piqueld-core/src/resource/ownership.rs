//! Ownership labels on the Docker resources piqueld manages.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{
    APPLICATION_LABEL, INSTANCE_LABEL, InstanceId, JOB_LABEL, MANAGED_LABEL, SERVICE_LABEL,
    SPEC_HASH_LABEL, valid_sha256,
};
use crate::{
    DockerNetworkName, EnvironmentId, JobName, ResourceKind, ServiceName, docker_resource_name,
};

/// Ownership metadata used to label runtime resources.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Ownership {
    /// The control-plane instance that owns the resource.
    pub instance_id: InstanceId,
    /// The environment that owns the resource.
    pub environment_id: EnvironmentId,
    /// Logical service name when the resource belongs to one service.
    pub service: Option<ServiceName>,
    /// Normalized application spec hash.
    pub spec_hash: String,
}

impl Ownership {
    /// Produces the labels used to identify an owned Docker resource.
    ///
    /// The service label is only present for service-scoped resources.
    ///
    /// ```text
    /// io.piqueld.managed     = "true"
    /// io.piqueld.instance    = <instance_id>
    /// io.piqueld.application = <environment_id>
    /// io.piqueld.spec-hash   = "sha256:..."
    /// io.piqueld.service     = <service>   (optional)
    /// ```
    #[must_use]
    pub fn labels(&self) -> BTreeMap<String, String> {
        let mut labels = Self::scope(&self.instance_id, &self.environment_id);
        labels.insert(SPEC_HASH_LABEL.into(), self.spec_hash.clone());
        if let Some(service) = &self.service {
            labels.insert(SERVICE_LABEL.into(), service.to_string());
        }
        labels
    }

    /// The subset of [`Self::labels`] every resource `instance` manages for
    /// `environment` carries, which scopes Docker lookups and removals to them.
    ///
    /// ```text
    /// io.piqueld.managed     = "true"
    /// io.piqueld.instance    = <instance>
    /// io.piqueld.application = <environment>
    /// ```
    #[must_use]
    pub fn scope(instance: &InstanceId, environment: &EnvironmentId) -> BTreeMap<String, String> {
        BTreeMap::from([
            (MANAGED_LABEL.into(), "true".into()),
            (INSTANCE_LABEL.into(), instance.to_string()),
            (APPLICATION_LABEL.into(), environment.to_string()),
        ])
    }

    /// The environment desired ownership `labels` name. `None` unless the
    /// managed marker is `"true"`, the spec hash is a valid `sha256:` digest,
    /// and both the environment and instance identities parse.
    pub(super) fn labelled_environment(labels: &BTreeMap<String, String>) -> Option<EnvironmentId> {
        if labels.get(MANAGED_LABEL).map(String::as_str) != Some("true")
            || labels
                .get(SPEC_HASH_LABEL)
                .is_none_or(|hash| !valid_sha256(hash))
        {
            return None;
        }
        InstanceId::parse(labels.get(INSTANCE_LABEL)?.as_str()).ok()?;
        EnvironmentId::parse(labels.get(APPLICATION_LABEL)?.as_str()).ok()
    }
}

/// Result of comparing runtime ownership labels with an expected owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OwnershipState {
    /// Labels identify the expected instance and application.
    Owned,
    /// Labels identify a different instance or application.
    Foreign,
    /// Required ownership labels are missing or malformed.
    Invalid,
}

impl OwnershipState {
    /// Checks shared ownership plus the resource's role and canonical identity;
    /// a wrong role or name is `Invalid`. Services need a valid `SERVICE_LABEL`
    /// that derives their name, and networks need no service label and a name
    /// scoped to the application. Volumes have no logical-name label, so their
    /// name is checked separately against the desired volume before use;
    /// retained volumes are never deleted.
    #[must_use]
    pub fn for_resource(
        labels: &BTreeMap<String, String>,
        instance: &InstanceId,
        application: &EnvironmentId,
        kind: ResourceKind,
        name: &str,
    ) -> Self {
        let shared = Self::from_labels(labels, instance, application);
        if shared != Self::Owned {
            return shared;
        }
        let valid_role = match kind {
            ResourceKind::Service => labels.get(SERVICE_LABEL).is_some_and(|service| {
                ServiceName::parse(service.clone()).is_ok()
                    && name == docker_resource_name(application, kind, Some(service))
            }),
            ResourceKind::Network => {
                !labels.contains_key(SERVICE_LABEL)
                    && DockerNetworkName::parse(name)
                        .is_ok_and(|name| name.is_canonical(application, labels))
            }
            ResourceKind::Volume | ResourceKind::File => !labels.contains_key(SERVICE_LABEL),
            ResourceKind::Job => {
                !labels.contains_key(SERVICE_LABEL)
                    && labels.get(JOB_LABEL).is_some_and(|job| {
                        JobName::parse(job.clone()).is_ok()
                            && name == docker_resource_name(application, kind, Some(job))
                    })
            }
        };
        if valid_role {
            Self::Owned
        } else {
            Self::Invalid
        }
    }

    /// Classifies ownership labels without exposing raw backend data.
    ///
    /// A missing managed marker or malformed spec hash is `Invalid`; otherwise a
    /// different instance or application is `Foreign`.
    #[must_use]
    pub fn from_labels(
        labels: &BTreeMap<String, String>,
        instance: &InstanceId,
        application: &EnvironmentId,
    ) -> Self {
        if labels.get(MANAGED_LABEL).map(String::as_str) != Some("true")
            || labels
                .get(SPEC_HASH_LABEL)
                .is_none_or(|hash| !valid_sha256(hash))
        {
            return Self::Invalid;
        }
        if labels.get(INSTANCE_LABEL).map(String::as_str) != Some(instance.as_str())
            || labels.get(APPLICATION_LABEL).map(String::as_str) != Some(application.as_str())
        {
            return Self::Foreign;
        }
        Self::Owned
    }
}
