//! The release channel operators chose, and the newer builds the daemon
//! found on it.

use crate::{Author, Store, StoreError, now_ms};
use piqueld_core::EventKind;
use piqueld_core::access::{GlobalPermission, Permission};
use piqueld_core::version::Channel;

impl Store {
    /// The channel operators chose with [`Self::set_update_channel`], if any.
    /// # Errors
    /// Returns storage errors, or [`StoreError::Corrupt`] for an unknown
    /// channel.
    pub async fn update_channel(&self) -> Result<Option<Channel>, StoreError> {
        sqlx::query_scalar!("SELECT channel FROM update_channel")
            .fetch_optional(&self.pool)
            .await
            .map_err(StoreError::database)?
            .map(|channel| Channel::parse(&channel).ok_or(StoreError::Corrupt))
            .transpose()
    }

    /// Follows `channel`, which needs `system:operate`, recording the change
    /// in the audit trail.
    /// # Errors
    /// Returns refusal or storage errors.
    pub async fn set_update_channel(
        &self,
        author: impl Into<Author<'_>>,
        channel: Channel,
    ) -> Result<(), StoreError> {
        let author = author.into();
        let (_writer, mut tx) = self.begin_immediate().await?;
        author
            .actor
            .require_on(
                &mut tx,
                Permission::Global(GlobalPermission::SystemOperate),
                None,
            )
            .await?;
        let name = channel.as_str();
        sqlx::query!(
            "INSERT INTO update_channel(singleton,channel) VALUES(1,?1)
            ON CONFLICT(singleton) DO UPDATE SET channel=excluded.channel",
            name,
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let change = format!("Followed the {channel} channel");
        author
            .commit_recorded(tx, 200, Some(&change), (None, None))
            .await
    }

    /// Records that `version` is available, as a daemon-scoped
    /// `update_available` event whose resource is the version, which notifies
    /// as `update_available`, unless one was already recorded for it.
    /// Returns whether it recorded one. Once retention removes that event,
    /// a build still newest notifies again, as a reminder.
    /// # Errors
    /// Returns storage errors.
    pub async fn record_update_available(
        &self,
        version: &str,
        message: &str,
    ) -> Result<bool, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let kind = EventKind::UpdateAvailable.as_str();
        let recorded = sqlx::query_scalar!(
            "SELECT 1 AS \"found!: i64\" FROM events WHERE kind=?1 AND resource=?2",
            kind,
            version,
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        if recorded.is_some() {
            return Ok(false);
        }
        let now = now_ms();
        sqlx::query!(
            "INSERT INTO events(scope,kind,message,resource,created_at_ms)
            VALUES('daemon',?1,?2,?3,?4)",
            kind,
            message,
            version,
            now,
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(true)
    }
}
