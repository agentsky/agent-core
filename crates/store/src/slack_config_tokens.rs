//! `slack_config_tokens`: members' Slack app configuration tokens.
//!
//! A member registers a configuration token and its refresh token for a
//! workspace; agentd uses the token to create and edit that member's agent
//! apps, and renews both with `tooling.tokens.rotate` before the token's
//! 12 hours run out. One row per member and workspace.
//!
//! Every write of the tokens gives the row a new random
//! [`version`](SlackConfigTokenRef::version), and the calls that act on a
//! row read earlier take the version they read, so a rotation never
//! overwrites or breaks a token the member registered meanwhile.
//!
//! Rotation and the notice about a refused refresh token are claimed with a
//! lease (`lease_until`), so one caller at a time acts on a row across
//! processes and restarts. A caller that dies holding a claim leaves the row
//! to the next one once the lease ends.

use core_types::{MemberId, TeamId};
use secrecy::SecretString;
use time::OffsetDateTime;

use crate::seal::Aad;
use crate::{Result, Store, StoreError, from_unix, parse_column, to_unix};

const TABLE: &str = "slack_config_tokens";
const TOKEN: &str = "token_enc";
const REFRESH_TOKEN: &str = "refresh_token_enc";

/// A configuration token to store, with
/// [`Store::put_slack_config_token`] or
/// [`Store::update_rotated_slack_config_token`]. `Debug` redacts both
/// tokens.
#[derive(Debug)]
pub struct NewSlackConfigToken {
    /// The configuration token.
    pub token: SecretString,
    /// Its refresh token.
    pub refresh_token: SecretString,
    /// When the configuration token stops working.
    pub expires_at: OffsetDateTime,
}

/// A stored configuration token, decrypted. `Debug` redacts both tokens.
#[derive(Debug)]
pub struct SlackConfigToken {
    /// Which row, and the version read.
    pub row: SlackConfigTokenRef,
    /// The configuration token.
    pub token: SecretString,
    /// Its refresh token.
    pub refresh_token: SecretString,
    /// When the configuration token stops working.
    pub expires_at: OffsetDateTime,
}

/// A row and the version of its tokens that the caller read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackConfigTokenRef {
    /// The member who registered the token.
    pub member: MemberId,
    /// The workspace it acts in.
    pub team: TeamId,
    /// The version of the tokens, replaced on every write of them.
    pub version: String,
}

/// What `/agent me` says about a configuration token, read without
/// decrypting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlackConfigTokenStatus {
    /// When the configuration token stops working.
    pub expires_at: OffsetDateTime,
    /// Whether Slack refused to renew it.
    pub broken: bool,
}

fn aad_key(member: MemberId, team: &TeamId) -> String {
    format!("{member}:{team}")
}

fn aad<'a>(column: &'static str, key: &'a str) -> Aad<'a> {
    Aad {
        table: TABLE,
        column,
        key,
    }
}

fn new_version() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn attempt(value: i64) -> Result<u32> {
    u32::try_from(value).map_err(|_| StoreError::Corrupt {
        table: TABLE,
        column: "notice_attempts",
    })
}

