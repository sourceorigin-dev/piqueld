//! `volume`: the Docker volumes the daemon tracks, which environments bind
//! their manifest volume names to, and the detached volumes deleted
//! environments leave.
use crate::{
    environments::select,
    error::Result,
    output::{Console, reports::report},
};
use clap::Subcommand;
use piqueld_client::{
    Client,
    observability::{Bytes, Timestamp},
    volumes::{AttachVolumeRequest, VolumeId, VolumeView},
};

#[derive(Debug, Subcommand)]
pub(crate) enum VolumeCommand {
    /// List volumes with their state and size.
    ///
    /// With an application, lists the volumes its environments bind, or with
    /// an environment, that environment's (needs `apps:read`). Without one,
    /// lists every volume (needs `system:read`). Sizes are measured every 15
    /// minutes.
    List {
        /// Application name or stable ID.
        application: Option<String>,
        /// Environment name or stable ID.
        #[arg(requires = "application")]
        environment: Option<String>,
        /// Only list detached volumes, which deleted environments left.
        #[arg(long, conflicts_with = "application")]
        detached: bool,
    },
    /// Show one volume: its Docker name, state, size, and where it is bound
    /// or was bound when it was detached.
    ///
    /// Needs `system:read`.
    Show {
        /// Volume ID, from `volume list`.
        volume: VolumeId,
    },
    /// Attach a detached volume to an environment under a volume name.
    ///
    /// The environment mounts it where its manifest mounts NAME, from its next
    /// deployment. NAME must not be bound yet, and previews delete their
    /// volumes, so they take none. Needs `apps:write` on the application and
    /// `system:operate`.
    Attach {
        /// Application name or stable ID.
        application: String,
        /// Environment name or stable ID.
        environment: String,
        /// The manifest volume name to bind.
        name: String,
        /// Detached volume ID, from `volume list --detached`.
        volume: VolumeId,
    },
    /// Delete a detached volume and its data.
    ///
    /// Attached volumes can't be deleted. Needs `system:operate`.
    Delete {
        /// Volume ID, from `volume list --detached`.
        volume: VolumeId,
        /// Skip the confirmation prompt.
        #[arg(long, short)]
        yes: bool,
    },
}

impl VolumeCommand {
    /// Runs a volume command.
    pub(crate) async fn run(&self, client: &Client, console: &mut Console) -> Result<()> {
        match self {
            Self::List {
                application: None,
                detached,
                ..
            } => console.emit(&Volumes(client.volumes(*detached).await?)),
            Self::List {
                application: Some(application),
                environment,
                ..
            } => {
                let environments = match environment {
                    Some(_) => vec![select(client, application, environment.as_deref()).await?.1],
                    None => {
                        crate::commands::resolve_application(client, application)
                            .await?
                            .environments
                    }
                };
                let mut volumes = Vec::new();
                for environment in environments {
                    volumes.extend(client.environment_volumes(environment.id.as_str()).await?);
                }
                console.emit(&Volumes(volumes))
            }
            Self::Show { volume } => console.emit(&client.volume(volume.as_str()).await?),
            Self::Attach {
                application,
                environment,
                name,
                volume,
            } => {
                let (_, environment) = select(client, application, Some(environment)).await?;
                let attached = client
                    .attach_volume(
                        environment.id.as_str(),
                        name,
                        &AttachVolumeRequest {
                            volume: volume.clone(),
                        },
                    )
                    .await?;
                console.emit(&attached)
            }
            Self::Delete { volume, yes } => {
                console
                    .confirm(
                        *yes,
                        &format!("Delete volume {volume} and all its data? This can't be undone."),
                    )
                    .await?;
                client.delete_volume(volume.as_str()).await?;
                console.emit(&crate::output::reports::NameDeletionReport {
                    deleted: volume.as_str(),
                })
            }
        }
    }
}

/// Where a volume is bound, or was when it was detached.
fn whereabouts(volume: &VolumeView) -> String {
    if let Some(binding) = &volume.binding {
        return format!(
            "{}/{} as {}",
            binding.application, binding.environment, binding.name
        );
    }
    match &volume.detached_from {
        Some(from) => match &from.name {
            Some(name) => format!("from {}/{} as {name}", from.application, from.environment),
            None => format!("from {}/{}", from.application, from.environment),
        },
        None => "from an unknown environment".to_owned(),
    }
}

/// Its measured size, or `-` before it is measured.
fn size(volume: &VolumeView) -> String {
    volume
        .size_bytes
        .map_or_else(|| "-".to_owned(), |bytes| Bytes(bytes).to_string())
}

/// Volumes, one per line; JSON is the volumes.
struct Volumes(Vec<VolumeView>);
report!(Volumes as [VolumeView] = 0, self, out, {
    if self.0.is_empty() {
        return out.line("No volumes.");
    }
    for volume in &self.0 {
        out.line(format_args!(
            "{}  {}  {}  {}",
            volume.id,
            volume.state,
            size(volume),
            whereabouts(volume)
        ))?;
    }
    Ok(())
});

report!(VolumeView, self, out, {
    out.label("Volume", self.id.as_str())?;
    out.label("Docker name", self.docker_name.as_str())?;
    out.label("Pool", self.pool)?;
    out.label("State", self.state)?;
    out.label("Bound", whereabouts(self))?;
    out.label("Size", size(self))?;
    if let Some(measured) = self.measured_at_ms {
        out.label("Measured", Timestamp(measured))?;
    }
    out.label("Created", Timestamp(self.created_at_ms))
});
