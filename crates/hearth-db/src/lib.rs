//! Hearth's data layer.
//!
//! Owns the schema (`migrations/`), the domain types, and the queries behind
//! identity, spaces, membership and invites. Server-only: the wasm client
//! never links this crate, which is why it lives here rather than inside
//! `hearth` behind a feature.
//!
//! # Queries are runtime-checked, not `query!`-checked
//!
//! [`docs/data-model.md`](../../../docs/data-model.md) argued for sqlx partly
//! on compile-time-checked queries. This crate uses `query_as` with explicit
//! [`sqlx::FromRow`] structs instead, deliberately: `query!` needs either a
//! live database at build time or a committed `.sqlx` directory regenerated
//! on every query edit, and requiring Postgres to `cargo build` is a poor
//! trade for a project whose whole pitch is that self-hosting is easy.
//!
//! What we give up is caught by the integration suite instead, which runs
//! every query against a real Postgres — and catches things a macro cannot:
//! constraint behaviour, transaction semantics, and the concurrency around
//! invite redemption.
//!
//! # Tests
//!
//! `tests/` needs a real database and is skipped without one:
//!
//! ```sh
//! HEARTH_TEST_DATABASE_URL=postgres://postgres:dev@localhost:55432/hearth \
//!   cargo test -p hearth-db
//! ```

use sqlx::postgres::{PgPool, PgPoolOptions};

pub mod credentials;
pub mod ids;
pub mod instance;
pub mod invites;
pub mod model;
pub mod spaces;
pub mod users;

pub use model::*;

/// Everything that can go wrong in this crate.
///
/// The uniqueness variants exist because "handle taken" is a normal thing for
/// a user to do and the caller has to tell it apart from a real fault —
/// matching on a `sqlx::Error` string at the call site is how that goes wrong.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database: {0}")]
    Sql(#[from] sqlx::Error),
    #[error("migrations: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("that handle is already taken")]
    HandleTaken,
    #[error("that email is already registered")]
    EmailTaken,
    #[error("that slug is already taken")]
    SlugTaken,
    #[error("password hashing failed: {0}")]
    Hash(String),
    #[error("{0}")]
    Invalid(&'static str),
}

pub type Result<T> = std::result::Result<T, DbError>;

impl DbError {
    /// Map a unique-violation onto the field that actually collided.
    ///
    /// Postgres names the *constraint*, so the mapping lives next to the
    /// schema that names them. A collision we don't recognise stays a raw
    /// `Sql` error rather than being guessed at.
    pub(crate) fn from_unique(e: sqlx::Error) -> Self {
        let Some(db) = e.as_database_error() else { return DbError::Sql(e) };
        match db.constraint() {
            Some("users_handle_key") => DbError::HandleTaken,
            Some("users_email_key") => DbError::EmailTaken,
            Some("spaces_slug_key") => DbError::SlugTaken,
            _ => DbError::Sql(e),
        }
    }
}

/// A handle to the database. Cheap to clone — it shares one pool.
#[derive(Clone)]
pub struct Db {
    pool: PgPool,
}

impl Db {
    /// Connect and verify the connection eagerly.
    ///
    /// `max_connections` is small on purpose: this tier is IO-bound on a
    /// handful of indexed lookups per request, and an oversized pool mostly
    /// buys a way to exhaust Postgres's own connection limit during a
    /// thundering herd.
    pub async fn connect(url: &str) -> Result<Db> {
        let pool = PgPoolOptions::new().max_connections(16).connect(url).await?;
        Ok(Db { pool })
    }

    /// Apply any migrations this binary carries that the database has not
    /// seen. Embedded at compile time, so a deployed binary can always
    /// migrate the database it is pointed at without shipping .sql files
    /// alongside it.
    pub async fn migrate(&self) -> Result<()> {
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        Ok(())
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

impl Db {
    /// Append to the security trail. Callers treat a failure here as
    /// non-fatal — see `hearth::srv::audit`.
    pub async fn record_auth_event(
        &self,
        user_id: Option<uuid::Uuid>,
        kind: &str,
    ) -> Result<()> {
        sqlx::query("INSERT INTO auth_events (id, user_id, kind) VALUES ($1, $2, $3)")
            .bind(ids::new_id())
            .bind(user_id)
            .bind(kind)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