impl Store {
    /// Stores `member`'s configuration token for `team` at `now`, replacing
    /// any they had there, with a new version and no lease, break or notice.
    /// Then ends the leases of the manifest updates of `member`'s Slack apps
    /// in `team`, so the new token updates them at once. That is best
    /// effort: a failure there is logged, and the token still counts as
    /// stored, since a lease left behind only delays an update by its hour.
    ///
    /// # Errors
    ///
    /// [`StoreError::Seal`] if a token can't be sealed,
    /// [`StoreError::Database`] if the member doesn't exist or the query
    /// fails.
    pub async fn put_slack_config_token(
        &self,
        member: MemberId,
        team: &TeamId,
        token: &NewSlackConfigToken,
        now: OffsetDateTime,
    ) -> Result<SlackConfigTokenRef> {
        let key = aad_key(member, team);
        let token_enc = self.seal(aad(TOKEN, &key), &token.token)?;
        let refresh_enc = self.seal(aad(REFRESH_TOKEN, &key), &token.refresh_token)?;
        let version = new_version();
        sqlx::query(
            "INSERT INTO slack_config_tokens \
             (member_id, team_id, token_enc, refresh_token_enc, expires_at, version, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT (member_id, team_id) DO UPDATE SET \
             token_enc = excluded.token_enc, refresh_token_enc = excluded.refresh_token_enc, \
             expires_at = excluded.expires_at, version = excluded.version, \
             updated_at = excluded.updated_at, lease_until = NULL, broken_at = NULL, \
             notified_at = NULL, notice_attempts = 0",
        )
        .bind(member.to_string())
        .bind(team.as_str())
        .bind(token_enc)
        .bind(refresh_enc)
        .bind(to_unix(token.expires_at))
        .bind(&version)
        .bind(to_unix(now))
        .execute(&self.pool)
        .await?;
        if let Err(err) = sqlx::query(
            "UPDATE agent_bindings SET manifest_lease_until = NULL \
             WHERE surface = 'slack' AND team_id = ? AND manifest_lease_until IS NOT NULL \
             AND agent_id IN (SELECT id FROM agents WHERE owner_id = ?)",
        )
        .bind(team.as_str())
        .bind(member.to_string())
        .execute(&self.pool)
        .await
        {
            tracing::warn!(%member, error = %err, "stored a configuration token but couldn't end its member's manifest update leases; their apps are updated once the leases end");
        }
        Ok(SlackConfigTokenRef {
            member,
            team: team.clone(),
            version,
        })
    }

    /// `member`'s configuration token for `team`, decrypted, broken or not.
    ///
    /// # Errors
    ///
    /// [`StoreError::Seal`] if a token fails to decrypt,
    /// [`StoreError::Database`] if the query fails.
    pub async fn slack_config_token(
        &self,
        member: MemberId,
        team: &TeamId,
    ) -> Result<Option<SlackConfigToken>> {
        let row: Option<(Vec<u8>, Vec<u8>, i64, String)> = sqlx::query_as(
            "SELECT token_enc, refresh_token_enc, expires_at, version FROM slack_config_tokens \
             WHERE member_id = ? AND team_id = ?",
        )
        .bind(member.to_string())
        .bind(team.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(token, refresh, expires_at, version)| {
            self.open_config_token(member, team, &token, &refresh, expires_at, version)
        })
        .transpose()
    }

