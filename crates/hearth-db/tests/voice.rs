//! Voice presence — who the sidebar says is in a room.

use hearth_db::{Db, NewUser, Role};
use uuid::Uuid;

async fn db() -> Option<Db> {
    let url = std::env::var("HEARTH_TEST_DATABASE_URL").ok()?;
    let db = Db::connect(&url).await.expect("connect");
    db.migrate().await.expect("migrate");
    Some(db)
}

fn unique(prefix: &str) -> String {
    format!("{prefix}{}", &Uuid::now_v7().simple().to_string()[20..32])
}

async fn a_user(db: &Db, p: &str) -> hearth_db::User {
    db.create_user(NewUser { handle: unique(p), display_name: format!("User {p}"), email: None })
        .await
        .unwrap()
}

/// Held by every test that sweeps, or that ages a row into being sweepable.
///
/// `voice_sweep` is deliberately global — it reaps whatever has expired, with
/// no notion of whose it was — so two tests running at once will happily reap
/// each other's rows and each find its own already gone. The lock is not
/// hiding a concurrency bug in the code; it is admitting that the operation
/// under test has no scope to isolate on.
static SWEEPING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take the sweep lock, surviving a poisoning from an unrelated failure — a
/// second test reporting "poisoned mutex" buries the first one's real message.
fn sweeping() -> std::sync::MutexGuard<'static, ()> {
    SWEEPING.lock().unwrap_or_else(|e| e.into_inner())
}

macro_rules! skip_without_db {
    () => {
        match db().await {
            Some(db) => db,
            None => return,
        }
    };
}

/// A space with two voice rooms, and its owner.
async fn a_space_with_rooms(db: &Db, owner: Uuid) -> (Uuid, Uuid, Uuid) {
    let space = db.create_space(&unique("v-"), "Space", owner).await.unwrap();
    let rooms: Vec<Uuid> = db
        .channels_in_space(space.id)
        .await
        .unwrap()
        .into_iter()
        .filter(|c| c.kind == hearth_db::ChannelKind::Voice)
        .map(|c| c.id)
        .collect();
    assert!(rooms.len() >= 2, "a space seeds at least two voice rooms");
    (space.id, rooms[0], rooms[1])
}

#[tokio::test]
async fn someone_in_a_room_shows_up_in_that_space() {
    let db = skip_without_db!();
    let ana = a_user(&db, "vp1").await;
    let (space, porch, _) = a_space_with_rooms(&db, ana.id).await;

    assert!(db.voice_in_space(space).await.unwrap().is_empty());

    db.voice_present(ana.id, porch).await.unwrap();
    let here = db.voice_in_space(space).await.unwrap();
    assert_eq!(here.len(), 1);
    assert_eq!(here[0].user_id, ana.id);
    assert_eq!(here[0].channel_id, porch);
    assert_eq!(here[0].display_name, ana.display_name);
    assert_eq!(here[0].role, Role::Owner);
}

/// The property the primary key exists to guarantee: you are in one call.
#[tokio::test]
async fn moving_to_another_room_does_not_leave_you_in_both() {
    let db = skip_without_db!();
    let ana = a_user(&db, "vp2").await;
    let (space, porch, movies) = a_space_with_rooms(&db, ana.id).await;

    db.voice_present(ana.id, porch).await.unwrap();
    db.voice_present(ana.id, movies).await.unwrap();

    let here = db.voice_in_space(space).await.unwrap();
    assert_eq!(here.len(), 1, "in two rooms at once: {here:?}");
    assert_eq!(here[0].channel_id, movies);
}

/// A heartbeat says "still here", not "arrived again" — otherwise a list
/// ordered by arrival would reshuffle itself every few seconds.
#[tokio::test]
async fn a_heartbeat_does_not_reset_when_you_arrived() {
    let db = skip_without_db!();
    let ana = a_user(&db, "vp3").await;
    let theo = a_user(&db, "vp4").await;
    let (space, porch, _) = a_space_with_rooms(&db, ana.id).await;
    // Membership comes from redeeming an invite; there is no back door, which
    // is the point — so the fixture uses the real path.
    let invite = db.create_invite(space, ana.id, 5).await.unwrap();
    db.redeem_invite(&invite.code, theo.id).await.unwrap();

    db.voice_present(ana.id, porch).await.unwrap();
    db.voice_present(theo.id, porch).await.unwrap();
    // Ana renews. She still got there first.
    db.voice_present(ana.id, porch).await.unwrap();

    let here = db.voice_in_space(space).await.unwrap();
    assert_eq!(here.len(), 2);
    assert_eq!(here[0].user_id, ana.id, "arrival order was not preserved");
    assert_eq!(here[1].user_id, theo.id);
}

