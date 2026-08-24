//! Message reads, writes, and who is allowed to do them.

use hearth_db::{ids, Db, DbError, NewUser, Role};
use uuid::Uuid;

async fn db() -> Option<Db> {
    let url = std::env::var("HEARTH_TEST_DATABASE_URL").ok()?;
    let db = Db::connect(&url).await.expect("connect");
    db.migrate().await.expect("migrate");
    Some(db)
}

fn unique(prefix: &str) -> String {
    // The TAIL of a v7 uuid, not the head: the head is the millisecond
    // timestamp, so two fixtures built in the same millisecond collide — which
    // is exactly what happens when the suite runs concurrently.
    format!("{prefix}{}", &Uuid::now_v7().simple().to_string()[20..32])
}

async fn a_user(db: &Db, p: &str) -> hearth_db::User {
    db.create_user(NewUser { handle: unique(p), display_name: format!("User {p}"), email: None })
        .await
        .unwrap()
}

macro_rules! skip_without_db {
    () => {
        match db().await {
            Some(db) => db,
            None => return,
        }
    };
}

/// A space with an owner and its first text channel.
async fn a_space(db: &Db, owner: Uuid) -> (Uuid, Uuid) {
    let space = db.create_space(&unique("s-"), "Space", owner).await.unwrap();
    let channel = db
        .channels_in_space(space.id)
        .await
        .unwrap()
        .into_iter()
        .find(|c| c.kind == hearth_db::ChannelKind::Text)
        .expect("a seeded text channel");
    (space.id, channel.id)
}

// ===========================================================================
// Visibility — the check every read and write path goes through
// ===========================================================================

#[tokio::test]
async fn a_member_can_see_a_space_channel_and_a_stranger_cannot() {
    let db = skip_without_db!();
    let owner = a_user(&db, "vis1").await;
    let stranger = a_user(&db, "vis2").await;
    let (_, channel) = a_space(&db, owner.id).await;

    assert!(db.can_see_channel(channel, owner.id).await.unwrap());
    assert!(!db.can_see_channel(channel, stranger.id).await.unwrap());
}

#[tokio::test]
async fn a_dm_is_visible_to_its_two_parties_and_nobody_else() {
    let db = skip_without_db!();
    let a = a_user(&db, "dm1").await;
    let b = a_user(&db, "dm2").await;
    let outsider = a_user(&db, "dm3").await;

    let dm = db.dm_between(a.id, b.id).await.unwrap();
    assert!(db.can_see_channel(dm.id, a.id).await.unwrap());
    assert!(db.can_see_channel(dm.id, b.id).await.unwrap());
    assert!(!db.can_see_channel(dm.id, outsider.id).await.unwrap());
}

/// Opening a DM twice — from either side — must land on one channel, or two
/// people end up talking into different rooms.
#[tokio::test]
async fn a_dm_is_the_same_channel_from_either_side() {
    let db = skip_without_db!();
    let a = a_user(&db, "sym1").await;
    let b = a_user(&db, "sym2").await;

    let one = db.dm_between(a.id, b.id).await.unwrap();
    let two = db.dm_between(b.id, a.id).await.unwrap();
    assert_eq!(one.id, two.id, "the pair is unordered; the channel must be one");
}

#[tokio::test]
async fn you_cannot_open_a_conversation_with_yourself() {
    let db = skip_without_db!();
    let a = a_user(&db, "self").await;
    assert!(matches!(db.dm_between(a.id, a.id).await, Err(DbError::Invalid(_))));
}

// ===========================================================================
// Posting
// ===========================================================================

#[tokio::test]
async fn a_message_can_be_posted_and_read_back() {
    let db = skip_without_db!();
    let owner = a_user(&db, "post").await;
    let (_, channel) = a_space(&db, owner.id).await;

    let id = ids::new_id();
    db.post_message(id, channel, owner.id, "  hello there  ", None).await.unwrap();

    let page = db.channel_page(channel, None).await.unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].body.as_deref(), Some("hello there"), "the body is trimmed");
}

