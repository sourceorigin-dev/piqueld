//! Authenticate the database's key before accepting any new ciphertext.
use super::{Envelope, SecretCipher, SecretOwner, Store, StoreError};
use crate::secrets::{KeyFailure, SecretError};
use piqueld_core::EventKind;
use piqueld_core::api::{CheckStatus, SecretKeyRecovery, SystemCheck, SystemCheckId};
use sqlx::SqliteConnection;

/// How a master key file relates to the database it should decrypt.
pub(crate) enum KeyBinding {
    /// Nothing stored needs a key yet; `present` says whether one exists.
    Unbound {
        /// Whether the key file exists.
        present: bool,
    },
    /// The key decrypts the verifier or, without one, a stored value.
    Matches,
}

/// What a master key makes of the stored values and webhook secrets.
enum Ciphertext {
    /// Nothing is stored.
    None,
    /// It decrypts one of them.
    Decrypts,
    /// It decrypts none of them.
    Undecryptable,
}

impl Store {
    /// Checks without side effects that `secrets.key` exists while values
    /// need it, is private, and matches the stored verifier or, without one,
    /// the stored values. Unlike [`Self::verified_secret_cipher`], it never
    /// generates a key or stores a verifier.
    pub async fn secret_key_check(&self) -> SystemCheck {
        self.inspect_secret_key().await.unwrap_or_else(|error| {
            SystemCheck::new(
                SystemCheckId::SecretKey,
                CheckStatus::Fail,
                format!("could not read the stored secrets: {error}"),
            )
            .next_step("Check the database; daemon logs have details.")
        })
    }

    /// [`Self::secret_key_check`]'s verdict, short of database errors.
    async fn inspect_secret_key(&self) -> Result<SystemCheck, StoreError> {
        let check =
            |status, message: String| SystemCheck::new(SystemCheckId::SecretKey, status, message);
        let path = self.secret_key_path.display();
        let key = match self.secret_key_path.try_exists() {
            Ok(present) => present.then_some(self.secret_key_path.as_path()),
            Err(error) => {
                return Ok(check(
                    CheckStatus::Fail,
                    format!("could not inspect {path}: {error}"),
                )
                .next_step("Check the data directory's permissions."));
            }
        };
        let mut tx = self.snapshot().await?;
        Ok(match Self::key_binding_on(&mut tx, key).await? {
            Ok(KeyBinding::Unbound { present: false }) => check(
                CheckStatus::Pass,
                format!("no secret is stored yet; {path} is created by the first secret write"),
            ),
            Ok(KeyBinding::Unbound { present: true }) => check(
                CheckStatus::Pass,
                format!("{path} is private; the first secret write binds it to the database"),
            ),
            Ok(KeyBinding::Matches) => check(
                CheckStatus::Pass,
                format!("{path} is private and matches the database"),
            ),
            Err(error) => {
                let step = match error.key_failure() {
                    Some(KeyFailure::NotPrivate) => format!(
                        "Make {path} a regular file owned by the daemon user with mode 0600."
                    ),
                    Some(KeyFailure::Missing) => format!(
                        "Restore {path} from backup, or discard every stored value with `piquelctl secrets recover-key`."
                    ),
                    _ => format!(
                        "Restore the original {path} from backup, or discard every stored value with `piquelctl secrets recover-key`."
                    ),
                };
                check(CheckStatus::Fail, error.chain()).next_step(step)
            }
        })
    }

