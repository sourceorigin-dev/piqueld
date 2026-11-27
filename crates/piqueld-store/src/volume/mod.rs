//! Volumes: the Docker volumes the daemon tracks, and the manifest volume
//! names environments bind to them. A volume is attached while a binding
//! points at it, so rebinding a name is one update of its binding, which
//! detaches the volume it pointed at before.
//!
//! Saving a target binds each volume name it declares, creating a volume
//! named after its ID for a name bound to none. Deleting an environment
//! detaches its volumes; a preview's volumes go with it instead.
use super::{Author, EnvironmentId, ResolvedApplication, Store, StoreError, new_id, now_ms};
use piqueld_core::access::{AppPermission, GlobalPermission, Permission};
use piqueld_core::api::{DetachedFrom, VolumeBinding, VolumePool, VolumeState, VolumeView};
use piqueld_core::resource::{BoundVolume, ManagedVolume};
use piqueld_core::{
    ApplicationId, ApplicationName, DockerVolumeName, EnvironmentName, EnvironmentSource, VolumeId,
    VolumeName,
};
use sqlx::SqliteConnection;
use std::collections::BTreeMap;

/// Why a volume change was refused.
#[derive(Debug, thiserror::Error)]
pub enum VolumeError {
    /// The environment already binds the name to a volume.
    #[error("{environment} already binds volume name {name}; pick a name it doesn't use")]
    NameBound {
        /// The environment's name.
        environment: EnvironmentName,
        /// The bound name.
        name: VolumeName,
    },
    /// Only detached volumes can be attached or deleted.
    #[error(
        "volume {volume} is attached to an environment; only detached volumes can be attached or deleted"
    )]
    Attached {
        /// The attached volume.
        volume: VolumeId,
    },
    /// Previews delete their volumes with them.
    #[error(
        "previews delete their volumes with them, so no volume can be attached to a preview, nor a live preview's volume to an environment"
    )]
    Preview,
    /// The saved manifest the environment deploys declares no volume of the
    /// name.
    #[error(
        "{environment}'s manifest declares no volume {name}; declare it first, then attach the volume before deploying it"
    )]
    NameUndeclared {
        /// The environment's name.
        environment: EnvironmentName,
        /// The undeclared name.
        name: VolumeName,
    },
}

/// A volume as Docker knows it: what removing it needs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DockerVolume {
    /// Its ID.
    pub id: VolumeId,
    /// Its Docker name.
    pub docker_name: DockerVolumeName,
    /// The environment its labels name, which removal checks.
    pub owner: EnvironmentId,
}

/// What changing a detached volume takes, since it may hold any
/// application's data.
const OPERATE: Permission = Permission::Global(GlobalPermission::SystemOperate);

