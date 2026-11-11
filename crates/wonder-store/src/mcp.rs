//! Persistent OAuth state for outside agents. Callers pass only SHA-256 hashes
//! of bearer credentials, authorization codes and Mac pairing codes.
use crate::Store;
use serde::Serialize;
use sqlx::Row;

#[derive(Clone, Debug, Serialize)]
pub struct Client {
    pub id: String,
    pub name: String,
    pub redirect_uris: Vec<String>,
    pub created_at_ms: i64,
    pub revoked_at_ms: Option<i64>,
}

#[derive(Clone)]
pub struct AuthorizationCode {
    pub hash: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub audience: String,
    pub scopes: String,
    pub permission_mode: String,
    pub expires_at_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub id: String,
    pub client_id: String,
    pub client_name: String,
    pub audience: String,
    pub scopes: String,
    pub permission_mode: String,
    pub expires_at_ms: i64,
}

pub struct TokenHashes<'a> {
    pub access: &'a str,
    pub refresh: &'a str,
    pub access_expires_at_ms: i64,
}

/// Nonsecret metadata for the Mac's agent review/revocation UI.
#[derive(Serialize)]
pub struct GrantSummary {
    pub id: String,
    pub client_id: String,
    pub audience: String,
    pub scopes: String,
    pub permission_mode: String,
    pub expires_at_ms: i64,
    pub revoked_at_ms: Option<i64>,
}

fn client(row: sqlx::sqlite::SqliteRow) -> Client {
    Client {
        id: row.get("id"),
        name: row.get("name"),
        redirect_uris: serde_json::from_str(row.get("redirect_uris")).unwrap_or_default(),
        created_at_ms: row.get("created_at_ms"),
        revoked_at_ms: row.get("revoked_at_ms"),
    }
}
fn grant(row: &sqlx::sqlite::SqliteRow) -> Grant {
    Grant {
        id: row.get("id"),
        client_id: row.get("client_id"),
        client_name: row.get("name"),
        audience: row.get("audience"),
        scopes: row.get("scopes"),
        permission_mode: row.get("permission_mode"),
        expires_at_ms: row.get("expires_at_ms"),
    }
}

