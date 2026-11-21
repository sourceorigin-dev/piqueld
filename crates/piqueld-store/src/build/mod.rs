//! Build records outlive their output, until deployment history prunes their
//! operation; output is chunked and bounded.
use super::auth::access::scope_json;
use super::{Paging, Store, StoreError, days_ago_ms, decode_name, now_ms};
use piqueld_core::access::Scope;
use piqueld_core::{
    ApplicationId, BuildInputs, BuiltImage, EnvironmentId,
    api::{BuildLogChunk, BuildLogPage, BuildRecord, BuildState, LogStream, Page},
    manifest::ValidatedSource as Source,
};
use sqlx::{QueryBuilder, Sqlite, Transaction};

/// A succeeded build whose image another build may reuse; see
/// [`Store::reusable_builds`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReusableBuild {
    /// The build that produced the image.
    pub id: i64,
    /// The image it produced.
    pub image: BuiltImage,
}

/// How a build ended: its terminal state, and the image of a succeeded one.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BuildOutcome {
    /// The terminal state.
    pub state: BuildState,
    /// The image the build produced or reused.
    pub image: Option<String>,
    /// The earlier build whose image it reused instead of building.
    pub reused_build: Option<i64>,
}
impl BuildOutcome {
    /// A build that ended in `state` without an image.
    #[must_use]
    pub const fn without_image(state: BuildState) -> Self {
        Self {
            state,
            image: None,
            reused_build: None,
        }
    }
}

/// Whether a build's log took output offered to [`Store::append_build_log`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LogAppend {
    /// The output was stored.
    Accepted,
    /// The log accepts no more output: it reached its byte cap, is marked
    /// truncated or expired, or the build finished. Output past the cap was
    /// dropped.
    Closed,
}

/// The end of a build's output, written as the build finishes (see
/// [`Store::finish_build`]): output held back until then, and whether any of
/// its output was dropped.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LogEnding {
    /// Output to append, oldest first.
    pub output: Vec<(LogStream, Vec<u8>)>,
    /// Whether to mark the log truncated.
    pub truncated: bool,
}

/// `builds` row read by the build listing.
#[derive(sqlx::FromRow)]
struct BuildRow {
    id: i64,
    environment_id: String,
    operation_id: String,
    service: String,
    job: Option<String>,
    source_json: String,
    state: String,
    started_at_ms: i64,
    finished_at_ms: Option<i64>,
    commit_hash: Option<String>,
    image_id: Option<String>,
    reused_build_id: Option<i64>,
    exit_code: Option<i64>,
    log_bytes: i64,
    log_truncated: i64,
    log_expired: i64,
}

/// `build_log_chunks` row shared by the stream-filtered and unfiltered page queries.
struct BuildLogRow {
    offset: i64,
    data: Vec<u8>,
    stream: String,
    timestamp_ms: i64,
}

impl BuildRow {
    /// Converts raw columns into a build record, reporting unknown states as
    /// corruption.
    fn decode(self) -> Result<BuildRecord, StoreError> {
        // Its image tells where a build ran; a job run or a reused image
        // ran no build. It is only shown, so an image that doesn't parse
        // leaves it unknown.
        let built_on = self
            .image_id
            .as_deref()
            .filter(|_| self.job.is_none() && self.reused_build_id.is_none())
            .and_then(|image| BuiltImage::parse(image).ok())
            .as_ref()
            .map(piqueld_core::api::BuiltOn::of);
        Ok(BuildRecord {
            id: self.id,
            environment_id: self.environment_id,
            operation_id: self.operation_id,
            service: self.service,
            job: self.job,
            source: serde_json::from_str(&self.source_json).map_err(StoreError::corrupt)?,
            state: decode_name(&self.state)?,
            started_at_ms: self.started_at_ms,
            finished_at_ms: self.finished_at_ms,
            commit: self.commit_hash,
            built_on,
            image_id: self.image_id,
            reused_build: self.reused_build_id,
            exit_code: self.exit_code,
            log_bytes: self.log_bytes,
            log_truncated: self.log_truncated != 0,
            log_expired: self.log_expired != 0,
        })
    }
}

