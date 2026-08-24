//! Messages, threads, and channel membership checks.

use uuid::Uuid;

use crate::{ids, model::*, Db, DbError, Result};

const COLS: &str = "m.id, m.channel_id, m.author_id, m.thread_root_id, m.body, \
                    m.reply_count, m.created_at, m.edited_at, m.deleted_at";

/// How many messages one page returns. Fixed rather than caller-supplied: a
/// client that could ask for 100_000 would be a trivial way to make the server
/// do unbounded work.
pub const PAGE: i64 = 50;

impl Db {
    /// Can this user see this channel at all?
    ///
    /// Both membership paths in one query — space channels through
    /// `space_members`, DMs through `channel_members` — because the caller
    /// should not have to know which kind of channel it is holding. Every read
    /// and write path goes through here; forgetting it on one endpoint is the
    /// whole vulnerability.
    pub async fn can_see_channel(&self, channel_id: Uuid, user_id: Uuid) -> Result<bool> {
        let (visible,): (bool,) = sqlx::query_as(
            "SELECT EXISTS (
               SELECT 1 FROM channels c
                LEFT JOIN space_members sm
                       ON sm.space_id = c.space_id AND sm.user_id = $2
                LEFT JOIN channel_members cm
                       ON cm.channel_id = c.id AND cm.user_id = $2
                WHERE c.id = $1
                  AND (sm.user_id IS NOT NULL OR cm.user_id IS NOT NULL)
             )",
        )
        .bind(channel_id)
        .bind(user_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(visible)
    }

    /// One page of a channel's root messages, newest first.
    ///
    /// `before` is a message id, not an offset. Because ids are UUIDv7 and
    /// therefore ordered, this is a single indexed range scan whose result is
    /// stable while people keep posting — an OFFSET would silently skip or
    /// repeat rows as the list grows underneath it.
    pub async fn channel_page(
        &self,
        channel_id: Uuid,
        before: Option<Uuid>,
    ) -> Result<Vec<Message>> {
        let mut rows = sqlx::query_as::<_, Message>(&format!(
            "SELECT {COLS} FROM messages m
              WHERE m.channel_id = $1
                AND m.thread_root_id IS NULL
                AND ($2::uuid IS NULL OR m.id < $2)
              ORDER BY m.id DESC
              LIMIT $3"
        ))
        .bind(channel_id)
        .bind(before)
        .bind(PAGE)
        .fetch_all(&self.pool)
        .await?;
        // Query newest-first so the index runs backwards from the cursor;
        // return oldest-first because that is reading order.
        rows.reverse();
        Ok(rows)
    }

    /// Everything after a cursor, for reconnect catch-up.
    ///
    /// Capped: a client that has been away for a month should refetch the
    /// channel cold rather than have the server assemble its entire history
    /// into one frame. `truncated` is how the caller learns to do that.
    pub async fn channel_since(
        &self,
        channel_id: Uuid,
        after: Uuid,
    ) -> Result<(Vec<Message>, bool)> {
        let rows = sqlx::query_as::<_, Message>(&format!(
            "SELECT {COLS} FROM messages m
              WHERE m.channel_id = $1 AND m.id > $2
              ORDER BY m.id
              LIMIT $3"
        ))
        .bind(channel_id)
        .bind(after)
        .bind(PAGE + 1)
        .fetch_all(&self.pool)
        .await?;

        let truncated = rows.len() as i64 > PAGE;
        Ok((rows.into_iter().take(PAGE as usize).collect(), truncated))
    }