/// A late "leave" for a room you already moved on from must not eject you from
/// the one you are actually in.
#[tokio::test]
async fn leaving_a_room_you_already_left_is_harmless() {
    let db = skip_without_db!();
    let ana = a_user(&db, "vp5").await;
    let (space, porch, movies) = a_space_with_rooms(&db, ana.id).await;

    db.voice_present(ana.id, porch).await.unwrap();
    db.voice_present(ana.id, movies).await.unwrap();
    db.voice_absent(ana.id, porch).await.unwrap();

    let here = db.voice_in_space(space).await.unwrap();
    assert_eq!(here.len(), 1, "a stale leave took her out of the wrong call");
    assert_eq!(here[0].channel_id, movies);

    db.voice_absent(ana.id, movies).await.unwrap();
    assert!(db.voice_in_space(space).await.unwrap().is_empty());
}

/// The ghost guard: presence is only presence while it is fresh.
#[tokio::test]
async fn a_stale_row_is_not_presence() {
    let _sweeping = sweeping();
    let db = skip_without_db!();
    let ana = a_user(&db, "vp6").await;
    let (space, porch, _) = a_space_with_rooms(&db, ana.id).await;

    db.voice_present(ana.id, porch).await.unwrap();
    assert_eq!(db.voice_in_space(space).await.unwrap().len(), 1);

    // Age the row past the TTL, as a closed tab would.
    sqlx::query("UPDATE voice_presence SET seen_at = now() - make_interval(secs => $1) WHERE user_id = $2")
        .bind((hearth_db::voice::PRESENCE_TTL_SECS + 5) as f64)
        .bind(ana.id)
        .execute(db.pool())
        .await
        .unwrap();

    assert!(
        db.voice_in_space(space).await.unwrap().is_empty(),
        "a tab closed badly leaves a ghost in the room"
    );
    // ...and a heartbeat brings them back without a re-join.
    db.voice_present(ana.id, porch).await.unwrap();
    assert_eq!(db.voice_in_space(space).await.unwrap().len(), 1);
}

#[tokio::test]
async fn another_spaces_room_is_not_in_this_ones_presence() {
    let db = skip_without_db!();
    let ana = a_user(&db, "vp7").await;
    let (mine, _, _) = a_space_with_rooms(&db, ana.id).await;
    let (theirs, their_porch, _) = a_space_with_rooms(&db, ana.id).await;

    db.voice_present(ana.id, their_porch).await.unwrap();

    assert!(db.voice_in_space(mine).await.unwrap().is_empty());
    assert_eq!(db.voice_in_space(theirs).await.unwrap().len(), 1);
}

#[tokio::test]
async fn the_sweep_removes_only_what_is_long_dead() {
    let _sweeping = sweeping();
    let db = skip_without_db!();
    let ana = a_user(&db, "vp8").await;
    let (space, porch, _) = a_space_with_rooms(&db, ana.id).await;

    db.voice_present(ana.id, porch).await.unwrap();
    db.voice_sweep().await.unwrap();
    assert_eq!(db.voice_in_space(space).await.unwrap().len(), 1, "swept a live row");

    age(&db, ana.id, hearth_db::voice::PRESENCE_TTL_SECS * 5).await;
    let gone = db.voice_sweep().await.unwrap();
    assert!(gone.iter().any(|(u, c, s)| *u == ana.id && *c == porch && *s == space));
}

/// Whether a sweep took `user`'s lease.
///
/// Every assertion about a sweep is scoped to the row under test, because the
/// sweep is not: it reaps whatever has expired anywhere, including rows other
/// tests left lying around, which are older than the TTL by the time a later
/// run reaches them. Asserting a sweep returned *nothing* was a test that
/// passed only on a clean database.
fn took(gone: &[(Uuid, Uuid, Uuid)], user: Uuid) -> bool {
    gone.iter().any(|(u, _, _)| *u == user)
}