    /// Compares the master key at `key`, or no key file (`None`), with the
    /// database `tx` reads, without changing either. The key must exist
    /// while the database has a verifier or encrypted values, be private,
    /// and decrypt the verifier or, without one, a stored value. The inner
    /// error is the key's failure, such as [`KeyFailure::Missing`].
    ///
    /// # Errors
    /// Returns a storage error when the database can't be read.
    pub(crate) async fn key_binding_on(
        tx: &mut SqliteConnection,
        key: Option<&std::path::Path>,
    ) -> Result<Result<KeyBinding, SecretError>, StoreError> {
        let verifier = Self::secret_key_verifier_on(&mut *tx).await?;
        let bound = verifier.is_some()
            || !matches!(Self::probe_ciphertext_on(tx, None).await?, Ciphertext::None);
        let Some(key) = key else {
            return Ok(if bound {
                Err(KeyFailure::Missing.into())
            } else {
                Ok(KeyBinding::Unbound { present: false })
            });
        };
        let cipher = match SecretCipher::open(key) {
            Ok(cipher) => cipher,
            Err(error) => return Ok(Err(error)),
        };
        Ok(match &verifier {
            Some(verifier) => cipher.verify(verifier).map(|()| KeyBinding::Matches),
            None if bound => match Self::probe_ciphertext_on(tx, Some(&cipher)).await? {
                Ciphertext::Decrypts => Ok(KeyBinding::Matches),
                _ => Err(KeyFailure::AuthenticationFailed.into()),
            },
            None => Ok(KeyBinding::Unbound { present: true }),
        })
    }

    /// Loads the master key and proves it matches the database by decrypting the
    /// stored verifier. The first write binds the key by storing a new verifier
    /// in `tx`, so it only persists if the caller commits. Without a verifier,
    /// a key is only generated while nothing is encrypted, and only an
    /// existing key that decrypts a stored value is bound otherwise.
    ///
    /// # Errors
    /// Returns a storage error.
    pub(crate) async fn verified_secret_cipher(
        &self,
        tx: &mut SqliteConnection,
    ) -> Result<SecretCipher, StoreError> {
        let Some(verifier) = Self::secret_key_verifier_on(&mut *tx).await? else {
            let encrypted = !matches!(Self::probe_ciphertext_on(tx, None).await?, Ciphertext::None);
            let cipher = SecretCipher::load(&self.secret_key_path, encrypted)
                .map_err(StoreError::SecretSource)?;
            if encrypted
                && !matches!(
                    Self::probe_ciphertext_on(tx, Some(&cipher)).await?,
                    Ciphertext::Decrypts
                )
            {
                return Err(StoreError::SecretSource(
                    KeyFailure::AuthenticationFailed.into(),
                ));
            }
            let envelope = cipher
                .encrypt(
                    SecretOwner::KeyVerifier,
                    "key-verification",
                    1,
                    b"piqueld-secret-key-v1",
                )
                .map_err(StoreError::SecretSource)?;
            sqlx::query!(
                "INSERT INTO secret_key_verification(singleton,nonce,ciphertext) VALUES(1,?1,?2)",
                envelope.nonce,
                envelope.ciphertext
            )
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
            return Ok(cipher);
        };
        let cipher =
            SecretCipher::load(&self.secret_key_path, true).map_err(StoreError::SecretSource)?;
        cipher.verify(&verifier).map_err(StoreError::SecretSource)?;
        Ok(cipher)
    }