    /// A thread: its root, then its replies in order.
    pub async fn thread(&self, root_id: Uuid) -> Result<Vec<Message>> {
        Ok(sqlx::query_as::<_, Message>(&format!(
            "SELECT {COLS} FROM messages m
              WHERE m.id = $1 OR m.thread_root_id = $1
              ORDER BY m.id"
        ))
        .bind(root_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// Post a message.
    ///
    /// The id comes from the caller, which is what makes a retry idempotent:
    /// re-sending after a dropped connection lands on the same primary key and
    /// returns the message already stored rather than posting it twice.
    ///
    /// The insert, the reply-count bump and the channel's `last_message_id`
    /// move together — a channel whose `last_message_id` disagreed with its
    /// newest message would show a permanent phantom unread.
    pub async fn post_message(
        &self,
        id: Uuid,
        channel_id: Uuid,
        author_id: Uuid,
        body: &str,
        thread_root_id: Option<Uuid>,
    ) -> Result<Message> {
        let body = body.trim();
        if body.is_empty() {
            return Err(DbError::Invalid("a message needs something in it"));
        }
        if body.chars().count() > 4000 {
            return Err(DbError::Invalid("that message is too long"));
        }
        ids::check_client_id(id, time::OffsetDateTime::now_utc())
            .map_err(|_| DbError::Invalid("that message id is not usable"))?;

        let mut tx = self.pool.begin().await?;

        // A resend of an id we already hold is the retry case, not a conflict.
        if let Some(existing) = sqlx::query_as::<_, Message>(&format!(
            "SELECT {COLS} FROM messages m WHERE m.id = $1"
        ))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        {
            return Ok(existing);
        }

        let message = sqlx::query_as::<_, Message>(&format!(
            "INSERT INTO messages (id, channel_id, author_id, body, thread_root_id)
             VALUES ($1, $2, $3, $4, $5)
             RETURNING {}",
            COLS.replace("m.", "")
        ))
        .bind(id)
        .bind(channel_id)
        .bind(author_id)
        .bind(body)
        .bind(thread_root_id)
        .fetch_one(&mut *tx)
        .await?;

        if let Some(root) = thread_root_id {
            sqlx::query("UPDATE messages SET reply_count = reply_count + 1 WHERE id = $1")
                .bind(root)
                .execute(&mut *tx)
                .await?;
        } else {
            sqlx::query("UPDATE channels SET last_message_id = $2 WHERE id = $1")
                .bind(channel_id)
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }

        tx.commit().await?;
        Ok(message)
    }

    /// Edit a message. Only its author may, and only while it exists.
    pub async fn edit_message(&self, id: Uuid, author_id: Uuid, body: &str) -> Result<Message> {
        let body = body.trim();
        if body.is_empty() {
            return Err(DbError::Invalid("a message needs something in it"));
        }
        sqlx::query_as::<_, Message>(&format!(
            "UPDATE messages SET body = $3, edited_at = now()
              WHERE id = $1 AND author_id = $2 AND deleted_at IS NULL
              RETURNING {}",
            COLS.replace("m.", "")
        ))
        .bind(id)
        .bind(author_id)
        .bind(body)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(DbError::Invalid("that message is not yours to edit"))
    }

    /// Tombstone a message: the row stays so replies keep a parent and the
    /// thread does not lose its root, but the text goes.
    ///
    /// The author may always delete their own; a moderator may delete anything
    /// in a space they moderate, which is the "remove this" a moderator needs.
    pub async fn delete_message(
        &self,
        id: Uuid,
        actor_id: Uuid,
        as_moderator: bool,
    ) -> Result<bool> {
        let sql = if as_moderator {
            "UPDATE messages SET deleted_at = now(), body = NULL
              WHERE id = $1 AND deleted_at IS NULL"
        } else {
            "UPDATE messages SET deleted_at = now(), body = NULL
              WHERE id = $1 AND author_id = $2 AND deleted_at IS NULL"
        };
        let done = sqlx::query(sql).bind(id).bind(actor_id).execute(&self.pool).await?;
        Ok(done.rows_affected() == 1)
    }

    /// Which space a channel belongs to, if any. DMs answer `None`, which is
    /// how a caller knows there is no moderator to appeal to.
    pub async fn channel_space(&self, channel_id: Uuid) -> Result<Option<Uuid>> {
        let row: Option<(Option<Uuid>,)> =
            sqlx::query_as("SELECT space_id FROM channels WHERE id = $1")
                .bind(channel_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.and_then(|(s,)| s))
    }

    /// Mark a channel read up to a message.
    pub async fn mark_read(&self, user_id: Uuid, channel_id: Uuid, upto: Uuid) -> Result<()> {
        sqlx::query(
            "INSERT INTO read_state (user_id, channel_id, last_read_message_id, mention_count)
             VALUES ($1, $2, $3, 0)
             ON CONFLICT (user_id, channel_id) DO UPDATE
               SET last_read_message_id = GREATEST(
                     read_state.last_read_message_id, EXCLUDED.last_read_message_id),
                   mention_count = 0,
                   updated_at = now()",
        )
        .bind(user_id)
        .bind(channel_id)
        .bind(upto)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Which of this user's channels have something unread.
    ///
    /// One query for every channel rather than one per channel: the room list
    /// needs all of it to render a single frame, so N+1 here would be N+1 on
    /// every navigation.
    pub async fn unread_channels(&self, user_id: Uuid) -> Result<Vec<Uuid>> {
        let rows: Vec<(Uuid,)> = sqlx::query_as(
            "SELECT c.id
               FROM channels c
               LEFT JOIN space_members sm ON sm.space_id = c.space_id AND sm.user_id = $1
               LEFT JOIN channel_members cm ON cm.channel_id = c.id AND cm.user_id = $1
               LEFT JOIN read_state r ON r.channel_id = c.id AND r.user_id = $1
              WHERE (sm.user_id IS NOT NULL OR cm.user_id IS NOT NULL)
                AND c.last_message_id IS NOT NULL
                AND (r.last_read_message_id IS NULL
                     OR c.last_message_id > r.last_read_message_id)",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// Find or create the DM between two people.
    ///
    /// `dm_key` is the sorted pair, so the unique index makes a duplicate
    /// impossible rather than unlikely — two people opening a DM with each
    /// other simultaneously converge on one channel instead of two.
    pub async fn dm_between(&self, a: Uuid, b: Uuid) -> Result<Channel> {
        if a == b {
            return Err(DbError::Invalid("you cannot open a conversation with yourself"));
        }
        let (lo, hi) = if a < b { (a, b) } else { (b, a) };
        let key = format!("{lo}:{hi}");

        if let Some(existing) = sqlx::query_as::<_, Channel>(
            "SELECT id, space_id, kind, name, topic, position, last_message_id
               FROM channels WHERE dm_key = $1",
        )
        .bind(&key)
        .fetch_optional(&self.pool)
        .await?
        {
            return Ok(existing);
        }

        let mut tx = self.pool.begin().await?;
        let created = sqlx::query_as::<_, Channel>(
            "INSERT INTO channels (id, kind, dm_key) VALUES ($1, 'dm', $2)
             ON CONFLICT (dm_key) DO UPDATE SET dm_key = EXCLUDED.dm_key
             RETURNING id, space_id, kind, name, topic, position, last_message_id",
        )
        .bind(ids::new_id())
        .bind(&key)
        .fetch_one(&mut *tx)
        .await?;

        for user in [lo, hi] {
            sqlx::query(
                "INSERT INTO channel_members (channel_id, user_id) VALUES ($1, $2)
                 ON CONFLICT DO NOTHING",
            )
            .bind(created.id)
            .bind(user)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(created)
    }
}
