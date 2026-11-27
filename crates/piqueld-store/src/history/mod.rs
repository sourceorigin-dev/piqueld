//! Bounded deployment history. Every deploy, promotion, restore and refresh
//! records a deployment, so without a bound history grows forever.
use super::{Store, StoreError, days_ago_ms, now_ms};

/// Deployments pruned per transaction, so pruning never holds the writer long.
const PRUNE_BATCH: usize = 1_000;
/// How long ago a preview's deployment must have finished before it can be
/// pruned: a superseded one may still be stopping.
const PREVIEW_MIN_AGE_MS: i64 = 3_600_000;
/// How many of each application's releases pruning keeps, newest by creation
/// time first (then highest ID), whether or not a deployment references them;
/// it keeps every one a deployment references too. Not by ID alone, which
/// follows creation time only within one daemon run.
const KEPT_RELEASES: i64 = 50;

impl Store {
    /// Prunes each environment's finished deployments older than
    /// `retention.deployment_days` and beyond its newest
    /// `retention.deployments_per_environment`, with their operations, keeping
    /// its current and latest deployment, its newest `images.keep_deployments`
    /// successful ones, restored ones, ones whose jobs need cleaning up and, for
    /// previews, those finished under [`PREVIEW_MIN_AGE_MS`] ago. Then removes
    /// expired build and job-run records of pruned operations and releases
    /// beyond each application's newest [`KEPT_RELEASES`]. A zero setting disables it.
    /// # Errors
    /// Returns storage or encoding errors.
    pub async fn prune_deployment_history(&self) -> Result<(), StoreError> {
        let settings = self.settings();
        let retention = settings.retention;
        if retention.deployment_days == 0 || retention.deployments_per_environment == 0 {
            return Ok(());
        }
        let (succeeded, cutoff, preview_cutoff) = (
            settings.images.keep_deployments.max(1),
            days_ago_ms(retention.deployment_days),
            now_ms().saturating_sub(PREVIEW_MIN_AGE_MS),
        );
        let candidates = sqlx::query_scalar!(
            r#"WITH current AS (
                SELECT id FROM (SELECT (SELECT d.id FROM deployments d JOIN operations o ON o.id=d.id WHERE o.environment_id=e.id AND o.promoted=1 ORDER BY o.created_at_ms DESC,o.id DESC LIMIT 1) AS id FROM environments e) WHERE id IS NOT NULL
            ), ranked AS (
                SELECT d.id,e.kind,o.finished_at_ms,d.succeeded_at_ms IS NOT NULL AS succeeded,
                    ROW_NUMBER() OVER (PARTITION BY d.environment_id ORDER BY d.created_at_ms DESC,d.id DESC) AS rank,
                    ROW_NUMBER() OVER (PARTITION BY d.environment_id,d.succeeded_at_ms IS NOT NULL ORDER BY d.created_at_ms DESC,d.id DESC) AS outcome_rank
                FROM deployments d JOIN operations o ON o.id=d.id JOIN environments e ON e.id=d.environment_id
            )
            SELECT r.id AS "id!" FROM ranked r
            WHERE r.finished_at_ms IS NOT NULL AND r.rank>1
                AND NOT (r.succeeded AND r.outcome_rank<=CASE WHEN r.kind='preview' THEN 1 ELSE ?1 END)
                AND CASE WHEN r.kind='preview' THEN r.finished_at_ms<?4 ELSE r.rank>?2 AND r.finished_at_ms<?3 END
                AND r.id NOT IN (SELECT id FROM current)"#,
            succeeded,
            retention.deployments_per_environment,
            cutoff,
            preview_cutoff
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?;
        for batch in candidates.chunks(PRUNE_BATCH) {
            let ids = serde_json::to_string(batch).map_err(StoreError::corrupt)?;
            let (_writer, mut tx) = self.begin_immediate().await?;
            let finished = sqlx::query_scalar!(
                r#"DELETE FROM operations WHERE id IN (SELECT value FROM json_each(?1))
                    AND NOT EXISTS(SELECT 1 FROM job_cleanup j WHERE j.operation_id=operations.id)
                    AND id NOT IN (SELECT json_extract(origin_json,'$.deployment') FROM deployments WHERE json_extract(origin_json,'$.type')='restore')
                    RETURNING finished_at_ms AS "finished_at_ms!""#,
                ids
            )
            .fetch_all(&mut *tx)
            .await
            .map_err(StoreError::database)?;
            if let Some(last) = finished.into_iter().max() {
                sqlx::query!(
                    "UPDATE history_coverage SET pruned_through_ms=MAX(COALESCE(pruned_through_ms,0),?1) WHERE singleton=1",
                    last
                )
                .execute(&mut *tx)
                .await
                .map_err(StoreError::database)?;
            }
            tx.commit().await.map_err(StoreError::database)?;
        }
        let (_writer, mut tx) = self.begin_immediate().await?;
        sqlx::query!("DELETE FROM builds WHERE log_expired=1 AND NOT EXISTS(SELECT 1 FROM operations o WHERE o.id=builds.operation_id) AND id NOT IN (SELECT reused_build_id FROM builds WHERE reused_build_id IS NOT NULL)")
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        sqlx::query!(
            "DELETE FROM releases WHERE NOT EXISTS(SELECT 1 FROM deployments d WHERE d.release_id=releases.id) AND id NOT IN (SELECT id FROM (SELECT id,ROW_NUMBER() OVER (PARTITION BY application_id ORDER BY created_at_ms DESC,id DESC) AS rank FROM releases) WHERE rank<=?1)",
            KEPT_RELEASES
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }
}

#[cfg(test)]
mod tests;