    /// `member`'s configuration token for `team`, decrypted, if it can
    /// still be used at `now`: Slack hasn't refused to renew it, and it
    /// hasn't expired.
    ///
    /// # Errors
    ///
    /// As for [`slack_config_token`](Self::slack_config_token).
    pub async fn usable_slack_config_token(
        &self,
        member: MemberId,
        team: &TeamId,
        now: OffsetDateTime,
    ) -> Result<Option<SlackConfigToken>> {
        let row: Option<(Vec<u8>, Vec<u8>, i64, String)> = sqlx::query_as(
            "SELECT token_enc, refresh_token_enc, expires_at, version FROM slack_config_tokens \
             WHERE member_id = ? AND team_id = ? AND broken_at IS NULL AND expires_at > ?",
        )
        .bind(member.to_string())
        .bind(team.as_str())
        .bind(to_unix(now))
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(token, refresh, expires_at, version)| {
            self.open_config_token(member, team, &token, &refresh, expires_at, version)
        })
        .transpose()
    }

    fn open_config_token(
        &self,
        member: MemberId,
        team: &TeamId,
        token: &[u8],
        refresh: &[u8],
        expires_at: i64,
        version: String,
    ) -> Result<SlackConfigToken> {
        let key = aad_key(member, team);
        Ok(SlackConfigToken {
            token: self.open_sealed(aad(TOKEN, &key), token)?,
            refresh_token: self.open_sealed(aad(REFRESH_TOKEN, &key), refresh)?,
            expires_at: from_unix(expires_at, TABLE, "expires_at")?,
            row: SlackConfigTokenRef {
                member,
                team: team.clone(),
                version,
            },
        })
    }

    /// Whether `member` has a configuration token for `team`, when it
    /// expires and whether it is broken, without decrypting it.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn slack_config_token_status(
        &self,
        member: MemberId,
        team: &TeamId,
    ) -> Result<Option<SlackConfigTokenStatus>> {
        let row: Option<(i64, Option<i64>)> = sqlx::query_as(
            "SELECT expires_at, broken_at FROM slack_config_tokens \
             WHERE member_id = ? AND team_id = ?",
        )
        .bind(member.to_string())
        .bind(team.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(expires_at, broken_at)| {
            Ok(SlackConfigTokenStatus {
                expires_at: from_unix(expires_at, TABLE, "expires_at")?,
                broken: broken_at.is_some(),
            })
        })
        .transpose()
    }

    /// Deletes every configuration token of `member`, in every workspace,
    /// as `/agent logout` does. Returns how many were deleted.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn delete_slack_config_tokens(&self, member: MemberId) -> Result<u64> {
        let result = sqlx::query("DELETE FROM slack_config_tokens WHERE member_id = ?")
            .bind(member.to_string())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    /// Deletes `member`'s configuration token for `team`, as when they
    /// leave that workspace. Returns whether there was one.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn delete_slack_config_token(&self, member: MemberId, team: &TeamId) -> Result<bool> {
        let result =
            sqlx::query("DELETE FROM slack_config_tokens WHERE member_id = ? AND team_id = ?")
                .bind(member.to_string())
                .bind(team.as_str())
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Every token that expires before `renew_before` and may be claimed
    /// for rotation at `now`: not broken, and no lease runs past `now`.
    /// Soonest to expire first.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails,
    /// [`StoreError::Corrupt`] if a member id doesn't parse.
    pub async fn due_slack_config_tokens(
        &self,
        renew_before: OffsetDateTime,
        now: OffsetDateTime,
    ) -> Result<Vec<SlackConfigTokenRef>> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT member_id, team_id, version FROM slack_config_tokens \
             WHERE broken_at IS NULL AND expires_at < ? \
             AND (lease_until IS NULL OR lease_until <= ?) \
             ORDER BY expires_at, member_id, team_id",
        )
        .bind(to_unix(renew_before))
        .bind(to_unix(now))
        .fetch_all(&self.pool)
        .await?;
        rows_to_refs(rows)
    }

    /// Claims `row` for rotation at `now`, with a lease until
    /// `lease_until`, and returns its tokens; `None` if the row is gone,
    /// has a new version, is broken, or another lease runs past `now`.
    ///
    /// Follow a claim with
    /// [`update_rotated_slack_config_token`](Self::update_rotated_slack_config_token)
    /// or [`mark_slack_config_token_broken`](Self::mark_slack_config_token_broken).
    /// A claim that neither follows, because the rotation failed for a
    /// while or its caller died, lets the row be claimed again once the
    /// lease ends.
    ///
    /// # Errors
    ///
    /// [`StoreError::Seal`] if a token fails to decrypt (the lease is taken
    /// all the same), [`StoreError::Database`] if the query fails.
    pub async fn claim_slack_config_token(
        &self,
        row: &SlackConfigTokenRef,
        now: OffsetDateTime,
        lease_until: OffsetDateTime,
    ) -> Result<Option<SlackConfigToken>> {
        let claimed: Option<(Vec<u8>, Vec<u8>, i64)> = sqlx::query_as(
            "UPDATE slack_config_tokens SET lease_until = ? \
             WHERE member_id = ? AND team_id = ? AND version = ? AND broken_at IS NULL \
             AND (lease_until IS NULL OR lease_until <= ?) \
             RETURNING token_enc, refresh_token_enc, expires_at",
        )
        .bind(to_unix(lease_until))
        .bind(row.member.to_string())
        .bind(row.team.as_str())
        .bind(&row.version)
        .bind(to_unix(now))
        .fetch_optional(&self.pool)
        .await?;
        claimed
            .map(|(token, refresh, expires_at)| {
                self.open_config_token(
                    row.member,
                    &row.team,
                    &token,
                    &refresh,
                    expires_at,
                    row.version.clone(),
                )
            })
            .transpose()
    }

    /// Stores the tokens a rotation of `row` returned, at `now`, with a new
    /// version, the lease ended, and no break or notice. Returns the new
    /// version, or `None`, changing nothing, if the row is gone or has a
    /// version other than `row`'s (the member registered a new token
    /// meanwhile).
    ///
    /// A row marked broken since the claim is repaired: a claim whose lease
    /// ran out while Slack was rotating lets a second caller try the same,
    /// already used refresh token, and Slack's refusal marks the row
    /// broken, but the pair the first caller stores works.
    ///
    /// # Errors
    ///
    /// [`StoreError::Seal`] if a token can't be sealed,
    /// [`StoreError::Database`] if the query fails.
    pub async fn update_rotated_slack_config_token(
        &self,
        row: &SlackConfigTokenRef,
        token: &NewSlackConfigToken,
        now: OffsetDateTime,
    ) -> Result<Option<SlackConfigTokenRef>> {
        let key = aad_key(row.member, &row.team);
        let token_enc = self.seal(aad(TOKEN, &key), &token.token)?;
        let refresh_enc = self.seal(aad(REFRESH_TOKEN, &key), &token.refresh_token)?;
        let version = new_version();
        let result = sqlx::query(
            "UPDATE slack_config_tokens SET token_enc = ?, refresh_token_enc = ?, \
             expires_at = ?, version = ?, updated_at = ?, lease_until = NULL, \
             broken_at = NULL, notified_at = NULL, notice_attempts = 0 \
             WHERE member_id = ? AND team_id = ? AND version = ?",
        )
        .bind(token_enc)
        .bind(refresh_enc)
        .bind(to_unix(token.expires_at))
        .bind(&version)
        .bind(to_unix(now))
        .bind(row.member.to_string())
        .bind(row.team.as_str())
        .bind(&row.version)
        .execute(&self.pool)
        .await?;
        Ok((result.rows_affected() > 0).then(|| SlackConfigTokenRef {
            member: row.member,
            team: row.team.clone(),
            version,
        }))
    }

    /// Marks `row` broken at `now`, because Slack refused its refresh token,
    /// and ends the lease; its member is owed a notice. Returns false,
    /// changing nothing, if the row is gone, already broken, or has another
    /// version.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn mark_slack_config_token_broken(
        &self,
        row: &SlackConfigTokenRef,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE slack_config_tokens SET broken_at = ?, lease_until = NULL \
             WHERE member_id = ? AND team_id = ? AND version = ? AND broken_at IS NULL",
        )
        .bind(to_unix(now))
        .bind(row.member.to_string())
        .bind(row.team.as_str())
        .bind(&row.version)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Every broken token whose member is owed a notice that may be claimed
    /// at `now`: not told yet, no lease past `now`, and claimed fewer than
    /// `max_attempts` times. Oldest break first.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails,
    /// [`StoreError::Corrupt`] if a member id doesn't parse.
    pub async fn pending_slack_config_token_notices(
        &self,
        now: OffsetDateTime,
        max_attempts: u32,
    ) -> Result<Vec<SlackConfigTokenRef>> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT member_id, team_id, version FROM slack_config_tokens \
             WHERE broken_at IS NOT NULL AND notified_at IS NULL \
             AND (lease_until IS NULL OR lease_until <= ?) AND notice_attempts < ? \
             ORDER BY broken_at, member_id, team_id",
        )
        .bind(to_unix(now))
        .bind(i64::from(max_attempts))
        .fetch_all(&self.pool)
        .await?;
        rows_to_refs(rows)
    }

    /// Claims the notice owed for `row` at `now`, with a lease until
    /// `lease_until`, and returns which attempt this is, counting from 1;
    /// `None` if it may not be claimed (as for
    /// [`pending_slack_config_token_notices`](Self::pending_slack_config_token_notices))
    /// or the row has another version.
    ///
    /// A claim whose sender failed or died needs no call: the notice may be
    /// claimed again once the lease ends, until the attempts run out.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails,
    /// [`StoreError::Corrupt`] if the attempt count is negative.
    pub async fn claim_slack_config_token_notice(
        &self,
        row: &SlackConfigTokenRef,
        now: OffsetDateTime,
        lease_until: OffsetDateTime,
        max_attempts: u32,
    ) -> Result<Option<u32>> {
        let attempts: Option<i64> = sqlx::query_scalar(
            "UPDATE slack_config_tokens \
             SET lease_until = ?, notice_attempts = notice_attempts + 1 \
             WHERE member_id = ? AND team_id = ? AND version = ? \
             AND broken_at IS NOT NULL AND notified_at IS NULL \
             AND (lease_until IS NULL OR lease_until <= ?) AND notice_attempts < ? \
             RETURNING notice_attempts",
        )
        .bind(to_unix(lease_until))
        .bind(row.member.to_string())
        .bind(row.team.as_str())
        .bind(&row.version)
        .bind(to_unix(now))
        .bind(i64::from(max_attempts))
        .fetch_optional(&self.pool)
        .await?;
        attempts.map(attempt).transpose()
    }

    /// Records at `now` that `row`'s member was told, and ends the lease.
    /// Returns false if the notice isn't owed or the row has another
    /// version.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`] if the query fails.
    pub async fn mark_slack_config_token_notice_sent(
        &self,
        row: &SlackConfigTokenRef,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE slack_config_tokens SET notified_at = ?, lease_until = NULL \
             WHERE member_id = ? AND team_id = ? AND version = ? \
             AND broken_at IS NOT NULL AND notified_at IS NULL",
        )
        .bind(to_unix(now))
        .bind(row.member.to_string())
        .bind(row.team.as_str())
        .bind(&row.version)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
}