/// **The idempotency claim.** A retry after a dropped connection must not
/// double-post — the client-minted id is what makes that true.
#[tokio::test]
async fn resending_the_same_id_returns_the_stored_message_rather_than_a_second_one() {
    let db = skip_without_db!();
    let owner = a_user(&db, "idem").await;
    let (_, channel) = a_space(&db, owner.id).await;

    let id = ids::new_id();
    let first = db.post_message(id, channel, owner.id, "only once", None).await.unwrap();
    let retry = db.post_message(id, channel, owner.id, "only once", None).await.unwrap();

    assert_eq!(first.id, retry.id);
    assert_eq!(db.channel_page(channel, None).await.unwrap().len(), 1);
}

#[tokio::test]
async fn an_empty_message_is_refused() {
    let db = skip_without_db!();
    let owner = a_user(&db, "empty").await;
    let (_, channel) = a_space(&db, owner.id).await;
    assert!(matches!(
        db.post_message(ids::new_id(), channel, owner.id, "   ", None).await,
        Err(DbError::Invalid(_))
    ));
}

/// A client that could mint a far-future id would pin its message to the top
/// of the channel permanently.
#[tokio::test]
async fn a_skewed_client_id_is_refused() {
    let db = skip_without_db!();
    let owner = a_user(&db, "skew").await;
    let (_, channel) = a_space(&db, owner.id).await;

    // A v4 uuid carries no timestamp, so it cannot be ordered at all.
    let bad = Uuid::parse_str("9f8e7d6c-5b4a-4392-8180-7f6e5d4c3b2a").unwrap();
    assert!(matches!(
        db.post_message(bad, channel, owner.id, "from nowhere", None).await,
        Err(DbError::Invalid(_))
    ));
}

// ===========================================================================
// Paging
// ===========================================================================

/// Paging is by id, not offset, so a page stays stable while people post.
#[tokio::test]
async fn paging_walks_backwards_through_history_without_repeating() {
    let db = skip_without_db!();
    let owner = a_user(&db, "page").await;
    let (_, channel) = a_space(&db, owner.id).await;

    for i in 0..120 {
        db.post_message(ids::new_id(), channel, owner.id, &format!("message {i}"), None)
            .await
            .unwrap();
    }

    let first = db.channel_page(channel, None).await.unwrap();
    assert_eq!(first.len(), 50);
    // Oldest-first within the page — that is reading order.
    assert!(first[0].id < first[49].id);
    // …and it is the NEWEST fifty.
    assert_eq!(first[49].body.as_deref(), Some("message 119"));

    let older = db.channel_page(channel, Some(first[0].id)).await.unwrap();
    assert_eq!(older.len(), 50);
    assert!(older[49].id < first[0].id, "pages must not overlap");

    let oldest = db.channel_page(channel, Some(older[0].id)).await.unwrap();
    assert_eq!(oldest.len(), 20, "the last page is short, which is how paging ends");
}

#[tokio::test]
async fn catch_up_reports_when_there_is_too_much_to_send() {
    let db = skip_without_db!();
    let owner = a_user(&db, "catch").await;
    let (_, channel) = a_space(&db, owner.id).await;

    let anchor = ids::new_id();
    db.post_message(anchor, channel, owner.id, "anchor", None).await.unwrap();
    for i in 0..60 {
        db.post_message(ids::new_id(), channel, owner.id, &format!("after {i}"), None)
            .await
            .unwrap();
    }

    let (rows, truncated) = db.channel_since(channel, anchor).await.unwrap();
    assert_eq!(rows.len(), 50);
    assert!(truncated, "the client must be told to refetch cold rather than trust a gap");
}

// ===========================================================================
// Threads
// ===========================================================================

#[tokio::test]
async fn a_reply_bumps_its_root_and_stays_out_of_the_channel_view() {
    let db = skip_without_db!();
    let owner = a_user(&db, "thr").await;
    let (_, channel) = a_space(&db, owner.id).await;

    let root = ids::new_id();
    db.post_message(root, channel, owner.id, "the question", None).await.unwrap();
    for i in 0..3 {
        db.post_message(ids::new_id(), channel, owner.id, &format!("reply {i}"), Some(root))
            .await
            .unwrap();
    }

    // The channel shows one message, not four — replies live in the thread.
    let page = db.channel_page(channel, None).await.unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].reply_count, 3, "the badge count is denormalised onto the root");

    // The thread shows the root plus its replies, in order.
    let thread = db.thread(root).await.unwrap();
    assert_eq!(thread.len(), 4);
    assert_eq!(thread[0].id, root);
}

