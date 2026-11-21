//! Deployments whose images cleanup keeps, by retention root, and the images
//! a build machine pushed, which registry cleanup deletes.
use super::{Store, StoreError};
use piqueld_core::{
    ImmutableImage, RepositoryDigest, RetentionRoot, build_machine::BuildRepository,
    resource::ResolvedApplication,
};
use sqlx::SqliteConnection;
use std::collections::BTreeSet;

impl Store {
    /// Every image a retention root keeps, given how many successful
    /// deployments each environment keeps. The roots are read from one
    /// snapshot: read one by one, a target published and then superseded in
    /// between would be neither current nor latest. Fails rather than
    /// skipping a target that doesn't decode, so cleanup never runs on a
    /// partial set.
    /// # Errors
    /// Returns storage or decoding errors.
    pub async fn retained_images(&self, keep: u32) -> Result<BTreeSet<ImmutableImage>, StoreError> {
        let mut snapshot = self.snapshot().await?;
        let mut images = BTreeSet::new();
        for root in RetentionRoot::ALL {
            for target in Self::retained_on(&mut snapshot, root, keep).await? {
                images.extend(target.images());
            }
        }
        Ok(images)
    }

    /// Records that a build machine pushed `image` for this installation,
    /// once the build reports it, before the engine pulls it: registry
    /// cleanup deletes it once no deployment keeps it, whatever happens to
    /// the build's record.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn record_pushed_image(&self, image: &RepositoryDigest) -> Result<(), StoreError> {
        let image = image.as_str();
        self.write(sqlx::query!(
            "INSERT OR IGNORE INTO pushed_images(image) VALUES (?1)",
            image
        ))
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// The images a build machine pushed to `repository` for this
    /// installation that registry cleanup hasn't deleted yet; see
    /// [`Self::record_pushed_image`].
    ///
    /// # Errors
    /// Returns storage errors, or a decoding error for a corrupt row.
    pub async fn pushed_images(
        &self,
        repository: &BuildRepository,
    ) -> Result<Vec<RepositoryDigest>, StoreError> {
        let prefix = format!("{repository}@");
        sqlx::query_scalar!(
            "SELECT image FROM pushed_images WHERE substr(image,1,length(?1))=?1 ORDER BY image",
            prefix
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?
        .into_iter()
        .map(|image| RepositoryDigest::parse(image).map_err(StoreError::corrupt))
        .collect()
    }

    /// Forgets `image` once it is deleted from the build registry.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn forget_pushed_image(&self, image: &RepositoryDigest) -> Result<(), StoreError> {
        let image = image.as_str();
        self.write(sqlx::query!(
            "DELETE FROM pushed_images WHERE image=?1",
            image
        ))
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// The prepared targets `root` keeps, across every environment and
    /// preview.
    async fn retained_on(
        connection: &mut SqliteConnection,
        root: RetentionRoot,
        keep: u32,
    ) -> Result<Vec<ResolvedApplication>, StoreError> {
        let targets = match root {
            RetentionRoot::Current => sqlx::query_scalar!(
                r#"SELECT resolved_json AS "json!" FROM environments WHERE resolved_json IS NOT NULL"#
            )
            .fetch_all(&mut *connection)
            .await,
            RetentionRoot::Latest => sqlx::query_scalar!(
                r#"SELECT o.target_json AS "json!" FROM operations o WHERE o.target_json IS NOT NULL AND o.id=(SELECT latest.id FROM operations latest WHERE latest.environment_id=o.environment_id ORDER BY latest.created_at_ms DESC,latest.id DESC LIMIT 1)"#
            )
            .fetch_all(&mut *connection)
            .await,
            // Only environments: previews are never restored.
            RetentionRoot::Recent => sqlx::query_scalar!(
                r#"SELECT json AS "json!" FROM (SELECT o.target_json AS json,ROW_NUMBER() OVER (PARTITION BY d.environment_id ORDER BY d.created_at_ms DESC,d.id DESC) AS rank FROM deployments d JOIN operations o ON o.id=d.id JOIN environments e ON e.id=d.environment_id WHERE e.kind='environment' AND d.succeeded_at_ms IS NOT NULL AND o.target_json IS NOT NULL) WHERE rank<=?1"#,
                keep
            )
            .fetch_all(&mut *connection)
            .await,
            RetentionRoot::PromotionSource => sqlx::query_scalar!(
                r#"SELECT s.resolved_json AS "json!" FROM environments s WHERE s.resolved_json IS NOT NULL AND EXISTS(SELECT 1 FROM environments p WHERE p.promoted_from=s.id)"#
            )
            .fetch_all(&mut *connection)
            .await,
        }
        .map_err(StoreError::database)?;
        targets
            .iter()
            .map(|json| serde_json::from_str(json).map_err(StoreError::corrupt))
            .collect()
    }
}

#[cfg(test)]
mod tests;