fn rows_to_refs(rows: Vec<(String, String, String)>) -> Result<Vec<SlackConfigTokenRef>> {
    rows.into_iter()
        .map(|(member, team, version)| {
            Ok(SlackConfigTokenRef {
                member: parse_column(&member, TABLE, "member_id")?,
                team: team.into(),
                version,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret;

    use super::*;
    use crate::SealError;
    use crate::test_util::*;

    const MAX: u32 = 3;

    fn team() -> TeamId {
        TeamId::new("T0TEAM001")
    }

    fn tokens(token: &str, expires_at: i64) -> NewSlackConfigToken {
        NewSlackConfigToken {
            token: SecretString::from(token),
            refresh_token: SecretString::from(format!("{token}-refresh")),
            expires_at: at(expires_at),
        }
    }

    async fn member(store: &Store, user: &str) -> MemberId {
        store
            .ensure_member(&member_key(user), user, at(1_000))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn only_a_token_neither_broken_nor_expired_is_usable() {
        let store = memory_store().await;
        let ada = member(&store, "ada").await;
        let workspace = team();
        let usable = |now| store.usable_slack_config_token(ada, &workspace, at(now));
        assert!(usable(1_000).await.unwrap().is_none());
        let row = store
            .put_slack_config_token(ada, &team(), &tokens("xoxe.a", 50_000), at(1_000))
            .await
            .unwrap();
        let read = usable(49_999).await.unwrap().unwrap();
        assert_eq!(read.token.expose_secret(), "xoxe.a");
        assert_eq!(read.row, row);
        assert!(usable(50_000).await.unwrap().is_none());
        assert!(
            store
                .usable_slack_config_token(ada, &TeamId::new("T0OTHER"), at(2_000))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .mark_slack_config_token_broken(&row, at(2_000))
                .await
                .unwrap()
        );
        assert!(usable(2_000).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_token_is_stored_sealed_and_read_back() {
        let store = memory_store().await;
        let ada = member(&store, "ada").await;
        let put = store
            .put_slack_config_token(ada, &team(), &tokens("xoxe.a", 50_000), at(1_000))
            .await
            .unwrap();
        let read = store
            .slack_config_token(ada, &team())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read.row, put);
        assert_eq!(read.token.expose_secret(), "xoxe.a");
        assert_eq!(read.refresh_token.expose_secret(), "xoxe.a-refresh");
        assert_eq!(read.expires_at, at(50_000));
        assert!(!format!("{read:?}").contains("xoxe.a"));

        let raw: (Vec<u8>, Vec<u8>) =
            sqlx::query_as("SELECT token_enc, refresh_token_enc FROM slack_config_tokens")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        for sealed in [raw.0, raw.1] {
            assert!(!String::from_utf8_lossy(&sealed).contains("xoxe.a"));
        }
        assert_eq!(
            store.slack_config_token_status(ada, &team()).await.unwrap(),
            Some(SlackConfigTokenStatus {
                expires_at: at(50_000),
                broken: false
            })
        );
        assert!(
            store
                .slack_config_token(ada, &TeamId::new("T0OTHER"))
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_sealed_token_moved_to_another_row_fails_to_decrypt() {
        let store = memory_store().await;
        let ada = member(&store, "ada").await;
        let bob = member(&store, "bob").await;
        store
            .put_slack_config_token(ada, &team(), &tokens("xoxe.a", 50_000), at(1_000))
            .await
            .unwrap();
        store
            .put_slack_config_token(bob, &team(), &tokens("xoxe.b", 50_000), at(1_000))
            .await
            .unwrap();
        sqlx::query(
            "UPDATE slack_config_tokens SET token_enc = \
             (SELECT token_enc FROM slack_config_tokens WHERE member_id = ?) \
             WHERE member_id = ?",
        )
        .bind(ada.to_string())
        .bind(bob.to_string())
        .execute(&store.pool)
        .await
        .unwrap();
        let err = store.slack_config_token(bob, &team()).await.unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Seal {
                    table: TABLE,
                    column: TOKEN,
                    source: SealError::Decrypt,
                }
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn putting_again_replaces_the_token_and_clears_its_state() {
        let store = memory_store().await;
        let ada = member(&store, "ada").await;
        let first = store
            .put_slack_config_token(ada, &team(), &tokens("xoxe.a", 2_000), at(1_000))
            .await
            .unwrap();
        assert!(
            store
                .mark_slack_config_token_broken(&first, at(1_500))
                .await
                .unwrap()
        );
        let second = store
            .put_slack_config_token(ada, &team(), &tokens("xoxe.b", 60_000), at(1_600))
            .await
            .unwrap();
        assert_ne!(first.version, second.version);
        let status = store
            .slack_config_token_status(ada, &team())
            .await
            .unwrap()
            .unwrap();
        assert!(!status.broken);
        assert!(
            store
                .pending_slack_config_token_notices(at(1_700), MAX)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            !store
                .mark_slack_config_token_broken(&first, at(1_700))
                .await
                .unwrap(),
            "the old version can't break the new token"
        );
    }

    #[tokio::test]
    async fn rotation_is_claimed_once_and_stored_under_a_new_version() {
        let store = memory_store().await;
        let ada = member(&store, "ada").await;
        let bob = member(&store, "bob").await;
        let row = store
            .put_slack_config_token(ada, &team(), &tokens("xoxe.a", 5_000), at(1_000))
            .await
            .unwrap();
        store
            .put_slack_config_token(bob, &team(), &tokens("xoxe.b", 90_000), at(1_000))
            .await
            .unwrap();

        let due = store
            .due_slack_config_tokens(at(10_000), at(3_000))
            .await
            .unwrap();
        assert_eq!(due, std::slice::from_ref(&row));

        let claimed = store
            .claim_slack_config_token(&row, at(3_000), at(3_300))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.refresh_token.expose_secret(), "xoxe.a-refresh");
        assert!(
            store
                .claim_slack_config_token(&row, at(3_100), at(3_400))
                .await
                .unwrap()
                .is_none(),
            "the lease keeps a second claim off"
        );
        assert!(
            store
                .due_slack_config_tokens(at(10_000), at(3_100))
                .await
                .unwrap()
                .is_empty()
        );

        let rotated = store
            .update_rotated_slack_config_token(&row, &tokens("xoxe.c", 50_000), at(3_200))
            .await
            .unwrap()
            .unwrap();
        assert_ne!(rotated.version, row.version);
        let read = store
            .slack_config_token(ada, &team())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read.token.expose_secret(), "xoxe.c");
        assert_eq!(read.row, rotated);
        assert!(
            store
                .update_rotated_slack_config_token(&row, &tokens("xoxe.d", 60_000), at(3_300))
                .await
                .unwrap()
                .is_none(),
            "a stale version writes nothing"
        );
        assert!(
            store
                .due_slack_config_tokens(at(10_000), at(3_300))
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn an_expired_lease_lets_the_row_be_claimed_again() {
        let store = memory_store().await;
        let ada = member(&store, "ada").await;
        let row = store
            .put_slack_config_token(ada, &team(), &tokens("xoxe.a", 5_000), at(1_000))
            .await
            .unwrap();
        store
            .claim_slack_config_token(&row, at(3_000), at(3_300))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .due_slack_config_tokens(at(10_000), at(3_300))
                .await
                .unwrap(),
            std::slice::from_ref(&row)
        );
        assert!(
            store
                .claim_slack_config_token(&row, at(3_300), at(3_600))
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn a_rotation_after_a_new_registration_changes_nothing() {
        let store = memory_store().await;
        let ada = member(&store, "ada").await;
        let row = store
            .put_slack_config_token(ada, &team(), &tokens("xoxe.a", 5_000), at(1_000))
            .await
            .unwrap();
        store
            .claim_slack_config_token(&row, at(3_000), at(3_300))
            .await
            .unwrap()
            .unwrap();
        store
            .put_slack_config_token(ada, &team(), &tokens("xoxe.new", 50_000), at(3_100))
            .await
            .unwrap();
        assert!(
            store
                .update_rotated_slack_config_token(&row, &tokens("xoxe.c", 60_000), at(3_200))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !store
                .mark_slack_config_token_broken(&row, at(3_200))
                .await
                .unwrap()
        );
        let read = store
            .slack_config_token(ada, &team())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(read.token.expose_secret(), "xoxe.new");
    }

    #[tokio::test]
    async fn a_late_rotation_repairs_a_row_its_lapsed_lease_let_break() {
        let store = memory_store().await;
        let ada = member(&store, "ada").await;
        let row = store
            .put_slack_config_token(ada, &team(), &tokens("xoxe.a", 5_000), at(1_000))
            .await
            .unwrap();
        store
            .claim_slack_config_token(&row, at(3_000), at(3_300))
            .await
            .unwrap()
            .unwrap();
        let second = store
            .claim_slack_config_token(&row, at(3_300), at(3_600))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second.row, row, "the lapsed lease lets a second claim in");
        assert!(
            store
                .mark_slack_config_token_broken(&row, at(3_310))
                .await
                .unwrap()
        );
        assert_eq!(
            store
                .claim_slack_config_token_notice(&row, at(3_320), at(3_920), MAX)
                .await
                .unwrap(),
            Some(1)
        );

        let rotated = store
            .update_rotated_slack_config_token(&row, &tokens("xoxe.c", 50_000), at(3_330))
            .await
            .unwrap()
            .unwrap();
        let status = store
            .slack_config_token_status(ada, &team())
            .await
            .unwrap()
            .unwrap();
        assert!(!status.broken);
        assert_eq!(status.expires_at, at(50_000));
        assert!(
            store
                .pending_slack_config_token_notices(at(9_000), MAX)
                .await
                .unwrap()
                .is_empty(),
            "no notice is owed for a working token"
        );
        assert_eq!(
            store
                .due_slack_config_tokens(at(60_000), at(3_340))
                .await
                .unwrap(),
            std::slice::from_ref(&rotated),
            "the repaired token is renewed again"
        );
        let attempts: i64 = sqlx::query_scalar("SELECT notice_attempts FROM slack_config_tokens")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(attempts, 0);
    }

    #[tokio::test]
    async fn a_broken_token_owes_one_notice_with_bounded_attempts() {
        let store = memory_store().await;
        let ada = member(&store, "ada").await;
        let row = store
            .put_slack_config_token(ada, &team(), &tokens("xoxe.a", 5_000), at(1_000))
            .await
            .unwrap();
        store
            .claim_slack_config_token(&row, at(3_000), at(3_300))
            .await
            .unwrap()
            .unwrap();
        assert!(
            store
                .mark_slack_config_token_broken(&row, at(3_010))
                .await
                .unwrap()
        );
        assert!(
            store
                .due_slack_config_tokens(at(10_000), at(3_020))
                .await
                .unwrap()
                .is_empty(),
            "a broken token isn't rotated"
        );
        assert!(
            store
                .slack_config_token_status(ada, &team())
                .await
                .unwrap()
                .unwrap()
                .broken
        );

        let mut now = 3_020;
        for expected in 1..=MAX {
            let pending = store
                .pending_slack_config_token_notices(at(now), MAX)
                .await
                .unwrap();
            assert_eq!(pending, std::slice::from_ref(&row));
            let claimed = store
                .claim_slack_config_token_notice(&row, at(now), at(now + 600), MAX)
                .await
                .unwrap();
            assert_eq!(claimed, Some(expected));
            assert!(
                store
                    .claim_slack_config_token_notice(&row, at(now + 1), at(now + 601), MAX)
                    .await
                    .unwrap()
                    .is_none()
            );
            now += 600;
        }
        assert!(
            store
                .pending_slack_config_token_notices(at(now), MAX)
                .await
                .unwrap()
                .is_empty(),
            "the attempts ran out"
        );
    }

    #[tokio::test]
    async fn a_sent_notice_is_not_owed_again() {
        let store = memory_store().await;
        let ada = member(&store, "ada").await;
        let row = store
            .put_slack_config_token(ada, &team(), &tokens("xoxe.a", 5_000), at(1_000))
            .await
            .unwrap();
        store
            .mark_slack_config_token_broken(&row, at(3_000))
            .await
            .unwrap();
        store
            .claim_slack_config_token_notice(&row, at(3_000), at(3_600), MAX)
            .await
            .unwrap()
            .unwrap();
        assert!(
            store
                .mark_slack_config_token_notice_sent(&row, at(3_010))
                .await
                .unwrap()
        );
        assert!(
            !store
                .mark_slack_config_token_notice_sent(&row, at(3_020))
                .await
                .unwrap()
        );
        assert!(
            store
                .pending_slack_config_token_notices(at(9_000), MAX)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn tokens_are_deleted_per_member_or_per_workspace() {
        let store = memory_store().await;
        let ada = member(&store, "ada").await;
        let bob = member(&store, "bob").await;
        let other = TeamId::new("T0OTHER");
        for (who, team) in [(ada, team()), (ada, other.clone()), (bob, team())] {
            store
                .put_slack_config_token(who, &team, &tokens("xoxe.x", 5_000), at(1_000))
                .await
                .unwrap();
        }
        assert!(store.delete_slack_config_token(ada, &other).await.unwrap());
        assert!(!store.delete_slack_config_token(ada, &other).await.unwrap());
        assert_eq!(store.delete_slack_config_tokens(ada).await.unwrap(), 1);
        assert_eq!(store.delete_slack_config_tokens(ada).await.unwrap(), 0);
        assert!(
            store
                .slack_config_token_status(bob, &team())
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn deleting_the_member_deletes_their_tokens() {
        let store = memory_store().await;
        let ada = member(&store, "ada").await;
        store
            .put_slack_config_token(ada, &team(), &tokens("xoxe.a", 5_000), at(1_000))
            .await
            .unwrap();
        sqlx::query("DELETE FROM members WHERE id = ?")
            .bind(ada.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM slack_config_tokens")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
}
