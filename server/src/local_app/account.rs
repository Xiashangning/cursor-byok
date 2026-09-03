//! Integrates local Cursor account state.
use std::path::{Path, PathBuf};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::json;
use sqlx::{Connection, Row, SqliteConnection};

use crate::{Error, Result};

const EMAIL: &str = "cursor@ai.com";
const SIGN_UP_TYPE: &str = "Google";
const SUBJECT: &str = "cursor-local-user";
const MEMBERSHIP_TYPE: &str = "ultra";
const SUBSCRIPTION_STATUS: &str = "active";

pub async fn ensure_local_account() -> Result<()> {
    ensure_local_account_at(&state_db_path()?).await
}

fn state_db_path() -> Result<PathBuf> {
    let home = dirs::home_dir()
        .ok_or_else(|| Error::Config("cannot resolve user home directory".into()))?;
    match std::env::consts::OS {
        "macos" => {
            Ok(home.join("Library/Application Support/Cursor/User/globalStorage/state.vscdb"))
        }
        "windows" => Ok(std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData/Roaming"))
            .join("Cursor/User/globalStorage/state.vscdb")),
        "linux" => Ok(std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("Cursor/User/globalStorage/state.vscdb")),
        platform => Err(Error::Config(format!(
            "Cursor account injection is unsupported on {platform}"
        ))),
    }
}

async fn ensure_local_account_at(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true);
    let mut connection = SqliteConnection::connect_with(&options).await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB)",
    )
    .execute(&mut connection)
    .await?;

    let account = sqlx::query("SELECT CAST(value AS TEXT) AS value FROM ItemTable WHERE key = ?")
        .bind("cursorAuth/accessToken")
        .fetch_optional(&mut connection)
        .await?;
    let has_access_token = account.is_some_and(|row| {
        row.try_get::<String, _>("value")
            .is_ok_and(|value| !value.trim().is_empty())
    });

    let mut transaction = connection.begin().await?;
    if !has_access_token {
        let token = local_token()?;
        let values = [
            ("cursorAuth/accessToken", token.as_str()),
            ("cursorAuth/refreshToken", token.as_str()),
            ("cursorAuth/cachedEmail", EMAIL),
            ("cursorAuth/cachedSignUpType", SIGN_UP_TYPE),
        ];
        for (key, value) in values {
            sqlx::query("INSERT OR REPLACE INTO ItemTable(key, value) VALUES(?, ?)")
                .bind(key)
                .bind(value)
                .execute(&mut *transaction)
                .await?;
        }
    }

    for (key, value) in [
        ("cursorAuth/stripeMembershipType", MEMBERSHIP_TYPE),
        ("cursorAuth/stripeSubscriptionStatus", SUBSCRIPTION_STATUS),
    ] {
        sqlx::query("INSERT OR REPLACE INTO ItemTable(key, value) VALUES(?, ?)")
            .bind(key)
            .bind(value)
            .execute(&mut *transaction)
            .await?;
    }
    transaction.commit().await?;
    if !has_access_token {
        tracing::info!(
            email = EMAIL,
            subject = SUBJECT,
            "injected local Cursor account"
        );
    }
    Ok(())
}

fn local_token() -> Result<String> {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({
        "sub": SUBJECT,
        "email": EMAIL,
        "type": "session",
        "iss": "cursor-client",
        "scope": "openid profile email",
        "exp": 4070908800_u64
    }))?);
    Ok(format!("{header}.{payload}.{SUBJECT}"))
}
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn injects_the_local_account_only_when_missing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");

        ensure_local_account_at(&path).await.unwrap();

        let options = sqlx::sqlite::SqliteConnectOptions::new().filename(&path);
        let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
        let values = sqlx::query("SELECT key, CAST(value AS TEXT) AS value FROM ItemTable")
            .fetch_all(&mut connection)
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.get::<String, _>("key"), row.get::<String, _>("value")))
            .collect::<std::collections::HashMap<_, _>>();
        let token = &values["cursorAuth/accessToken"];
        assert_eq!(values["cursorAuth/refreshToken"], *token);
        assert_eq!(values["cursorAuth/cachedEmail"], EMAIL);
        assert_eq!(values["cursorAuth/cachedSignUpType"], SIGN_UP_TYPE);
        assert_eq!(values["cursorAuth/stripeMembershipType"], MEMBERSHIP_TYPE);
        assert_eq!(
            values["cursorAuth/stripeSubscriptionStatus"],
            SUBSCRIPTION_STATUS
        );
        let payload = token.split('.').nth(1).unwrap();
        let payload: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
        assert_eq!(payload["sub"], SUBJECT);
        assert_eq!(payload["email"], EMAIL);
        assert_eq!(payload["exp"], 4070908800_u64);

        sqlx::query("UPDATE ItemTable SET value = 'existing-token' WHERE key = ?")
            .bind("cursorAuth/accessToken")
            .execute(&mut connection)
            .await
            .unwrap();
        drop(connection);
        ensure_local_account_at(&path).await.unwrap();

        let options = sqlx::sqlite::SqliteConnectOptions::new().filename(&path);
        let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
        let token: String =
            sqlx::query_scalar("SELECT CAST(value AS TEXT) FROM ItemTable WHERE key = ?")
                .bind("cursorAuth/accessToken")
                .fetch_one(&mut connection)
                .await
                .unwrap();
        assert_eq!(token, "existing-token");
    }

    #[tokio::test]
    async fn refreshes_ultra_metadata_for_an_existing_cursor_account() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.vscdb");

        ensure_local_account_at(&path).await.unwrap();

        let options = sqlx::sqlite::SqliteConnectOptions::new().filename(&path);
        let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
        sqlx::query("UPDATE ItemTable SET value = ? WHERE key = ?")
            .bind("existing-token")
            .bind("cursorAuth/accessToken")
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query("UPDATE ItemTable SET value = ? WHERE key = ?")
            .bind("free")
            .bind("cursorAuth/stripeMembershipType")
            .execute(&mut connection)
            .await
            .unwrap();
        sqlx::query("DELETE FROM ItemTable WHERE key = ?")
            .bind("cursorAuth/stripeSubscriptionStatus")
            .execute(&mut connection)
            .await
            .unwrap();
        drop(connection);

        ensure_local_account_at(&path).await.unwrap();

        let mut connection = SqliteConnection::connect_with(&options).await.unwrap();
        let values = sqlx::query("SELECT key, CAST(value AS TEXT) AS value FROM ItemTable")
            .fetch_all(&mut connection)
            .await
            .unwrap()
            .into_iter()
            .map(|row| (row.get::<String, _>("key"), row.get::<String, _>("value")))
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(values["cursorAuth/accessToken"], "existing-token");
        assert_eq!(values["cursorAuth/stripeMembershipType"], MEMBERSHIP_TYPE);
        assert_eq!(
            values["cursorAuth/stripeSubscriptionStatus"],
            SUBSCRIPTION_STATUS
        );
    }
}