/// Backdate a lease, to stand in for time passing.
async fn age(db: &Db, user: Uuid, secs: i64) {
    sqlx::query("UPDATE voice_presence SET seen_at = now() - make_interval(secs => $1) WHERE user_id = $2")
        .bind(secs as f64)
        .bind(user)
        .execute(db.pool())
        .await
        .unwrap();
}

// The bug this whole mechanism exists for. A live socket closing used to end
// the call outright, so a tunnel blip made everyone in the room hear a leave
// tone and then a join tone, and moved the person to the bottom of the list.
// Now it only shortens the lease — and a client that comes back renews it.
#[tokio::test]
async fn a_dropped_socket_does_not_end_a_call_that_comes_back() {
    let _sweeping = sweeping();
    let db = skip_without_db!();
    let ana = a_user(&db, "vp9").await;
    let (space, porch, _) = a_space_with_rooms(&db, ana.id).await;

    db.voice_present(ana.id, porch).await.unwrap();
    db.voice_fading(ana.id, porch).await.unwrap();
    assert_eq!(
        db.voice_in_space(space).await.unwrap().len(),
        1,
        "fading is not leaving — they are still in the room",
    );

    // The reconnect, well inside the grace period.
    let arrival = db.voice_present(ana.id, porch).await.unwrap();
    assert!(!arrival, "renewing a lease is not an arrival, so nobody is told");

    assert!(!took(&db.voice_sweep().await.unwrap(), ana.id), "her lease did not expire");
    assert_eq!(db.voice_in_space(space).await.unwrap().len(), 1);
}

// The other half: a socket that closes because the tab did. Nobody renews, and
// the sweeper collects it — once, with enough information to announce it.
#[tokio::test]
async fn a_socket_that_never_comes_back_expires_and_is_announced() {
    let _sweeping = sweeping();
    let db = skip_without_db!();
    let ana = a_user(&db, "vp10").await;
    let (space, porch, _) = a_space_with_rooms(&db, ana.id).await;

    db.voice_present(ana.id, porch).await.unwrap();
    db.voice_fading(ana.id, porch).await.unwrap();

    // The grace period runs out.
    age(&db, ana.id, hearth_db::voice::PRESENCE_TTL_SECS + 1).await;

    let gone = db.voice_sweep().await.unwrap();
    assert!(
        gone.iter().any(|(u, c, s)| *u == ana.id && *c == porch && *s == space),
        "the sweep says who left, and from where, so it can be announced",
    );
    assert!(db.voice_in_space(space).await.unwrap().is_empty());
    assert!(!took(&db.voice_sweep().await.unwrap(), ana.id), "announced exactly once");
}

// `voice_fading` must never *extend* a lease. A socket closing is not a
// heartbeat, and a client whose lease had already nearly run out must not get
// a fresh grace period out of its own disconnection.
#[tokio::test]
async fn fading_never_lengthens_a_lease() {
    let _sweeping = sweeping();
    let db = skip_without_db!();
    let ana = a_user(&db, "vp11").await;
    let (space, porch, _) = a_space_with_rooms(&db, ana.id).await;

    db.voice_present(ana.id, porch).await.unwrap();
    age(&db, ana.id, hearth_db::voice::PRESENCE_TTL_SECS + 1).await;
    db.voice_fading(ana.id, porch).await.unwrap();

    assert!(
        took(&db.voice_sweep().await.unwrap(), ana.id),
        "an already-expired lease stays expired",
    );
    assert!(db.voice_in_space(space).await.unwrap().is_empty());
}

// Scoped like `voice_absent`: a socket closing after its owner already moved
// rooms must not shorten the lease on the room they are actually in.
#[tokio::test]
async fn fading_the_wrong_room_leaves_the_real_one_alone() {
    let _sweeping = sweeping();
    let db = skip_without_db!();
    let ana = a_user(&db, "vp12").await;
    let (space, porch, movies) = a_space_with_rooms(&db, ana.id).await;

    db.voice_present(ana.id, movies).await.unwrap();
    db.voice_fading(ana.id, porch).await.unwrap();

    assert!(!took(&db.voice_sweep().await.unwrap(), ana.id));
    let here = db.voice_in_space(space).await.unwrap();
    assert_eq!(here.len(), 1);
    assert_eq!(here[0].channel_id, movies);
}