// ===========================================================================
// Editing and deleting
// ===========================================================================

#[tokio::test]
async fn only_the_author_may_edit() {
    let db = skip_without_db!();
    let author = a_user(&db, "ed1").await;
    let other = a_user(&db, "ed2").await;
    let (_, channel) = a_space(&db, author.id).await;
    let id = ids::new_id();
    db.post_message(id, channel, author.id, "mine", None).await.unwrap();

    assert!(db.edit_message(id, other.id, "not yours").await.is_err());
    let edited = db.edit_message(id, author.id, "revised").await.unwrap();
    assert_eq!(edited.body.as_deref(), Some("revised"));
    assert!(edited.edited_at.is_some(), "an edit must be visible as an edit");
}

/// Deleting tombstones rather than removing the row, so a thread does not
/// lose its root and replies do not become orphans.
#[tokio::test]
async fn deleting_keeps_the_row_and_drops_the_text() {
    let db = skip_without_db!();
    let author = a_user(&db, "del").await;
    let (_, channel) = a_space(&db, author.id).await;
    let id = ids::new_id();
    db.post_message(id, channel, author.id, "regrettable", None).await.unwrap();

    assert!(db.delete_message(id, author.id, false).await.unwrap());

    let page = db.channel_page(channel, None).await.unwrap();
    assert_eq!(page.len(), 1, "the row survives");
    assert_eq!(page[0].body, None, "the text does not");
    assert!(page[0].deleted_at.is_some());
}

#[tokio::test]
async fn a_stranger_cannot_delete_but_a_moderator_can() {
    let db = skip_without_db!();
    let author = a_user(&db, "dm_a").await;
    let moderator = a_user(&db, "dm_m").await;
    let (space, channel) = a_space(&db, author.id).await;

    let invite = db.create_invite(space, author.id, 5).await.unwrap();
    db.redeem_invite(&invite.code, moderator.id).await.unwrap();
    db.set_role(space, moderator.id, Role::Moderator).await.unwrap();

    let id = ids::new_id();
    db.post_message(id, channel, author.id, "contentious", None).await.unwrap();

    // Not as themselves…
    assert!(!db.delete_message(id, moderator.id, false).await.unwrap());
    // …but yes, as a moderator.
    assert!(db.delete_message(id, moderator.id, true).await.unwrap());
}

// ===========================================================================
// Read state
// ===========================================================================

#[tokio::test]
async fn a_channel_reads_as_unread_until_it_is_marked() {
    let db = skip_without_db!();
    let owner = a_user(&db, "rd").await;
    let (_, channel) = a_space(&db, owner.id).await;

    let id = ids::new_id();
    db.post_message(id, channel, owner.id, "something new", None).await.unwrap();
    assert!(db.unread_channels(owner.id).await.unwrap().contains(&channel));

    db.mark_read(owner.id, channel, id).await.unwrap();
    assert!(!db.unread_channels(owner.id).await.unwrap().contains(&channel));
}

/// Marking read must never go backwards — two devices reporting out of order
/// would otherwise resurrect an unread badge you already cleared.
#[tokio::test]
async fn marking_read_never_moves_backwards() {
    let db = skip_without_db!();
    let owner = a_user(&db, "rd2").await;
    let (_, channel) = a_space(&db, owner.id).await;

    let older = ids::new_id();
    db.post_message(older, channel, owner.id, "first", None).await.unwrap();
    let newer = ids::new_id();
    db.post_message(newer, channel, owner.id, "second", None).await.unwrap();

    db.mark_read(owner.id, channel, newer).await.unwrap();
    db.mark_read(owner.id, channel, older).await.unwrap(); // a late, stale report
    assert!(
        !db.unread_channels(owner.id).await.unwrap().contains(&channel),
        "a stale mark-read must not resurrect the unread badge"
    );
}

/// A message in a channel you cannot see must never count as unread for you.
#[tokio::test]
async fn unread_only_covers_channels_you_belong_to() {
    let db = skip_without_db!();
    let owner = a_user(&db, "un1").await;
    let stranger = a_user(&db, "un2").await;
    let (_, channel) = a_space(&db, owner.id).await;
    db.post_message(ids::new_id(), channel, owner.id, "private", None).await.unwrap();

    assert!(!db.unread_channels(stranger.id).await.unwrap().contains(&channel));
}