    /// The verifier binding the master key to the database, stored with the
    /// first value it encrypted.
    async fn secret_key_verifier_on<'e>(
        executor: impl sqlx::SqliteExecutor<'e>,
    ) -> Result<Option<Envelope>, StoreError> {
        Ok(
            sqlx::query!("SELECT nonce,ciphertext FROM secret_key_verification WHERE singleton=1")
                .fetch_optional(executor)
                .await
                .map_err(StoreError::database)?
                .map(|row| Envelope {
                    nonce: row.nonce,
                    ciphertext: row.ciphertext,
                }),
        )
    }

    /// What `cipher`, or a key that can't be loaded (`None`), makes of the
    /// stored values and webhook secrets, stopping at the first it decrypts.
    async fn probe_ciphertext_on(
        tx: &mut SqliteConnection,
        cipher: Option<&SecretCipher>,
    ) -> Result<Ciphertext, StoreError> {
        use futures_util::TryStreamExt;
        use piqueld_core::{ApplicationId, EnvironmentId};
        let mut rows = sqlx::query!(
            r#"SELECT 'environment' AS "kind!: String",environment_id AS "owner!",name AS "name!",generation AS "generation!",nonce AS "nonce!",ciphertext AS "ciphertext!" FROM secret_versions WHERE available=1
            UNION ALL SELECT 'application',application_id,name,generation,nonce,ciphertext FROM application_secret_versions WHERE available=1
            UNION ALL SELECT 'webhook',application_id,'webhook',1,nonce,ciphertext FROM webhook_secrets
            UNION ALL SELECT 'destination',id,'url',1,nonce,ciphertext FROM notification_destinations"#
        )
        .fetch(&mut *tx);
        let mut probed = Ciphertext::None;
        while let Some(row) = rows.try_next().await.map_err(StoreError::database)? {
            let Some(cipher) = cipher else {
                return Ok(Ciphertext::Undecryptable);
            };
            let envelope = Envelope {
                nonce: row.nonce,
                ciphertext: row.ciphertext,
            };
            let decrypt = |owner| cipher.decrypt(owner, &row.name, row.generation, &envelope);
            let decrypted = match row.kind.as_str() {
                "environment" => {
                    let id = EnvironmentId::parse(row.owner).map_err(StoreError::corrupt)?;
                    decrypt(SecretOwner::Environment(&id)).is_ok()
                }
                "destination" => decrypt(SecretOwner::NotificationDestination(&row.owner)).is_ok(),
                kind => {
                    let id = ApplicationId::parse(row.owner).map_err(StoreError::corrupt)?;
                    let owner = if kind == "webhook" {
                        SecretOwner::Webhook(&id)
                    } else {
                        SecretOwner::Application(&id)
                    };
                    decrypt(owner).is_ok()
                }
            };
            if decrypted {
                return Ok(Ciphertext::Decrypts);
            }
            probed = Ciphertext::Undecryptable;
        }
        Ok(probed)
    }

    /// Recovers from a lost master key by discarding every stored value, then
    /// moving the old key aside; the next value write generates a new key. The key
    /// is lost when it is missing or has the wrong length (see
    /// [`KeyFailure::lost`]) while something is encrypted, or decrypts nothing
    /// stored. A usable key is [`StoreError::SecretKeyUsable`]; one decrypting a
    /// value but not the verifier is [`KeyFailure::VerifierCorrupt`]. The author's
    /// actor needs `system:operate` and a recent passkey sign-in (see
    /// [`crate::Actor::require_fresh`]), checked in the transaction.
    /// # Errors
    /// Returns [`StoreError::SecretKeyUsable`] for a usable key or persistence errors.
    pub async fn recover_secret_key(
        &self,
        author: impl Into<crate::Author<'_>>,
    ) -> Result<SecretKeyRecovery, StoreError> {
        let author = author.into();
        let actor = author.actor;
        let mut tx = self.begin_immediate().await?;
        let operate = piqueld_core::access::Permission::Global(
            piqueld_core::access::GlobalPermission::SystemOperate,
        );
        actor.require_on(&mut tx, operate, None).await?;
        actor.require_fresh(&mut tx).await?;
        let verifier = Self::secret_key_verifier_on(&mut *tx).await?;
        let key = match SecretCipher::load(&self.secret_key_path, true) {
            Ok(cipher) => Ok(cipher),
            Err(error) => match error.key_failure() {
                Some(failure) if failure.lost() => Err(failure),
                _ => return Err(StoreError::SecretSource(error)),
            },
        };
        let cipher = key.as_ref().ok();
        if let (Some(cipher), Some(verifier)) = (cipher, &verifier)
            && cipher.verify(verifier).is_ok()
        {
            return Err(StoreError::SecretKeyUsable);
        }
        match (
            Self::probe_ciphertext_on(&mut tx, cipher).await?,
            verifier.is_some(),
            key,
        ) {
            (Ciphertext::Decrypts, true, _) => {
                Err(StoreError::SecretSource(KeyFailure::VerifierCorrupt.into()))
            }
            (Ciphertext::Decrypts, false, _)
            | (Ciphertext::None, false, Ok(_) | Err(KeyFailure::Missing)) => {
                Err(StoreError::SecretKeyUsable)
            }
            _ => self.discard_secret_values(tx, author).await,
        }
    }

    /// Discards every stored value and webhook secret in `tx`, the key
    /// recovery `author` requested, and moves the lost key aside once committed.
    async fn discard_secret_values(
        &self,
        mut tx: crate::WriteTransaction<'_>,
        author: crate::Author<'_>,
    ) -> Result<SecretKeyRecovery, StoreError> {
        let usage = sqlx::query!("SELECT COUNT(*) AS versions, COUNT(DISTINCT environment_id) AS environments, COUNT(DISTINCT environment_id||char(0)||name) AS secrets FROM secret_versions WHERE available=1")
            .fetch_one(&mut *tx).await.map_err(StoreError::database)?;
        let stored = sqlx::query!("SELECT COUNT(*) AS versions, COUNT(DISTINCT application_id) AS applications, COUNT(DISTINCT application_id||char(0)||name) AS secrets FROM application_secret_versions WHERE available=1")
            .fetch_one(&mut *tx).await.map_err(StoreError::database)?;
        let now = super::now_ms();
        let by = author.actor.attribution();
        let operator = by.operator_uid();
        let discarded = EventKind::SecretValuesDiscarded.as_str();
        sqlx::query!("INSERT INTO events(application_id,environment_id,kind,message,created_at_ms,actor_user_id,actor_credential_id,actor_operator_uid) SELECT (SELECT application_id FROM environments WHERE id=secret_versions.environment_id),environment_id,?5,'Secret key recovery discarded '||COUNT(*)||' stored values; store replacements, then deploy',?1,?2,?3,?4 FROM secret_versions WHERE available=1 GROUP BY environment_id",now,by.user_id,by.credential_id,operator,discarded)
            .execute(&mut *tx).await.map_err(StoreError::database)?;
        sqlx::query!("INSERT INTO events(application_id,kind,message,created_at_ms,actor_user_id,actor_credential_id,actor_operator_uid) SELECT application_id,?5,'Secret key recovery discarded '||COUNT(*)||' stored values; store replacements, then deploy',?1,?2,?3,?4 FROM application_secret_versions WHERE available=1 GROUP BY application_id",now,by.user_id,by.credential_id,operator,discarded)
            .execute(&mut *tx).await.map_err(StoreError::database)?;
        sqlx::query!(
            "UPDATE secret_versions SET available=0,nonce=X'',ciphertext=X'' WHERE available=1"
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        sqlx::query!(
            "UPDATE application_secret_versions SET available=0,nonce=X'',ciphertext=X'' WHERE available=1"
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let webhooks = sqlx::query!("DELETE FROM webhook_secrets")
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?
            .rows_affected();
        let destinations = sqlx::query_scalar!("SELECT COUNT(*) FROM notification_destinations")
            .fetch_one(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        let deleted = crate::DESTINATION_CHANGED;
        sqlx::query!(
            "UPDATE notification_deliveries
            SET state='cancelled',last_error=?2,updated_at_ms=?1
            WHERE state='pending'",
            now,
            deleted,
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        sqlx::query!("DELETE FROM notification_destinations")
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        sqlx::query!("DELETE FROM secret_key_verification")
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        let discarded = usage.versions + stored.versions;
        let counts = format!(
            "{discarded} values across {} environments and {} applications, {webhooks} webhook secrets, and {destinations} notification destinations",
            usage.environments, stored.applications
        );
        let message = format!("Recovered the secret master key; discarded {counts}");
        let recovered = crate::SecurityEvent::SecretKeyRecovered;
        Self::security_event_on(&mut tx, recovered, &message, by).await?;
        let change = format!("secret key recovery discarded {counts}");
        author
            .commit_recorded(tx, 200, Some(&change), (None, None))
            .await?;
        SecretCipher::retire(&self.secret_key_path, now).map_err(StoreError::SecretSource)?;
        Ok(SecretKeyRecovery {
            affected_environments: usage.environments,
            affected_applications: stored.applications,
            affected_secrets: usage.secrets + stored.secrets,
            discarded_versions: discarded,
            deleted_destinations: destinations,
        })
    }
}