impl Store {
    pub async fn mcp_register_client(&self, value: &Client) -> Result<bool, sqlx::Error> {
        // Bound unauthenticated registration storage. Expired, never-approved
        // registrations can be discarded; approved/revoked clients remain auditable.
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM mcp_authorization_codes WHERE expires_at_ms <= ?")
            .bind(value.created_at_ms)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM mcp_clients WHERE created_at_ms < ? AND NOT EXISTS (SELECT 1 FROM mcp_grants WHERE client_id=mcp_clients.id) AND NOT EXISTS (SELECT 1 FROM mcp_authorization_codes WHERE client_id=mcp_clients.id)")
            .bind(value.created_at_ms - 86_400_000).execute(&mut *tx).await?;
        let inserted = sqlx::query("INSERT INTO mcp_clients(id,name,redirect_uris,created_at_ms) SELECT ?,?,?,? WHERE (SELECT COUNT(*) FROM mcp_clients) < 1024")
            .bind(&value.id).bind(&value.name).bind(serde_json::to_string(&value.redirect_uris).unwrap())
            .bind(value.created_at_ms).execute(&mut *tx).await?.rows_affected() == 1;
        tx.commit().await?;
        Ok(inserted)
    }
    pub async fn mcp_client(&self, id: &str) -> Result<Option<Client>, sqlx::Error> {
        Ok(sqlx::query("SELECT * FROM mcp_clients WHERE id=?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .map(client))
    }
    pub async fn mcp_clients(&self) -> Result<Vec<Client>, sqlx::Error> {
        Ok(
            sqlx::query("SELECT * FROM mcp_clients ORDER BY created_at_ms DESC")
                .fetch_all(&self.pool)
                .await?
                .into_iter()
                .map(client)
                .collect(),
        )
    }
    pub async fn mcp_grants(&self) -> Result<Vec<GrantSummary>, sqlx::Error> {
        Ok(sqlx::query("SELECT * FROM mcp_grants")
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| GrantSummary {
                id: row.get("id"),
                client_id: row.get("client_id"),
                audience: row.get("audience"),
                scopes: row.get("scopes"),
                permission_mode: row.get("permission_mode"),
                expires_at_ms: row.get("expires_at_ms"),
                revoked_at_ms: row.get("revoked_at_ms"),
            })
            .collect())
    }
    pub async fn mcp_revoke_client(&self, id: &str, now: i64) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let found = sqlx::query(
            "UPDATE mcp_clients SET revoked_at_ms=COALESCE(revoked_at_ms,?) WHERE id=?",
        )
        .bind(now)
        .bind(id)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            > 0;
        sqlx::query("DELETE FROM mcp_authorization_codes WHERE client_id=?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(found)
    }
    /// A newly displayed Mac code replaces every older code. Only its hash persists.
    pub async fn mcp_pairing_code(&self, hash: &str, expires: i64) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM mcp_pairing_codes")
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO mcp_pairing_codes VALUES(?,?)")
            .bind(hash)
            .bind(expires)
            .execute(&mut *tx)
            .await?;
        tx.commit().await
    }
    /// Consuming owner consent and minting a code are one transaction.
    pub async fn mcp_approve(
        &self,
        pairing_hash: &str,
        code: &AuthorizationCode,
        now: i64,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let consumed = sqlx::query("DELETE FROM mcp_pairing_codes WHERE code_hash=? AND expires_at_ms>? RETURNING code_hash")
            .bind(pairing_hash).bind(now).fetch_optional(&mut *tx).await?.is_some();
        if !consumed {
            return Ok(false);
        }
        let inserted = sqlx::query("INSERT INTO mcp_authorization_codes SELECT ?,?,?,?,?,?,?,? WHERE EXISTS (SELECT 1 FROM mcp_clients WHERE id=? AND revoked_at_ms IS NULL)")
            .bind(&code.hash).bind(&code.client_id).bind(&code.redirect_uri).bind(&code.code_challenge)
            .bind(&code.audience).bind(&code.scopes).bind(&code.permission_mode).bind(code.expires_at_ms)
            .bind(&code.client_id).execute(&mut *tx).await?.rows_affected() == 1;
        tx.commit().await?;
        Ok(inserted)
    }
    /// Spend even expired or incorrectly presented codes. Concurrent/replayed
    /// exchanges cannot mint a second grant.
    pub async fn mcp_take_code(
        &self,
        hash: &str,
    ) -> Result<Option<AuthorizationCode>, sqlx::Error> {
        Ok(
            sqlx::query("DELETE FROM mcp_authorization_codes WHERE code_hash=? RETURNING *")
                .bind(hash)
                .fetch_optional(&self.pool)
                .await?
                .map(|row| AuthorizationCode {
                    hash: row.get("code_hash"),
                    client_id: row.get("client_id"),
                    redirect_uri: row.get("redirect_uri"),
                    code_challenge: row.get("code_challenge"),
                    audience: row.get("audience"),
                    scopes: row.get("scopes"),
                    permission_mode: row.get("permission_mode"),
                    expires_at_ms: row.get("expires_at_ms"),
                }),
        )
    }
    pub async fn mcp_issue_grant(
        &self,
        value: &Grant,
        tokens: &TokenHashes<'_>,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query("INSERT INTO mcp_grants(id,client_id,audience,scopes,permission_mode,expires_at_ms) SELECT ?,?,?,?,?,? WHERE EXISTS(SELECT 1 FROM mcp_clients WHERE id=? AND revoked_at_ms IS NULL)")
            .bind(&value.id).bind(&value.client_id).bind(&value.audience).bind(&value.scopes)
            .bind(&value.permission_mode).bind(value.expires_at_ms).bind(&value.client_id)
            .execute(&mut *tx).await?.rows_affected() == 1;
        if !inserted {
            return Ok(false);
        }
        insert_tokens(&mut tx, value, tokens).await?;
        tx.commit().await?;
        Ok(true)
    }
    pub async fn mcp_access(
        &self,
        hash: &str,
        audience: &str,
        now: i64,
    ) -> Result<Option<Grant>, sqlx::Error> {
        Ok(sqlx::query("SELECT g.*,c.name FROM mcp_tokens t JOIN mcp_grants g ON g.id=t.grant_id JOIN mcp_clients c ON c.id=g.client_id WHERE t.token_hash=? AND t.kind='access' AND t.expires_at_ms>? AND g.expires_at_ms>? AND g.audience=? AND g.revoked_at_ms IS NULL AND c.revoked_at_ms IS NULL")
            .bind(hash).bind(now).bind(now).bind(audience).fetch_optional(&self.pool).await?.as_ref().map(grant))
    }
    /// Rotation is atomic. Reusing a spent refresh token revokes its family,
    /// including the access token returned by the previous successful rotation.
    pub async fn mcp_refresh(
        &self,
        hash: &str,
        client_id: &str,
        audience: &str,
        now: i64,
        tokens: &TokenHashes<'_>,
        requested_scopes: Option<&str>,
    ) -> Result<Option<Grant>, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        // Acquire SQLite's writer before reading, including across processes.
        sqlx::query("UPDATE mcp_tokens SET used_at_ms=used_at_ms WHERE token_hash=?")
            .bind(hash)
            .execute(&mut *tx)
            .await?;
        let row = sqlx::query("SELECT g.*,c.name,t.used_at_ms,t.expires_at_ms AS token_expiry FROM mcp_tokens t JOIN mcp_grants g ON g.id=t.grant_id JOIN mcp_clients c ON c.id=g.client_id WHERE t.token_hash=? AND t.kind='refresh' AND g.client_id=? AND g.audience=? AND g.revoked_at_ms IS NULL AND c.revoked_at_ms IS NULL")
            .bind(hash).bind(client_id).bind(audience).fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let mut value = grant(&row);
        if row.get::<Option<i64>, _>("used_at_ms").is_some() {
            sqlx::query("UPDATE mcp_grants SET revoked_at_ms=? WHERE id=?")
                .bind(now)
                .bind(&value.id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(None);
        }
        if row.get::<i64, _>("token_expiry") <= now || value.expires_at_ms <= now {
            return Ok(None);
        }
        if let Some(scopes) = requested_scopes {
            if !scopes
                .split_whitespace()
                .all(|s| value.scopes.split_whitespace().any(|g| g == s))
            {
                return Ok(None);
            }
            // Narrow the whole family so future refreshes cannot recover removed scopes.
            value.scopes = scopes.to_owned();
            sqlx::query("UPDATE mcp_grants SET scopes=? WHERE id=?")
                .bind(scopes)
                .bind(&value.id)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("UPDATE mcp_tokens SET used_at_ms=? WHERE token_hash=?")
            .bind(now)
            .bind(hash)
            .execute(&mut *tx)
            .await?;
        // Old access tokens expire normally; all share the current narrowed grant.
        sqlx::query("DELETE FROM mcp_tokens WHERE kind='access' AND expires_at_ms<=?")
            .bind(now)
            .execute(&mut *tx)
            .await?;
        insert_tokens(&mut tx, &value, tokens).await?;
        tx.commit().await?;
        Ok(Some(value))
    }
}
async fn insert_tokens(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    grant: &Grant,
    tokens: &TokenHashes<'_>,
) -> Result<(), sqlx::Error> {
    for (hash, kind, expiry) in [
        (
            tokens.access,
            "access",
            tokens.access_expires_at_ms.min(grant.expires_at_ms),
        ),
        (tokens.refresh, "refresh", grant.expires_at_ms),
    ] {
        sqlx::query(
            "INSERT INTO mcp_tokens(token_hash,grant_id,kind,expires_at_ms) VALUES(?,?,?,?)",
        )
        .bind(hash)
        .bind(&grant.id)
        .bind(kind)
        .bind(expiry)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn fixture() -> Store {
        let s = Store::connect("sqlite::memory:").await.unwrap();
        s.mcp_register_client(&Client {
            id: "client".into(),
            name: "CLI".into(),
            redirect_uris: vec!["http://localhost/callback".into()],
            created_at_ms: 0,
            revoked_at_ms: None,
        })
        .await
        .unwrap();
        s
    }
    fn authorized() -> Grant {
        Grant {
            id: "grant".into(),
            client_id: "client".into(),
            client_name: "CLI".into(),
            audience: "https://mac.ts.net/mcp".into(),
            scopes: "read message".into(),
            permission_mode: "workspace".into(),
            expires_at_ms: 500,
        }
    }
    fn tokens<'a>(a: &'a str, r: &'a str) -> TokenHashes<'a> {
        TokenHashes {
            access: a,
            refresh: r,
            access_expires_at_ms: 100,
        }
    }
    #[tokio::test]
    async fn mcp_tokens_expire_and_bind_audience_and_client_revocation() {
        let s = fixture().await;
        let g = authorized();
        assert!(s
            .mcp_issue_grant(&g, &tokens("access-hash", "refresh-hash"))
            .await
            .unwrap());
        assert!(s
            .mcp_access("access-hash", &g.audience, 99)
            .await
            .unwrap()
            .is_some());
        assert!(s
            .mcp_access("access-hash", &g.audience, 100)
            .await
            .unwrap()
            .is_none());
        assert!(s
            .mcp_access("access-hash", "https://other/mcp", 1)
            .await
            .unwrap()
            .is_none());
        for (client, audience, at) in [
            ("different-client", g.audience.as_str(), 1),
            ("client", "https://other/mcp", 1),
            ("client", g.audience.as_str(), 500),
        ] {
            assert!(s
                .mcp_refresh(
                    "refresh-hash",
                    client,
                    audience,
                    at,
                    &tokens("unused-a", "unused-r"),
                    None
                )
                .await
                .unwrap()
                .is_none());
        }
        s.mcp_revoke_client("client", 2).await.unwrap();
        assert!(s
            .mcp_access("access-hash", &g.audience, 3)
            .await
            .unwrap()
            .is_none());
        assert!(s
            .mcp_refresh(
                "refresh-hash",
                "client",
                &g.audience,
                3,
                &tokens("a2", "r2"),
                None
            )
            .await
            .unwrap()
            .is_none());
    }
    #[tokio::test]
    // Single use and replay are owned over HTTP (mcp_http_tests); only an
    // expired pairing code is store-only.
    async fn an_expired_mcp_pairing_code_cannot_approve() {
        let s = fixture().await;
        let c = AuthorizationCode {
            hash: "code-hash".into(),
            client_id: "client".into(),
            redirect_uri: "http://localhost/callback".into(),
            code_challenge: "challenge".into(),
            audience: "resource".into(),
            scopes: "read".into(),
            permission_mode: "read_only".into(),
            expires_at_ms: 60,
        };
        s.mcp_pairing_code("expired-pair", 2).await.unwrap();
        assert!(!s.mcp_approve("expired-pair", &c, 2).await.unwrap());
        assert!(s.mcp_take_code("code-hash").await.unwrap().is_none());
    }
}
