//! `pending_logins`: PKCE logins started but not yet completed.

use core_types::MemberId;
use secrecy::SecretString;
use time::OffsetDateTime;

use crate::seal::Aad;
use crate::{Result, Store, from_unix, parse_column, to_unix};

const TABLE: &str = "pending_logins";
const VERIFIER: &str = "verifier_enc";

/// A pending login, as [`Store::take_pending_login`] returns it.
#[derive(Debug)]
pub struct PendingLogin {
    /// The member who started the login.
    pub member: MemberId,
    /// The PKCE code verifier.
    pub verifier: SecretString,
    /// When the login stops being valid.
    pub expires_at: OffsetDateTime,
}

impl PendingLogin {
    /// Whether the login has expired at `now`.
    pub fn is_expired(&self, now: OffsetDateTime) -> bool {
        now >= self.expires_at
    }
}

fn aad(state: &str) -> Aad<'_> {
    Aad {
        table: TABLE,
        column: VERIFIER,
        key: state,
    }
}

impl Store {
    /// Stores a pending login for `member`, keyed by its OAuth `state`, and
    /// deletes the member's other pending logins, so a member has at most
    /// one.
    ///
    /// It is one `BEGIN IMMEDIATE` transaction, so under concurrent callers
    /// for one member exactly one pending login is left.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if `state` is
    /// already in use by another member, the member doesn't exist, or a
    /// query fails.
    pub async fn put_pending_login(
        &self,
        state: &str,
        member: MemberId,
        verifier: &SecretString,
        expires_at: OffsetDateTime,
    ) -> Result<()> {
        let verifier = self.seal(aad(state), verifier)?;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("DELETE FROM pending_logins WHERE member_id = ?")
            .bind(member.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO pending_logins (state, member_id, verifier_enc, expires_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(state)
        .bind(member.to_string())
        .bind(verifier)
        .bind(to_unix(expires_at))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Deletes the pending login for `state` and returns it.
    ///
    /// It is one `DELETE … RETURNING` statement, so under concurrent callers
    /// exactly one gets the row. An expired login is still returned, so the
    /// caller can tell the member it expired; check
    /// [`PendingLogin::is_expired`] before using it.
    ///
    /// # Errors
    ///
    /// [`StoreError::Seal`](crate::StoreError::Seal) if the verifier fails to
    /// decrypt (the row is deleted all the same),
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn take_pending_login(&self, state: &str) -> Result<Option<PendingLogin>> {
        let rows: Vec<(String, Vec<u8>, i64)> = sqlx::query_as(
            "DELETE FROM pending_logins WHERE state = ? \
             RETURNING member_id, verifier_enc, expires_at",
        )
        .bind(state)
        .fetch_all(&self.pool)
        .await?;
        let Some((member, verifier, expires_at)) = rows.into_iter().next() else {
            return Ok(None);
        };
        Ok(Some(PendingLogin {
            member: parse_column(&member, TABLE, "member_id")?,
            verifier: self.open_sealed(aad(state), &verifier)?,
            expires_at: from_unix(expires_at, TABLE, "expires_at")?,
        }))
    }

    /// Deletes every pending login of `member`, for example after they pasted
    /// a code in public. Returns how many were deleted.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn invalidate_pending_logins(&self, member: MemberId) -> Result<u64> {
        let result = sqlx::query("DELETE FROM pending_logins WHERE member_id = ?")
            .bind(member.to_string())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret;

    use super::*;
    use crate::test_util::*;
    use crate::{SealError, StoreError};

    async fn store_with_member() -> (Store, MemberId) {
        let store = memory_store().await;
        let member = add_member(&store, "u1").await;
        (store, member)
    }

    async fn add_member(store: &Store, user: &str) -> MemberId {
        store
            .ensure_member(&member_key(user), "Ada", at(1_000))
            .await
            .unwrap()
    }

    async fn pending_count(store: &Store, member: MemberId) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM pending_logins WHERE member_id = ?")
            .bind(member.to_string())
            .fetch_one(&store.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn take_returns_the_login_once() {
        let (store, member) = store_with_member().await;
        store
            .put_pending_login("s1", member, &SecretString::from("verifier-1"), at(5_000))
            .await
            .unwrap();
        let login = store.take_pending_login("s1").await.unwrap().unwrap();
        assert_eq!(login.member, member);
        assert_eq!(login.verifier.expose_secret(), "verifier-1");
        assert_eq!(login.expires_at, at(5_000));
        assert!(store.take_pending_login("s1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn take_is_none_for_an_unknown_state() {
        let (store, _) = store_with_member().await;
        assert!(store.take_pending_login("nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn an_expired_login_is_returned_and_says_so() {
        let (store, member) = store_with_member().await;
        store
            .put_pending_login("s1", member, &SecretString::from("v"), at(5_000))
            .await
            .unwrap();
        let login = store.take_pending_login("s1").await.unwrap().unwrap();
        assert!(!login.is_expired(at(4_999)));
        assert!(login.is_expired(at(5_000)));
        assert!(login.is_expired(at(6_000)));
    }

    #[tokio::test]
    async fn a_state_cannot_be_reused() {
        let (store, alice) = store_with_member().await;
        let bob = add_member(&store, "bob").await;
        let verifier = SecretString::from("v");
        store
            .put_pending_login("s1", alice, &verifier, at(5_000))
            .await
            .unwrap();
        let err = store
            .put_pending_login("s1", bob, &verifier, at(5_000))
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::Database(_)), "{err:?}");
        assert_eq!(pending_count(&store, bob).await, 0);
        let login = store.take_pending_login("s1").await.unwrap().unwrap();
        assert_eq!(login.member, alice);
    }

    #[tokio::test]
    async fn put_replaces_the_members_earlier_login() {
        let (store, alice) = store_with_member().await;
        let bob = add_member(&store, "bob").await;
        let verifier = SecretString::from("v");
        for (state, member) in [("a1", alice), ("b1", bob), ("a2", alice)] {
            store
                .put_pending_login(state, member, &verifier, at(5_000))
                .await
                .unwrap();
        }
        assert!(store.take_pending_login("a1").await.unwrap().is_none());
        assert!(store.take_pending_login("a2").await.unwrap().is_some());
        assert!(store.take_pending_login("b1").await.unwrap().is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_puts_leave_one_login() {
        let dir = TempDir::new();
        let store = Store::open(&dir.db_url(), sealer()).await.unwrap();
        let member = add_member(&store, "u1").await;
        let verifier = SecretString::from("v");
        for round in 0..10 {
            let states: Vec<String> = (0..6).map(|i| format!("r{round}-s{i}")).collect();
            let results = tokio::join!(
                store.put_pending_login(&states[0], member, &verifier, at(5_000)),
                store.put_pending_login(&states[1], member, &verifier, at(5_000)),
                store.put_pending_login(&states[2], member, &verifier, at(5_000)),
                store.put_pending_login(&states[3], member, &verifier, at(5_000)),
                store.put_pending_login(&states[4], member, &verifier, at(5_000)),
                store.put_pending_login(&states[5], member, &verifier, at(5_000)),
            );
            for result in [
                results.0, results.1, results.2, results.3, results.4, results.5,
            ] {
                result.unwrap();
            }
            assert_eq!(pending_count(&store, member).await, 1, "round {round}");
        }
    }

    #[tokio::test]
    async fn the_verifier_is_stored_encrypted() {
        let (store, member) = store_with_member().await;
        store
            .put_pending_login(
                "s1",
                member,
                &SecretString::from("verifier-plain"),
                at(5_000),
            )
            .await
            .unwrap();
        let sealed: Vec<u8> = sqlx::query_scalar("SELECT verifier_enc FROM pending_logins")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert!(!sealed.windows(14).any(|w| w == b"verifier-plain"));
    }

    #[tokio::test]
    async fn invalidate_deletes_only_that_members_logins() {
        let (store, alice) = store_with_member().await;
        let bob = add_member(&store, "bob").await;
        let verifier = SecretString::from("v");
        for (state, member) in [("a1", alice), ("b1", bob)] {
            store
                .put_pending_login(state, member, &verifier, at(5_000))
                .await
                .unwrap();
        }
        assert_eq!(store.invalidate_pending_logins(alice).await.unwrap(), 1);
        assert_eq!(store.invalidate_pending_logins(alice).await.unwrap(), 0);
        assert!(store.take_pending_login("a1").await.unwrap().is_none());
        assert!(store.take_pending_login("b1").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_verifier_moved_to_another_state_fails_to_decrypt() {
        let (store, alice) = store_with_member().await;
        let bob = add_member(&store, "bob").await;
        for (state, verifier, member) in [("s1", "v1", alice), ("s2", "v2", bob)] {
            store
                .put_pending_login(state, member, &SecretString::from(verifier), at(5_000))
                .await
                .unwrap();
        }
        sqlx::query(
            "UPDATE pending_logins SET verifier_enc = \
             (SELECT verifier_enc FROM pending_logins WHERE state = 's1') WHERE state = 's2'",
        )
        .execute(&store.pool)
        .await
        .unwrap();
        let err = store.take_pending_login("s2").await.unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Seal {
                    table: TABLE,
                    column: VERIFIER,
                    source: SealError::Decrypt,
                }
            ),
            "{err:?}"
        );
        assert!(store.take_pending_login("s2").await.unwrap().is_none());
        let login = store.take_pending_login("s1").await.unwrap().unwrap();
        assert_eq!(login.verifier.expose_secret(), "v1");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn take_returns_a_row_exactly_once_under_concurrent_callers() {
        let dir = TempDir::new();
        let store = Store::open(&dir.db_url(), sealer()).await.unwrap();
        let member = add_member(&store, "u1").await;
        for round in 0..20 {
            let state = format!("state-{round}");
            store
                .put_pending_login(&state, member, &SecretString::from("v"), at(5_000))
                .await
                .unwrap();
            let results = tokio::join!(
                store.take_pending_login(&state),
                store.take_pending_login(&state),
                store.take_pending_login(&state),
                store.take_pending_login(&state),
                store.take_pending_login(&state),
                store.take_pending_login(&state),
                store.take_pending_login(&state),
                store.take_pending_login(&state),
            );
            let results = [
                results.0, results.1, results.2, results.3, results.4, results.5, results.6,
                results.7,
            ];
            let taken = results
                .into_iter()
                .map(|result| result.unwrap())
                .filter(Option::is_some)
                .count();
            assert_eq!(taken, 1, "round {round}");
        }
    }
}
