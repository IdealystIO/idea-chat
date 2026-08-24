//! Credentials — what proves who you are, once.
//!
//! Sessions are elsewhere: they ride `server_kit::Sessions` over Redis. This
//! module is only the durable half.

use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use password_hash::{PasswordHash, SaltString};
use rand::rngs::OsRng;
use uuid::Uuid;

use crate::{ids, Db, DbError, Result};

/// A real argon2id hash of a value nobody knows, verified against when the
/// account doesn't exist.
///
/// Without it, an unknown handle returns in microseconds and a known one takes
/// the full hashing time, which enumerates accounts by stopwatch. Generated
/// once at first use rather than hard-coded so it always matches current
/// parameters.
fn dummy_hash() -> &'static str {
    use std::sync::OnceLock;
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY.get_or_init(|| hash_password("not-a-real-password").expect("hash the dummy"))
}

/// argon2id at the OWASP floor: m=19456 KiB, t=2, p=1.
///
/// Encoded as a PHC string, so the parameters travel with the hash and can be
/// raised later without a migration — see [`needs_rehash`].
pub fn hash_password(plaintext: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(plaintext.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| DbError::Hash(e.to_string()))
}

pub fn verify_hash(hash: &str, plaintext: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else { return false };
    Argon2::default().verify_password(plaintext.as_bytes(), &parsed).is_ok()
}

/// Whether a stored hash was made with weaker parameters than we now use, in
/// which case a successful login is the moment to upgrade it silently.
pub fn needs_rehash(hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(hash) else { return true };
    let current = argon2::Params::DEFAULT_M_COST;
    parsed
        .params
        .get_decimal("m")
        .map(|m| m < current)
        .unwrap_or(true)
}

impl Db {
    pub async fn set_password(&self, user_id: Uuid, plaintext: &str) -> Result<()> {
        if plaintext.chars().count() < 10 {
            return Err(DbError::Invalid("passwords must be at least 10 characters"));
        }
        let hash = hash_password(plaintext)?;
        sqlx::query(
            "INSERT INTO credentials (id, user_id, kind, secret)
             VALUES ($1, $2, 'password', $3)
             ON CONFLICT (user_id, kind) WHERE kind IN ('password','totp')
             DO UPDATE SET secret = EXCLUDED.secret",
        )
        .bind(ids::new_id())
        .bind(user_id)
        .bind(&hash)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn password_hash(&self, user_id: Uuid) -> Result<Option<String>> {
        let row: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT secret FROM credentials WHERE user_id = $1 AND kind = 'password'",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|(s,)| s))
    }

    /// Check a password, upgrading the stored hash if our parameters have
    /// risen since it was written.
    pub async fn verify_password(&self, user_id: Uuid, plaintext: &str) -> Result<bool> {
        let Some(hash) = self.password_hash(user_id).await? else {
            // No password credential (an OIDC-only account). Still spend the
            // time, for the same reason as an unknown handle.
            verify_hash(dummy_hash(), plaintext);
            return Ok(false);
        };
        if !verify_hash(&hash, plaintext) {
            return Ok(false);
        }
        if needs_rehash(&hash) {
            self.set_password(user_id, plaintext).await?;
        }
        Ok(true)
    }

    /// Spend the same time as a real verification would.
    ///
    /// Call this on the "no such account" branch of login so the two branches
    /// are indistinguishable by timing.
    pub fn verify_absent_user(&self, plaintext: &str) {
        verify_hash(dummy_hash(), plaintext);
    }
}
