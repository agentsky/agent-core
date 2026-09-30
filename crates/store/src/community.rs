//! `community_settings`: the community API key, the one setting a
//! community admin changes for everyone.

use core_types::MemberKey;
use secrecy::SecretString;
use time::OffsetDateTime;

use crate::seal::Aad;
use crate::{Result, Store, from_unix, parse_column, to_unix};

const TABLE: &str = "community_settings";
const API_KEY: &str = "api_key_enc";
/// The primary key of the table's one row.
const ROW: &str = "1";

fn api_key_aad() -> Aad<'static> {
    Aad {
        table: TABLE,
        column: API_KEY,
        key: ROW,
    }
}

/// Whether the community API key is set, and who last set or cleared it,
/// as [`community_api_key_status`](Store::community_api_key_status) returns
/// it. It never holds the key.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommunityKeyStatus {
    /// Whether a key is set.
    pub set: bool,
    /// The admin who last set or cleared it, if anyone has.
    pub changed_by: Option<MemberKey>,
    /// When they did.
    pub changed_at: Option<OffsetDateTime>,
}

impl Store {
    /// Seals `key` and stores it as the community API key, replacing any
    /// key set before, and records that `by` set it at `now`.
    ///
    /// # Errors
    ///
    /// [`StoreError::Seal`](crate::StoreError::Seal) if the key can't be
    /// sealed, [`StoreError::Database`](crate::StoreError::Database) if the
    /// query fails.
    pub async fn set_community_api_key(
        &self,
        key: &SecretString,
        by: &MemberKey,
        now: OffsetDateTime,
    ) -> Result<()> {
        let sealed = self.seal(api_key_aad(), key)?;
        sqlx::query(
            "INSERT INTO community_settings (id, api_key_enc, api_key_changed_by, \
             api_key_changed_at) VALUES (1, ?, ?, ?) \
             ON CONFLICT (id) DO UPDATE SET api_key_enc = excluded.api_key_enc, \
             api_key_changed_by = excluded.api_key_changed_by, \
             api_key_changed_at = excluded.api_key_changed_at",
        )
        .bind(sealed)
        .bind(by.to_string())
        .bind(to_unix(now))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Removes the community API key, recording that `by` cleared it at
    /// `now`. Returns whether a key was set; if none was, nothing changes.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn clear_community_api_key(
        &self,
        by: &MemberKey,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let result = sqlx::query(
            "UPDATE community_settings SET api_key_enc = NULL, api_key_changed_by = ?, \
             api_key_changed_at = ? WHERE id = 1 AND api_key_enc IS NOT NULL",
        )
        .bind(by.to_string())
        .bind(to_unix(now))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// The community API key, opened, or `None` if none is set.
    ///
    /// # Errors
    ///
    /// [`StoreError::Seal`](crate::StoreError::Seal) if the key fails to
    /// decrypt (a wrong master key, or a value moved from elsewhere),
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails.
    pub async fn community_api_key(&self) -> Result<Option<SecretString>> {
        let sealed: Option<Option<Vec<u8>>> =
            sqlx::query_scalar("SELECT api_key_enc FROM community_settings WHERE id = 1")
                .fetch_optional(&self.pool)
                .await?;
        sealed
            .flatten()
            .map(|sealed| self.open_sealed(api_key_aad(), &sealed))
            .transpose()
    }

    /// Whether the community API key is set, and who last changed it,
    /// without reading or decrypting the key.
    ///
    /// # Errors
    ///
    /// [`StoreError::Database`](crate::StoreError::Database) if the query
    /// fails, [`StoreError::Corrupt`](crate::StoreError::Corrupt) if the
    /// row doesn't parse.
    pub async fn community_api_key_status(&self) -> Result<CommunityKeyStatus> {
        let row: Option<(bool, Option<String>, Option<i64>)> = sqlx::query_as(
            "SELECT api_key_enc IS NOT NULL, api_key_changed_by, api_key_changed_at \
             FROM community_settings WHERE id = 1",
        )
        .fetch_optional(&self.pool)
        .await?;
        let Some((set, by, at)) = row else {
            return Ok(CommunityKeyStatus::default());
        };
        Ok(CommunityKeyStatus {
            set,
            changed_by: by
                .map(|by| parse_column(&by, TABLE, "api_key_changed_by"))
                .transpose()?,
            changed_at: at
                .map(|at| from_unix(at, TABLE, "api_key_changed_at"))
                .transpose()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret as _;

    use super::*;
    use crate::test_util::*;
    use crate::{SealError, StoreError};

    const KEY: &str = "sk-ant-api03-community-7f3e";

    #[tokio::test]
    async fn no_key_is_set_until_an_admin_sets_one() {
        let store = memory_store().await;
        assert!(store.community_api_key().await.unwrap().is_none());
        assert_eq!(
            store.community_api_key_status().await.unwrap(),
            CommunityKeyStatus::default()
        );
    }

    #[tokio::test]
    async fn a_set_key_is_read_back_replaced_and_cleared_with_who_changed_it() {
        let store = memory_store().await;
        let root = member_key("root");
        store
            .set_community_api_key(&SecretString::from(KEY), &root, at(1_000))
            .await
            .unwrap();
        assert_eq!(
            store
                .community_api_key()
                .await
                .unwrap()
                .unwrap()
                .expose_secret(),
            KEY
        );
        assert_eq!(
            store.community_api_key_status().await.unwrap(),
            CommunityKeyStatus {
                set: true,
                changed_by: Some(root.clone()),
                changed_at: Some(at(1_000)),
            }
        );

        let other = member_key("other-admin");
        store
            .set_community_api_key(&SecretString::from("sk-ant-new"), &other, at(2_000))
            .await
            .unwrap();
        assert_eq!(
            store
                .community_api_key()
                .await
                .unwrap()
                .unwrap()
                .expose_secret(),
            "sk-ant-new"
        );

        assert!(
            store
                .clear_community_api_key(&root, at(3_000))
                .await
                .unwrap()
        );
        assert!(store.community_api_key().await.unwrap().is_none());
        let status = store.community_api_key_status().await.unwrap();
        assert!(!status.set);
        assert_eq!(status.changed_by, Some(root.clone()));
        assert_eq!(status.changed_at, Some(at(3_000)));

        assert!(
            !store
                .clear_community_api_key(&other, at(4_000))
                .await
                .unwrap(),
            "clearing again changes nothing"
        );
        assert_eq!(
            store.community_api_key_status().await.unwrap().changed_by,
            Some(root)
        );
    }

    #[tokio::test]
    async fn the_key_is_sealed_at_rest_and_bound_to_its_column() {
        let store = memory_store().await;
        store
            .set_community_api_key(&SecretString::from(KEY), &member_key("root"), at(1_000))
            .await
            .unwrap();
        let raw: Vec<u8> =
            sqlx::query_scalar("SELECT api_key_enc FROM community_settings WHERE id = 1")
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert!(
            !raw.windows(KEY.len())
                .any(|window| window == KEY.as_bytes()),
            "the key is stored in the clear"
        );

        let member = store
            .ensure_member(&member_key("u1"), "Ada", at(1_000))
            .await
            .unwrap();
        store
            .put_pending_login("state", member, &SecretString::from("verifier"), at(1_000))
            .await
            .unwrap();
        sqlx::query(
            "UPDATE pending_logins SET verifier_enc = \
             (SELECT api_key_enc FROM community_settings WHERE id = 1)",
        )
        .execute(&store.pool)
        .await
        .unwrap();
        let err = store.take_pending_login("state").await.unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Seal {
                    source: SealError::Decrypt,
                    ..
                }
            ),
            "a sealed key moved to another column must not open: {err:?}"
        );
    }

    #[tokio::test]
    async fn a_key_sealed_under_another_master_key_fails_to_open_without_leaking() {
        let dir = TempDir::new();
        let store = Store::open(&dir.db_url(), sealer()).await.unwrap();
        store
            .set_community_api_key(&SecretString::from(KEY), &member_key("root"), at(1_000))
            .await
            .unwrap();
        drop(store);
        let store = Store::open(&dir.db_url(), sealer()).await.unwrap();
        let err = store.community_api_key().await.unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Seal {
                    table: "community_settings",
                    column: "api_key_enc",
                    source: SealError::Decrypt,
                }
            ),
            "{err:?}"
        );
        assert!(!format!("{err} {err:?}").contains(KEY));
        assert!(store.community_api_key_status().await.unwrap().set);
    }

    #[tokio::test]
    async fn a_missing_row_reads_as_unset_and_setting_brings_it_back() {
        let store = memory_store().await;
        sqlx::query("DELETE FROM community_settings")
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(store.community_api_key().await.unwrap().is_none());
        assert!(!store.community_api_key_status().await.unwrap().set);
        assert!(
            !store
                .clear_community_api_key(&member_key("root"), at(1_000))
                .await
                .unwrap()
        );
        store
            .set_community_api_key(&SecretString::from(KEY), &member_key("root"), at(2_000))
            .await
            .unwrap();
        assert!(store.community_api_key_status().await.unwrap().set);
    }

    #[tokio::test]
    async fn the_table_holds_one_row_at_most() {
        let store = memory_store().await;
        let err = sqlx::query("INSERT INTO community_settings (id) VALUES (2)")
            .execute(&store.pool)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("CHECK"), "{err}");
    }
}