/// Which volumes a listing returns.
#[derive(Clone, Copy, Debug)]
enum Selection<'a> {
    /// Every volume.
    All,
    /// Detached volumes only.
    Detached,
    /// Those bound to the environment.
    Environment(&'a EnvironmentId),
    /// One volume.
    One(&'a VolumeId),
}

impl Store {
    /// Every volume, or only detached ones, attached ones first by
    /// application, environment and name.
    ///
    /// # Errors
    /// Returns a storage or decoding error.
    pub async fn volumes(&self, detached: bool) -> Result<Vec<VolumeView>, StoreError> {
        let selection = if detached {
            Selection::Detached
        } else {
            Selection::All
        };
        self.select_volumes(selection).await
    }

    /// The volumes `environment` binds names to, by name.
    ///
    /// # Errors
    /// Returns a storage or decoding error.
    pub async fn environment_volumes(
        &self,
        environment: &EnvironmentId,
    ) -> Result<Vec<VolumeView>, StoreError> {
        self.select_volumes(Selection::Environment(environment))
            .await
    }

    /// One volume.
    ///
    /// # Errors
    /// Returns `NotFound`, or a storage or decoding error.
    pub async fn volume(&self, id: &VolumeId) -> Result<VolumeView, StoreError> {
        self.select_volumes(Selection::One(id))
            .await?
            .pop()
            .ok_or(StoreError::NotFound)
    }

    /// Binds `name` of `environment` to the detached volume `volume` for
    /// `author`, who needs `apps:write` on it and `system:operate`; it mounts
    /// the volume from its next deployment. Refuses a preview or a live
    /// preview's volume (`Preview`), a name the saved manifest it deploys
    /// lacks (`NameUndeclared`), a bound name (`NameBound`) and an attached
    /// volume (`Attached`).
    ///
    /// # Errors
    /// Returns those refusals, `Denied`, `NotFound` for a missing
    /// environment or volume, `Busy` for an environment being deleted, or a
    /// storage error.
    pub async fn attach_volume(
        &self,
        author: impl Into<Author<'_>>,
        environment: &EnvironmentId,
        name: &VolumeName,
        volume: &VolumeId,
    ) -> Result<VolumeView, StoreError> {
        let author = author.into();
        let (_writer, mut tx) = self.begin_immediate().await?;
        author.actor.require_on(&mut tx, OPERATE, None).await?;
        author
            .actor
            .require_on_environment(&mut tx, AppPermission::Write, environment)
            .await?;
        let target = Self::environment_on(&mut tx, environment.as_str())
            .await?
            .ok_or(StoreError::NotFound)?;
        if target.environment.preview().is_some() {
            return Err(VolumeError::Preview.into());
        }
        if target.delete_intent() {
            return Err(StoreError::Busy);
        }
        let declared = match target.environment.source {
            EnvironmentSource::Saved => target
                .application
                .application
                .spec()
                .volumes
                .iter()
                .any(|volume| volume.name == name.as_str()),
            EnvironmentSource::Branch(_) | EnvironmentSource::Promoted(_) => true,
        };
        if !declared {
            return Err(VolumeError::NameUndeclared {
                environment: target.environment.name.clone(),
                name: name.clone(),
            }
            .into());
        }
        let (id, environment_id, binding) = (volume.as_str(), environment.as_str(), name.as_str());
        let row = sqlx::query!(
            r#"SELECT EXISTS(SELECT 1 FROM volume_bindings WHERE volume_id=v.id) AS "bound!: bool",EXISTS(SELECT 1 FROM environments WHERE id=v.owner_environment_id AND kind='preview') AS "preview!: bool" FROM volumes v WHERE v.id=?1 AND v.removing=0"#,
            id
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        if row.preview {
            return Err(VolumeError::Preview.into());
        }
        if row.bound {
            return Err(VolumeError::Attached {
                volume: volume.clone(),
            }
            .into());
        }
        let inserted = sqlx::query!(
            "INSERT INTO volume_bindings(environment_id,name,volume_id) VALUES(?1,?2,?3) ON CONFLICT DO NOTHING",
            environment_id,
            binding,
            id
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        if inserted == 0 {
            return Err(VolumeError::NameBound {
                environment: target.environment.name.clone(),
                name: name.clone(),
            }
            .into());
        }
        sqlx::query!(
            "UPDATE volumes SET detached_application=NULL,detached_environment=NULL,detached_name=NULL WHERE id=?1",
            id
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let change = format!("volume {volume} attached as {name}");
        let application = target.environment.application_id.as_str();
        author
            .commit_recorded(
                tx,
                200,
                Some(&change),
                (Some(application), Some(environment_id)),
            )
            .await?;
        self.volume(volume).await
    }

    /// Starts deleting the detached volume `id` for `author`, who needs
    /// `system:operate`: marks it removing, which hides it and keeps it from
    /// being attached or adopted again, and returns what removing it from
    /// Docker needs. Finish with [`Self::finish_volume_removal`].
    ///
    /// # Errors
    /// Returns `Attached` for an attached volume, `Denied`, `NotFound`, or a
    /// storage error.
    pub async fn delete_volume(
        &self,
        author: impl Into<Author<'_>>,
        id: &VolumeId,
    ) -> Result<DockerVolume, StoreError> {
        let author = author.into();
        let (_writer, mut tx) = self.begin_immediate().await?;
        author.actor.require_on(&mut tx, OPERATE, None).await?;
        let volume = id.as_str();
        let row = sqlx::query!(
            r#"SELECT docker_name AS "docker_name!",owner_environment_id AS "owner!",EXISTS(SELECT 1 FROM volume_bindings WHERE volume_id=v.id) AS "bound!: bool" FROM volumes v WHERE v.id=?1 AND v.removing=0"#,
            volume
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        if row.bound {
            return Err(VolumeError::Attached { volume: id.clone() }.into());
        }
        sqlx::query!("UPDATE volumes SET removing=1 WHERE id=?1", volume)
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        let change = format!("volume {id} ({}) deleted", row.docker_name);
        author
            .commit_recorded(tx, 200, Some(&change), (None, None))
            .await?;
        Ok(DockerVolume {
            id: id.clone(),
            docker_name: DockerVolumeName::parse(row.docker_name).map_err(StoreError::corrupt)?,
            owner: EnvironmentId::parse(row.owner).map_err(StoreError::corrupt)?,
        })
    }

    /// Ends deleting `volume` (see [`Self::delete_volume`]): forgets it once
    /// Docker `removed` it, or else keeps it, listed again under its ID, so
    /// deleting it again retries.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn finish_volume_removal(
        &self,
        volume: &VolumeId,
        removed: bool,
    ) -> Result<(), StoreError> {
        let volume = volume.as_str();
        let query = if removed {
            sqlx::query!("DELETE FROM volumes WHERE id=?1 AND removing=1", volume)
        } else {
            sqlx::query!("UPDATE volumes SET removing=0 WHERE id=?1", volume)
        };
        self.write(query).await.map_err(StoreError::database)?;
        Ok(())
    }

    /// Keeps the volumes a removal was interrupted for, as daemon startup
    /// finds them: no removal outlives the daemon.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn resume_volume_removals(&self) -> Result<(), StoreError> {
        self.write(sqlx::query!(
            "UPDATE volumes SET removing=0 WHERE removing=1"
        ))
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// The volumes a preview created, which its deletion removes, including
    /// those later manifests dropped; no other environment can attach them
    /// (see [`Self::attach_volume`]). None for an environment, whose
    /// deletion detaches its volumes instead.
    ///
    /// # Errors
    /// Returns a storage or decoding error.
    pub async fn preview_volumes(
        &self,
        preview: &EnvironmentId,
    ) -> Result<Vec<DockerVolume>, StoreError> {
        let preview = preview.as_str();
        sqlx::query!(
            r#"SELECT id AS "id!",docker_name AS "docker_name!" FROM volumes WHERE owner_environment_id=?1 AND EXISTS(SELECT 1 FROM environments WHERE id=?1 AND kind='preview') ORDER BY docker_name"#,
            preview
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?
        .into_iter()
        .map(|row| {
            Ok(DockerVolume {
                id: VolumeId::parse(row.id).map_err(StoreError::corrupt)?,
                docker_name: DockerVolumeName::parse(row.docker_name)
                    .map_err(StoreError::corrupt)?,
                owner: EnvironmentId::parse(preview).map_err(StoreError::corrupt)?,
            })
        })
        .collect()
    }

    /// Records a survey of every volume the instance manages, listed at
    /// `listed_at_ms`: adopts those the database doesn't know as detached
    /// volumes, and forgets detached volumes Docker no longer has. An
    /// attached volume missing from Docker is kept: it may not be created
    /// yet. So is a record newer than the listing, which can't include it,
    /// and one being removed, which only its removal forgets.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn record_volume_survey(
        &self,
        volumes: &[ManagedVolume],
        listed_at_ms: i64,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let now = now_ms();
        for volume in volumes {
            let (name, owner) = (volume.name.as_str(), volume.owner.as_str());
            let id = new_id("volume");
            sqlx::query!(
                "INSERT INTO volumes(id,docker_name,pool,owner_environment_id,created_at_ms,detached_application,detached_environment) VALUES(?1,?2,'local',?3,?4,(SELECT a.name FROM environments e JOIN applications a ON a.id=e.application_id WHERE e.id=?3),(SELECT name FROM environments WHERE id=?3)) ON CONFLICT(docker_name) DO NOTHING",
                id,
                name,
                owner,
                now
            )
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        }
        let names = serde_json::to_string(
            &volumes
                .iter()
                .map(|volume| volume.name.as_str())
                .collect::<Vec<_>>(),
        )
        .map_err(StoreError::corrupt)?;
        sqlx::query!(
            "DELETE FROM volumes WHERE docker_name NOT IN (SELECT value FROM json_each(?1)) AND NOT EXISTS(SELECT 1 FROM volume_bindings WHERE volume_id=volumes.id) AND created_at_ms<?2 AND removing=0",
            names,
            listed_at_ms
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }

    /// Records the measured `sizes` of tracked volumes, by Docker name.
    /// Volumes Docker didn't measure keep their last size.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn record_volume_sizes(
        &self,
        sizes: &BTreeMap<String, u64>,
    ) -> Result<(), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let now = now_ms();
        for (name, size) in sizes {
            let size = i64::try_from(*size).map_err(StoreError::corrupt)?;
            sqlx::query!(
                "UPDATE volumes SET size_bytes=?1,measured_at_ms=?2 WHERE docker_name=?3",
                size,
                now,
                name
            )
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        }
        tx.commit().await.map_err(StoreError::database)
    }

    /// Binds every volume name `target` declares before it is saved (see
    /// [`ResolvedApplication::bind_volumes`]). A name bound to none takes
    /// the detached volume created before volumes were tracked under it,
    /// if the environment has one, or else a new volume named after its ID.
    pub(super) async fn bind_volumes_on(
        connection: &mut SqliteConnection,
        target: &mut ResolvedApplication,
    ) -> Result<(), StoreError> {
        Self::bind_target_volumes(connection, target, true).await
    }

    /// Binds the names `target` declares that its environment already
    /// binds, leaving the others as compiled, as a plan of it shows them:
    /// new volumes.
    ///
    /// # Errors
    /// Returns a storage or decoding error.
    pub async fn bind_existing_volumes(
        &self,
        target: &mut ResolvedApplication,
    ) -> Result<(), StoreError> {
        Self::bind_target_volumes(&mut *self.connection().await?, target, false).await
    }

    /// Binds `target`'s volume names, creating the missing bindings when
    /// `create`.
    async fn bind_target_volumes(
        connection: &mut SqliteConnection,
        target: &mut ResolvedApplication,
        create: bool,
    ) -> Result<(), StoreError> {
        let mut bound = BTreeMap::new();
        for volume in &target.volumes {
            let binding = match Self::bound_volume_on(connection, &target.id, &volume.logical_name)
                .await?
            {
                Some(binding) => binding,
                None if create => {
                    Self::create_binding_on(connection, &target.id, &volume.logical_name).await?
                }
                None => continue,
            };
            bound.insert(volume.logical_name.clone(), binding);
        }
        target.bind_volumes(&bound);
        Ok(())
    }

    /// The volume `environment` binds `name` to, if any.
    async fn bound_volume_on(
        connection: &mut SqliteConnection,
        environment: &EnvironmentId,
        name: &VolumeName,
    ) -> Result<Option<BoundVolume>, StoreError> {
        let (environment, name) = (environment.as_str(), name.as_str());
        sqlx::query!(
            r#"SELECT v.docker_name AS "docker_name!",v.owner_environment_id AS "owner!" FROM volume_bindings b JOIN volumes v ON v.id=b.volume_id WHERE b.environment_id=?1 AND b.name=?2"#,
            environment,
            name
        )
        .fetch_optional(&mut *connection)
        .await
        .map_err(StoreError::database)?
        .map(|row| {
            Ok(BoundVolume {
                docker_name: DockerVolumeName::parse(row.docker_name)
                    .map_err(StoreError::corrupt)?,
                owner: EnvironmentId::parse(row.owner).map_err(StoreError::corrupt)?,
            })
        })
        .transpose()
    }

    /// Binds `name` of `environment`, which binds it to no volume, to the
    /// detached volume the environment created under it before volumes were
    /// tracked, or else to a new volume named after its ID.
    async fn create_binding_on(
        connection: &mut SqliteConnection,
        environment: &EnvironmentId,
        name: &VolumeName,
    ) -> Result<BoundVolume, StoreError> {
        let legacy = DockerVolumeName::for_volume(environment, name);
        let (environment_id, binding, legacy_name) =
            (environment.as_str(), name.as_str(), legacy.as_str());
        let existing = sqlx::query_scalar!(
            "SELECT id FROM volumes v WHERE docker_name=?1 AND owner_environment_id=?2 AND removing=0 AND NOT EXISTS(SELECT 1 FROM volume_bindings WHERE volume_id=v.id)",
            legacy_name,
            environment_id
        )
        .fetch_optional(&mut *connection)
        .await
        .map_err(StoreError::database)?;
        let (id, docker_name) = if let Some(id) = existing {
            (id, legacy)
        } else {
            let id = VolumeId::parse(new_id("volume")).map_err(StoreError::corrupt)?;
            let docker_name = DockerVolumeName::for_id(&id);
            let (volume, docker, now) = (id.as_str(), docker_name.as_str(), now_ms());
            sqlx::query!(
                "INSERT INTO volumes(id,docker_name,pool,owner_environment_id,created_at_ms) VALUES(?1,?2,'local',?3,?4)",
                volume,
                docker,
                environment_id,
                now
            )
            .execute(&mut *connection)
            .await
            .map_err(StoreError::database)?;
            (id.to_string(), docker_name)
        };
        sqlx::query!(
            "UPDATE volumes SET detached_application=NULL,detached_environment=NULL,detached_name=NULL WHERE id=?1",
            id
        )
        .execute(&mut *connection)
        .await
        .map_err(StoreError::database)?;
        sqlx::query!(
            "INSERT INTO volume_bindings(environment_id,name,volume_id) VALUES(?1,?2,?3)",
            environment_id,
            binding,
            id
        )
        .execute(&mut *connection)
        .await
        .map_err(StoreError::database)?;
        Ok(BoundVolume {
            docker_name,
            owner: environment.clone(),
        })
    }

    /// Records where `environment`'s volumes were bound, before deleting it
    /// detaches them, and forgets a preview's own volumes, which its
    /// deletion removed from Docker (see [`Self::preview_volumes`]).
    pub(super) async fn detach_volumes_on(
        connection: &mut SqliteConnection,
        environment: &str,
    ) -> Result<(), StoreError> {
        sqlx::query!(
            "DELETE FROM volumes WHERE owner_environment_id=?1 AND EXISTS(SELECT 1 FROM environments WHERE id=?1 AND kind='preview')",
            environment
        )
        .execute(&mut *connection)
        .await
        .map_err(StoreError::database)?;
        sqlx::query!(
            "UPDATE volumes SET detached_application=a.name,detached_environment=e.name,detached_name=b.name FROM volume_bindings b JOIN environments e ON e.id=b.environment_id JOIN applications a ON a.id=e.application_id WHERE b.volume_id=volumes.id AND b.environment_id=?1",
            environment
        )
        .execute(&mut *connection)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// The volumes `selection` selects, attached ones first by application,
    /// environment and name, then detached ones by creation.
    async fn select_volumes(
        &self,
        selection: Selection<'_>,
    ) -> Result<Vec<VolumeView>, StoreError> {
        let (id, environment, detached) = match selection {
            Selection::All => (None, None, false),
            Selection::Detached => (None, None, true),
            Selection::Environment(environment) => (None, Some(environment.as_str()), false),
            Selection::One(id) => (Some(id.as_str()), None, false),
        };
        sqlx::query!(
            r#"SELECT v.id AS "id!",v.docker_name AS "docker_name!",v.pool AS "pool!",v.created_at_ms AS "created_at_ms!",v.size_bytes,v.measured_at_ms,v.detached_application,v.detached_environment,v.detached_name,b.name AS "binding?",e.id AS "environment_id?",e.name AS "environment?",a.id AS "application_id?",a.name AS "application?" FROM volumes v LEFT JOIN volume_bindings b ON b.volume_id=v.id LEFT JOIN environments e ON e.id=b.environment_id LEFT JOIN applications a ON a.id=e.application_id WHERE v.removing=0 AND (?1 IS NULL OR v.id=?1) AND (?2 IS NULL OR b.environment_id=?2) AND (?3=0 OR b.volume_id IS NULL) ORDER BY b.volume_id IS NULL,a.name,e.name,b.name,v.created_at_ms,v.id"#,
            id,
            environment,
            detached
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?
        .into_iter()
        .map(|row| {
            let binding = match (
                row.binding,
                row.environment_id,
                row.environment,
                row.application_id,
                row.application,
            ) {
                (Some(name), Some(environment_id), Some(environment), Some(application_id), Some(application)) => {
                    Some(VolumeBinding {
                        application_id: ApplicationId::parse(application_id)
                            .map_err(StoreError::corrupt)?,
                        application: ApplicationName::parse(application)
                            .map_err(StoreError::corrupt)?,
                        environment_id: EnvironmentId::parse(environment_id)
                            .map_err(StoreError::corrupt)?,
                        environment: EnvironmentName::parse(environment)
                            .map_err(StoreError::corrupt)?,
                        name: VolumeName::parse(name).map_err(StoreError::corrupt)?,
                    })
                }
                _ => None,
            };
            let detached_from = if let (true, Some(application), Some(environment)) =
                (binding.is_none(), row.detached_application, row.detached_environment)
            {
                Some(DetachedFrom {
                    application: ApplicationName::parse(application).map_err(StoreError::corrupt)?,
                    environment: EnvironmentName::parse(environment).map_err(StoreError::corrupt)?,
                    name: row
                        .detached_name
                        .map(VolumeName::parse)
                        .transpose()
                        .map_err(StoreError::corrupt)?,
                })
            } else {
                None
            };
            Ok(VolumeView {
                id: VolumeId::parse(row.id).map_err(StoreError::corrupt)?,
                docker_name: DockerVolumeName::parse(row.docker_name).map_err(StoreError::corrupt)?,
                pool: VolumePool::parse(&row.pool).ok_or(StoreError::Corrupt)?,
                state: if binding.is_some() {
                    VolumeState::Attached
                } else {
                    VolumeState::Detached
                },
                binding,
                detached_from,
                size_bytes: row
                    .size_bytes
                    .map(u64::try_from)
                    .transpose()
                    .map_err(StoreError::corrupt)?,
                measured_at_ms: row.measured_at_ms,
                created_at_ms: row.created_at_ms,
            })
        })
        .collect()
    }
}

#[cfg(test)]
mod tests;
