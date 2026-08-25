//! Set an account's password directly against the database.
//!
//! ```text
//! DATABASE_URL=postgres://... cargo run -p hearth-db --example set_password -- nicho 'a passphrase'
//! ```
//!
//! Through [`Db::set_password`] rather than a SQL `UPDATE`, because the stored
//! secret is an argon2id PHC string with a per-password salt — there is no
//! literal to write — and because going through the real path means the length
//! rule applies here too.

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: set_password <handle> <password>  (with DATABASE_URL set)";
    let handle = args.next().ok_or(usage)?;
    let password = args.next().ok_or(usage)?;

    let url = std::env::var("DATABASE_URL").map_err(|_| "DATABASE_URL is not set")?;
    let db = hearth_db::Db::connect(&url).await?;

    let user = db
        .user_by_login(&handle)
        .await?
        .ok_or_else(|| format!("no account with handle or email {handle:?}"))?;
    db.set_password(user.id, &password).await?;

    println!("set password for {} ({})", user.handle, user.id);
    Ok(())
}