impl Store {
    /// Configures output retention and the per-build byte cap.
    #[must_use]
    pub fn with_build_history(mut self, policy: piqueld_config::BuildHistoryConfig) -> Self {
        self.build_history = policy;
        self
    }
    /// The most output bytes a build keeps.
    #[must_use]
    pub const fn build_log_max_bytes(&self) -> u32 {
        self.build_history.log_max_bytes
    }
    /// Records a new running build attempt for one service of an operation, or
    /// a job run when `job` names the job, and returns its row ID.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn start_build(
        &self,
        application: &EnvironmentId,
        operation: &str,
        service: &str,
        source: &Source,
        job: Option<&str>,
    ) -> Result<i64, StoreError> {
        let source = serde_json::to_string(source).map_err(StoreError::invalid_input)?;
        let app = application.as_str();
        let now = now_ms();
        Ok(self.write(sqlx::query!("INSERT INTO builds(environment_id,operation_id,service,job,source_json,state,started_at_ms) VALUES(?1,?2,?3,?4,?5,'running',?6)",app,operation,service,job,source,now)).await.map_err(StoreError::database)?.last_insert_rowid())
    }
    /// Appends build output, splitting it into chunks of at most 4096 bytes.
    /// Output past the per-build byte cap is dropped and the build is marked
    /// truncated; nothing more is accepted after truncation, expiry, or completion.
    /// Cut points never split a UTF-8 sequence, so multibyte text survives paging.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn append_build_log(
        &self,
        id: i64,
        bytes: &[u8],
        stream: LogStream,
    ) -> Result<LogAppend, StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let appended = self.append_build_log_on(&mut tx, id, bytes, stream).await?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok(appended)
    }
    /// [`Self::append_build_log`] in `tx`.
    async fn append_build_log_on(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        id: i64,
        bytes: &[u8],
        stream: LogStream,
    ) -> Result<LogAppend, StoreError> {
        let stream = stream.as_str();
        let timestamp = now_ms();
        let row = sqlx::query!(
            "SELECT log_bytes,log_expired,log_truncated,state FROM builds WHERE id=?1",
            id
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        if row.log_expired != 0 || row.log_truncated != 0 || row.state != "running" {
            return Ok(LogAppend::Closed);
        }
        let remaining = i64::from(self.build_history.log_max_bytes)
            .saturating_sub(row.log_bytes)
            .max(0);
        let mut count = bytes
            .len()
            .min(usize::try_from(remaining).map_err(StoreError::invalid_input)?);
        if count < bytes.len() {
            count = piqueld_core::api::BuildLogChunk::complete_prefix(&bytes[..count]);
        }
        // Each chunk is bounded regardless of which executor records output.
        let mut offset = row.log_bytes;
        let mut remaining = &bytes[..count];
        while !remaining.is_empty() {
            let mut end = remaining.len().min(4096);
            if end < remaining.len() {
                end = piqueld_core::api::BuildLogChunk::complete_prefix(&remaining[..end]);
            }
            let chunk = &remaining[..end];
            sqlx::query!(
                "INSERT INTO build_log_chunks(build_id,offset,data,stream,timestamp_ms) VALUES(?1,?2,?3,?4,?5)",
                id,
                offset,
                chunk,
                stream,
                timestamp
            )
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?;
            offset += i64::try_from(chunk.len()).map_err(StoreError::invalid_input)?;
            remaining = &remaining[end..];
        }
        let truncated = count < bytes.len();
        sqlx::query!(
            "UPDATE builds SET log_bytes=?1,log_truncated=log_truncated OR ?2 WHERE id=?3",
            offset,
            truncated,
            id
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Ok(if truncated {
            LogAppend::Closed
        } else {
            LogAppend::Accepted
        })
    }
    /// Records the repository commit a build checked out.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn build_commit(&self, id: i64, commit: &str) -> Result<(), StoreError> {
        self.write(sqlx::query!(
            "UPDATE builds SET commit_hash=?1 WHERE id=?2",
            commit,
            id
        ))
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }
    /// Records the exit code of a finished job run.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn build_exit_code(&self, id: i64, code: i64) -> Result<(), StoreError> {
        self.write(sqlx::query!(
            "UPDATE builds SET exit_code=?1 WHERE id=?2",
            code,
            id
        ))
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }
    /// Marks a running build finished with `outcome`: its terminal state,
    /// its image, and the build it reused that image from, if any. In the
    /// same transaction, appends `ending`'s output and marks the log
    /// truncated if `ending` says so, so a build never finishes without the
    /// end of its output. Already finished builds are left unchanged;
    /// `Running` is invalid input.
    ///
    /// # Errors
    /// Returns a storage error; then nothing is written.
    pub async fn finish_build(
        &self,
        id: i64,
        outcome: &BuildOutcome,
        ending: &LogEnding,
    ) -> Result<(), StoreError> {
        if outcome.state == BuildState::Running {
            return Err(StoreError::InvalidInput);
        }
        let (_writer, mut tx) = self.begin_immediate().await?;
        for (stream, bytes) in &ending.output {
            if self
                .append_build_log_on(&mut tx, id, bytes, *stream)
                .await?
                == LogAppend::Closed
            {
                break;
            }
        }
        let state = outcome.state.as_str();
        let now = now_ms();
        sqlx::query!(
            "UPDATE builds SET state=?1,finished_at_ms=?2,image_id=?3,reused_build_id=?4,log_truncated=log_truncated OR ?5 WHERE id=?6 AND state='running'",
            state,
            now,
            outcome.image,
            outcome.reused_build,
            ending.truncated,
            id
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }
    /// The succeeded builds of `environment`'s application whose image a
    /// build of `source` at `commit` may reuse, newest first: those that
    /// checked out the same commit with the same [`BuildInputs`], whichever
    /// branch or environment they built for. A build that itself reused an
    /// image is reported as the build that produced it.
    ///
    /// Callers check that the image is still present. A build whose record
    /// no longer decodes is skipped with a warning: reuse only saves a build.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn reusable_builds(
        &self,
        environment: &EnvironmentId,
        source: &Source,
        commit: &str,
    ) -> Result<Vec<ReusableBuild>, StoreError> {
        let inputs = BuildInputs::new(source, Some(commit));
        let environment = environment.as_str();
        Ok(sqlx::query!(
            r#"SELECT id AS "id!",source_json,image_id AS "image_id!",reused_build_id FROM builds
               WHERE commit_hash=?1 AND state='succeeded' AND job IS NULL AND image_id IS NOT NULL
               AND environment_id IN (SELECT id FROM environments WHERE application_id=
                   (SELECT application_id FROM environments WHERE id=?2))
               ORDER BY id DESC"#,
            commit,
            environment
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?
        .into_iter()
        .filter_map(|row| {
            let decoded = serde_json::from_str::<Source>(&row.source_json)
                .map_err(StoreError::corrupt)
                .and_then(|source| {
                    Ok((source, BuiltImage::parse(row.image_id).map_err(StoreError::corrupt)?))
                });
            let (source, image) = decoded
                .inspect_err(|error| {
                    tracing::warn!(build = row.id, %error, "skipping a build whose record no longer decodes");
                })
                .ok()?;
            (BuildInputs::new(&source, Some(commit)) == inputs).then(|| ReusableBuild {
                id: row.reused_build_id.unwrap_or(row.id),
                image,
            })
        })
        .collect())
    }
    /// Names of the jobs an operation already ran successfully. Retries and
    /// restarts skip them, so each job succeeds at most once per operation.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn succeeded_jobs(
        &self,
        operation: &str,
    ) -> Result<std::collections::BTreeSet<String>, StoreError> {
        Ok(sqlx::query_scalar!(
            "SELECT job AS \"job!\" FROM builds WHERE operation_id=?1 AND job IS NOT NULL AND state='succeeded'",
            operation
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?
        .into_iter()
        .collect())
    }
    /// Marks builds left running by a previous daemon process as interrupted.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn recover_builds(&self) -> Result<(), StoreError> {
        let now = now_ms();
        self.write(sqlx::query!(
            "UPDATE builds SET state='interrupted',finished_at_ms=?1 WHERE state='running'",
            now
        ))
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }
    /// Deletes output of builds finished before the retention window, keeping
    /// their metadata and flagging the output as expired. Builds already
    /// flagged are skipped.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn prune_build_logs(&self) -> Result<(), StoreError> {
        let cutoff = days_ago_ms(self.build_history.log_retention_days.into());
        let (_writer, mut tx) = self.begin_immediate().await?;
        sqlx::query!("DELETE FROM build_log_chunks WHERE build_id IN (SELECT id FROM builds WHERE finished_at_ms<?1 AND log_expired=0)",cutoff).execute(&mut *tx).await.map_err(StoreError::database)?;
        sqlx::query!(
            "UPDATE builds SET log_expired=1,log_bytes=0 WHERE finished_at_ms<?1 AND log_expired=0",
            cutoff
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }
    /// Lists build attempts newest first, optionally limited to the
    /// environments of one application and/or to one environment, and only
    /// of live environments of applications within `visible`.
    /// # Errors
    /// Returns invalid pagination or database errors.
    pub async fn builds(
        &self,
        application: Option<&ApplicationId>,
        environment: Option<&EnvironmentId>,
        visible: &Scope,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<BuildRecord>, StoreError> {
        let paging = Paging::new(limit)?;
        let before = Paging::cursor(cursor)?
            .map(str::parse::<i64>)
            .transpose()
            .map_err(StoreError::invalid_input)?
            .unwrap_or(i64::MAX);
        if before < 1 {
            return Err(StoreError::InvalidInput);
        }
        let mut query = QueryBuilder::<Sqlite>::new(
            "SELECT id,environment_id,operation_id,service,job,source_json,state,started_at_ms,\
             finished_at_ms,commit_hash,image_id,reused_build_id,exit_code,log_bytes,log_truncated,log_expired \
             FROM builds WHERE id<",
        );
        query.push_bind(before);
        if let Some(environment) = environment {
            query
                .push(" AND environment_id=")
                .push_bind(environment.as_str());
        }
        if let Some(visible) = scope_json(visible) {
            query
                .push(" AND environment_id IN (SELECT e.id FROM environments e JOIN json_each(")
                .push_bind(visible)
                .push(") v ON v.value=e.application_id)");
        }
        if let Some(application) = application {
            query
                .push(" AND environment_id IN (SELECT id FROM environments WHERE application_id=")
                .push_bind(application.as_str())
                .push(")");
        }
        query
            .push(" ORDER BY id DESC LIMIT ")
            .push_bind(paging.fetch);
        let mut rows = query
            .build_query_as::<BuildRow>()
            .fetch_all(&self.pool)
            .await
            .map_err(StoreError::database)?;
        let next_cursor = paging.next_cursor(&mut rows, |row| row.id);
        let items = rows
            .into_iter()
            .map(BuildRow::decode)
            .collect::<Result<_, _>>()?;
        Ok(Page { items, next_cursor })
    }
    /// Returns the owner of the environment a build attempt belongs to, if
    /// the build exists.
    /// # Errors
    /// Returns storage errors.
    pub async fn build_owner(
        &self,
        id: i64,
    ) -> Result<Option<super::EnvironmentOwner>, StoreError> {
        sqlx::query!(
            r#"SELECT e.application_id AS "application_id!",e.kind='preview' AS "preview!: bool" FROM builds b JOIN environments e ON e.id=b.environment_id WHERE b.id=?1"#,
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        .map(|row| {
            Ok(super::EnvironmentOwner {
                application: ApplicationId::parse(row.application_id)
                    .map_err(StoreError::corrupt)?,
                preview: row.preview,
            })
        })
        .transpose()
    }
    /// Reads the newest output chunks before an optional exclusive cursor. Stream filtering happens before pagination.
    /// Pages hold up to 16 chunks in offset order; `previous_offset` is the cursor
    /// for the next older page.
    /// # Errors
    /// Returns not found, invalid cursors, or storage errors.
    pub async fn build_logs(
        &self,
        id: i64,
        before: Option<i64>,
        stream: Option<LogStream>,
    ) -> Result<BuildLogPage, StoreError> {
        if before.is_some_and(|value| value < 0) {
            return Err(StoreError::InvalidInput);
        }
        let mut tx = self.snapshot().await?;
        let row = sqlx::query!(
            "SELECT log_truncated,log_expired FROM builds WHERE id=?1",
            id
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        let before = before.unwrap_or(i64::MAX);
        // Separate queries, so a stream filter seeks `build_log_stream`.
        let mut chunks = if let Some(stream) = stream {
            let stream = stream.as_str();
            sqlx::query_as!(
                BuildLogRow,
                "SELECT offset,data,stream,timestamp_ms FROM build_log_chunks
                 WHERE build_id=?1 AND stream=?2 AND offset<?3
                 ORDER BY offset DESC LIMIT 17",
                id,
                stream,
                before
            )
            .fetch_all(&mut *tx)
            .await
        } else {
            sqlx::query_as!(
                BuildLogRow,
                "SELECT offset,data,stream,timestamp_ms FROM build_log_chunks
                 WHERE build_id=?1 AND offset<?2 ORDER BY offset DESC LIMIT 17",
                id,
                before
            )
            .fetch_all(&mut *tx)
            .await
        }
        .map_err(StoreError::database)?;
        let more = chunks.len() > 16;
        chunks.truncate(16);
        chunks.reverse();
        let previous_offset = more.then(|| chunks[0].offset);
        let items = chunks
            .into_iter()
            .map(|chunk| {
                Ok(BuildLogChunk {
                    offset: chunk.offset,
                    timestamp_ms: chunk.timestamp_ms,
                    stream: decode_name(&chunk.stream)?,
                    text: String::from_utf8_lossy(&chunk.data).into_owned(),
                })
            })
            .collect::<Result<_, StoreError>>()?;
        Ok(BuildLogPage {
            items,
            previous_offset,
            truncated: row.log_truncated != 0,
            expired: row.log_expired != 0,
        })
    }
}

#[cfg(test)]
mod tests;
