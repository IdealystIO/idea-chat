//! Accounts.

use uuid::Uuid;

use crate::{ids, model::*, Db, DbError, Result};

const COLS: &str =
    "id, handle, display_name, email, is_instance_admin, created_at, deleted_at";

impl Db {
    pub async fn create_user(&self, new: NewUser) -> Result<User> {
        let id = ids::new_id();
        sqlx::query_as::<_, User>(&format!(
            "INSERT INTO users (id, handle, display_name, email)
             VALUES ($1, $2, $3, $4) RETURNING {COLS}"
        ))
        .bind(id)
        .bind(&new.handle)
        .bind(&new.display_name)
        .bind(&new.email)
        .fetch_one(&self.pool)
        .await
        .map_err(DbError::from_unique)
    }

    pub async fn user(&self, id: Uuid) -> Result<Option<User>> {
        Ok(sqlx::query_as::<_, User>(&format!("SELECT {COLS} FROM users WHERE id = $1"))
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// Look up by whatever the person typed into the one identity field.
    ///
    /// Tombstoned accounts are excluded here rather than at the call site:
    /// every caller is a login path, and one that forgot would let a deleted
    /// account sign in.
    ///
    /// **The `::citext` casts are load-bearing.** Both columns are `citext`,
    /// but sqlx binds a Rust `&str` as `text`, and Postgres then resolves
    /// `citext = text` as a case-*sensitive* text comparison. Without the
    /// casts, the unique index still folds case on write — so `ANA` could not
    /// register while `ana` existed — but `ANA` could not sign in either. Half
    /// case-insensitive is the worst of both, and it looks correct in every
    /// test that only checks uniqueness.
    pub async fn user_by_login(&self, identifier: &str) -> Result<Option<User>> {
        Ok(sqlx::query_as::<_, User>(&format!(
            "SELECT {COLS} FROM users
             WHERE (handle = $1::citext OR email = $1::citext) AND deleted_at IS NULL"
        ))
        .bind(identifier)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// Whether this instance has any account at all — the question the
    /// first-run claim flow turns on.
    pub async fn has_any_user(&self) -> Result<bool> {
        let (n,): (i64,) = sqlx::query_as("SELECT count(*) FROM users")
            .fetch_one(&self.pool)
            .await?;
        Ok(n > 0)
    }

    /// Grant instance admin, returning the **updated** row.
    ///
    /// Returning it is the point. This used to return `()`, and the bootstrap
    /// flow then built its session from the `User` it had fetched before the
    /// promotion — so the first account was an admin in the database and a
    /// plain member in its own session, until it signed out and back in.
    pub async fn promote_to_instance_admin(&self, id: Uuid) -> Result<User> {
        Ok(sqlx::query_as::<_, User>(&format!(
            "UPDATE users SET is_instance_admin = true WHERE id = $1 RETURNING {COLS}"
        ))
        .bind(id)
        .fetch_one(&self.pool)
        .await?)
    }

    /// Tombstone. Messages keep pointing at the row, so history is not
    /// rewritten; the handle and email are released so they can be reused, and
    /// credentials cascade away so nothing can authenticate as this account.
    pub async fn delete_user(&self, id: Uuid) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM credentials WHERE user_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "UPDATE users
                SET deleted_at = now(),
                    handle = 'deleted_' || replace(id::text, '-', ''),
                    email = NULL,
                    display_name = 'Deleted account'
              WHERE id = $1 AND deleted_at IS NULL",
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
}
