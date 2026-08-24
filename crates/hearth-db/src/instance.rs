//! Instance-wide settings — one row, enforced by the schema.

use crate::{model::*, Db, Result};

impl Db {
    pub async fn instance_settings(&self) -> Result<InstanceSettings> {
        Ok(sqlx::query_as::<_, InstanceSettings>(
            "SELECT registration_mode, instance_name FROM instance_settings WHERE id = 1",
        )
        .fetch_one(&self.pool)
        .await?)
    }

    pub async fn set_registration_mode(&self, mode: RegistrationMode) -> Result<()> {
        sqlx::query("UPDATE instance_settings SET registration_mode = $1 WHERE id = 1")
            .bind(mode)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
