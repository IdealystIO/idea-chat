//! Integration tests against a real Postgres.
//!
//! These exist because the interesting behaviour in this crate is not in the
//! Rust — it is in the constraints and the transactions. A mock would assert
//! that the code does what the code does.
//!
//! ```sh
//! HEARTH_TEST_DATABASE_URL=postgres://postgres:dev@localhost:55432/hearth \
//!   cargo test -p hearth-db
//! ```
//!
//! Without that variable every test skips rather than fails, so `cargo test`
//! on a machine with no database is still green — the suite is opt-in
//! infrastructure, not a broken build.

use hearth_db::{ChannelKind, Db, DbError, InviteRefusal, NewUser, Redemption, Role};
use uuid::Uuid;

/// Connect, migrate, and hand back a handle — or `None` when no test database
/// is configured, which every test treats as "skip".
async fn db() -> Option<Db> {
    let url = std::env::var("HEARTH_TEST_DATABASE_URL").ok()?;
    let db = Db::connect(&url).await.expect("connect to the test database");
    db.migrate().await.expect("migrate the test database");
    Some(db)
}

/// A handle nobody else in the suite will collide with. Tests share one
/// database and run concurrently, so every fixture has to be unique — and
/// truncating between tests would serialise the suite for no benefit.
fn unique(prefix: &str) -> String {
    // The TAIL of a v7 uuid, not the head: the head is the millisecond
    // timestamp, so two fixtures built in the same millisecond collide — which
    // is exactly what happens when the suite runs concurrently.
    let id = Uuid::now_v7().simple().to_string();
    format!("{prefix}{}", &id[20..32])
}

async fn a_user(db: &Db, prefix: &str) -> hearth_db::User {
    db.create_user(NewUser {
        handle: unique(prefix),
        display_name: "Test Person".into(),
        email: None,
    })
    .await
    .expect("create a user")
}

macro_rules! skip_without_db {
    () => {
        match db().await {
            Some(db) => db,
            None => {
                eprintln!("skipping: HEARTH_TEST_DATABASE_URL is not set");
                return;
            }
        }
    };
}

// ===========================================================================
// Identity
// ===========================================================================

#[tokio::test]
async fn a_handle_is_claimed_case_insensitively() {
    let db = skip_without_db!();
    let handle = unique("ana");
    db.create_user(NewUser { handle: handle.clone(), display_name: "Ana".into(), email: None })
        .await
        .unwrap();

    // The impostor differs only in case. citext is what makes this fail, and
    // the point of the test is that nothing in Rust has to remember to fold.
    let err = db
        .create_user(NewUser {
            handle: handle.to_uppercase(),
            display_name: "Impostor".into(),
            email: None,
        })
        .await
        .expect_err("a case-variant handle must not be a second account");
    assert!(matches!(err, DbError::HandleTaken), "got {err:?}");
}

#[tokio::test]
async fn login_lookup_accepts_either_a_handle_or_an_email() {
    let db = skip_without_db!();
    let handle = unique("theo");
    let email = format!("{}@example.test", unique("t"));
    let user = db
        .create_user(NewUser {
            handle: handle.clone(),
            display_name: "Theo".into(),
            email: Some(email.clone()),
        })
        .await
        .unwrap();

    assert_eq!(db.user_by_login(&handle).await.unwrap().map(|u| u.id), Some(user.id));
    assert_eq!(db.user_by_login(&email).await.unwrap().map(|u| u.id), Some(user.id));
    // …and case-insensitively on both, which is the whole reason both columns
    // are citext.
    assert_eq!(
        db.user_by_login(&email.to_uppercase()).await.unwrap().map(|u| u.id),
        Some(user.id)
    );
}

/// REGRESSION.
///
/// The unique index on a citext column folds case regardless of how a
/// parameter is bound, so uniqueness looked correct while lookup was not:
/// `ANA` could not register while `ana` existed, and could not sign in
/// either. Both halves are asserted here because passing only the first is
/// exactly the state this bug shipped in.
#[tokio::test]
async fn a_handle_can_be_used_to_sign_in_in_any_case() {
    let db = skip_without_db!();
    let handle = unique("case");
    let user = db
        .create_user(NewUser {
            handle: handle.clone(),
            display_name: "Case".into(),
            email: None,
        })
        .await
        .unwrap();

    for spelling in [handle.clone(), handle.to_uppercase(), {
        let mut c = handle.chars();
        c.next().map(|f| f.to_uppercase().to_string() + c.as_str()).unwrap()
    }] {
        assert_eq!(
            db.user_by_login(&spelling).await.unwrap().map(|u| u.id),
            Some(user.id),
            "signing in as {spelling:?} must find the same account"
        );
    }
}

/// A tombstoned account must not be able to sign in. The exclusion lives in
/// the query rather than at the call site precisely so no login path can
/// forget it.
#[tokio::test]
async fn a_deleted_account_cannot_be_found_by_login() {
    let db = skip_without_db!();
    let user = a_user(&db, "gone").await;
    let handle = user.handle.clone();
    db.set_password(user.id, "correct horse battery").await.unwrap();

    db.delete_user(user.id).await.unwrap();

    assert!(db.user_by_login(&handle).await.unwrap().is_none());
    // Credentials are gone too, so nothing can authenticate as this account
    // even if some other path found the row.
    assert!(!db.verify_password(user.id, "correct horse battery").await.unwrap());
    // But the row survives, so messages keep an author to point at.
    let row = db.user(user.id).await.unwrap().expect("the tombstone remains");
    assert!(!row.is_active());
}

// ===========================================================================
// Passwords
// ===========================================================================

#[tokio::test]
async fn a_password_verifies_and_a_wrong_one_does_not() {
    let db = skip_without_db!();
    let user = a_user(&db, "pw").await;
    db.set_password(user.id, "a decent passphrase").await.unwrap();

    assert!(db.verify_password(user.id, "a decent passphrase").await.unwrap());
    assert!(!db.verify_password(user.id, "a decent passphrasf").await.unwrap());
}

#[tokio::test]
async fn setting_a_password_twice_replaces_it_rather_than_erroring() {
    let db = skip_without_db!();
    let user = a_user(&db, "pw2").await;
    db.set_password(user.id, "the first passphrase").await.unwrap();
    db.set_password(user.id, "the second passphrase").await.unwrap();

    assert!(!db.verify_password(user.id, "the first passphrase").await.unwrap());
    assert!(db.verify_password(user.id, "the second passphrase").await.unwrap());
}

#[tokio::test]
async fn short_passwords_are_refused_before_they_are_hashed() {
    let db = skip_without_db!();
    let user = a_user(&db, "pw3").await;
    let err = db.set_password(user.id, "short").await.expect_err("too short");
    assert!(matches!(err, DbError::Invalid(_)), "got {err:?}");
}

/// Two hashes of the same password must differ, or the salt isn't doing its
/// job and a stolen table becomes a rainbow-table lookup.
#[tokio::test]
async fn hashing_is_salted() {
    let a = hearth_db::credentials::hash_password("the same passphrase").unwrap();
    let b = hearth_db::credentials::hash_password("the same passphrase").unwrap();
    assert_ne!(a, b);
    assert!(hearth_db::credentials::verify_hash(&a, "the same passphrase"));
    assert!(hearth_db::credentials::verify_hash(&b, "the same passphrase"));
}

// ===========================================================================
// Spaces
// ===========================================================================

#[tokio::test]
async fn creating_a_space_seeds_its_channels_and_makes_the_creator_owner() {
    let db = skip_without_db!();
    let user = a_user(&db, "owner").await;
    let space = db.create_space(&unique("cabin-"), "The Cabin", user.id).await.unwrap();

    assert_eq!(db.role_in_space(space.id, user.id).await.unwrap(), Some(Role::Owner));

    let channels = db.channels_in_space(space.id).await.unwrap();
    // The design's join dialog promises "four channels and two rooms".
    let text = channels.iter().filter(|c| c.kind == ChannelKind::Text).count();
    let rooms = channels.iter().filter(|c| c.kind != ChannelKind::Text).count();
    assert_eq!(text, 4, "the join dialog promises four channels");
    assert_eq!(rooms, 2, "the join dialog promises two rooms");
    // Ordering is by position, so the list renders the same way every time.
    let positions: Vec<i32> = channels.iter().map(|c| c.position).collect();
    let mut sorted = positions.clone();
    sorted.sort();
    assert_eq!(positions, sorted);
}

/// A space with no owner can never regain one — nobody left could promote
/// anybody. So the last owner cannot demote themselves.
#[tokio::test]
async fn the_last_owner_cannot_be_demoted() {
    let db = skip_without_db!();
    let owner = a_user(&db, "solo").await;
    let space = db.create_space(&unique("solo-"), "Solo", owner.id).await.unwrap();

    let err = db
        .set_role(space.id, owner.id, Role::Member)
        .await
        .expect_err("the last owner must not be demotable");
    assert!(matches!(err, DbError::Invalid(_)), "got {err:?}");
    assert_eq!(db.role_in_space(space.id, owner.id).await.unwrap(), Some(Role::Owner));
}

#[tokio::test]
async fn an_owner_can_be_demoted_once_another_owner_exists() {
    let db = skip_without_db!();
    let first = a_user(&db, "own1").await;
    let second = a_user(&db, "own2").await;
    let space = db.create_space(&unique("two-"), "Two", first.id).await.unwrap();

    let invite = db.create_invite(space.id, first.id, 5).await.unwrap();
    db.redeem_invite(&invite.code, second.id).await.unwrap();
    db.set_role(space.id, second.id, Role::Owner).await.unwrap();

    db.set_role(space.id, first.id, Role::Member).await.unwrap();
    assert_eq!(db.role_in_space(space.id, first.id).await.unwrap(), Some(Role::Member));
}

/// Removal is not deletion — the design is explicit that a removed member's
/// messages stay where they are.
#[tokio::test]
async fn removing_a_member_leaves_their_account_intact() {
    let db = skip_without_db!();
    let owner = a_user(&db, "mod").await;
    let member = a_user(&db, "mem").await;
    let space = db.create_space(&unique("rm-"), "Room", owner.id).await.unwrap();
    let invite = db.create_invite(space.id, owner.id, 5).await.unwrap();
    db.redeem_invite(&invite.code, member.id).await.unwrap();

    db.remove_member(space.id, member.id).await.unwrap();

    assert_eq!(db.role_in_space(space.id, member.id).await.unwrap(), None);
    assert!(db.user(member.id).await.unwrap().unwrap().is_active());
}

// ===========================================================================
// Invites — the reason this suite exists
// ===========================================================================

#[tokio::test]
async fn an_invite_preview_names_the_space_without_leaking_its_roster() {
    let db = skip_without_db!();
    let owner = a_user(&db, "inv").await;
    let space = db.create_space(&unique("peek-"), "The Cabin", owner.id).await.unwrap();
    let invite = db.create_invite(space.id, owner.id, 1).await.unwrap();

    let preview = db.peek_invite(&invite.code).await.unwrap().expect("a preview");
    assert_eq!(preview.space_name, "The Cabin");
    assert_eq!(preview.invited_by.as_deref(), Some("Test Person"));

    assert!(db.peek_invite("no-such-code-00").await.unwrap().is_none());
}

#[tokio::test]
async fn redeeming_joins_the_space_as_a_member() {
    let db = skip_without_db!();
    let owner = a_user(&db, "host").await;
    let guest = a_user(&db, "guest").await;
    let space = db.create_space(&unique("join-"), "The Cabin", owner.id).await.unwrap();
    let invite = db.create_invite(space.id, owner.id, 1).await.unwrap();

    assert_eq!(
        db.redeem_invite(&invite.code, guest.id).await.unwrap(),
        Redemption::Joined(space.id)
    );
    assert_eq!(db.role_in_space(space.id, guest.id).await.unwrap(), Some(Role::Member));
}

/// Clicking the link twice is a thing people do. It must not burn a second
/// use, and it must not be an error.
#[tokio::test]
async fn redeeming_twice_yourself_is_idempotent_and_costs_no_uses() {
    let db = skip_without_db!();
    let owner = a_user(&db, "h2").await;
    let guest = a_user(&db, "g2").await;
    let space = db.create_space(&unique("idem-"), "The Cabin", owner.id).await.unwrap();
    let invite = db.create_invite(space.id, owner.id, 1).await.unwrap();

    db.redeem_invite(&invite.code, guest.id).await.unwrap();
    assert_eq!(
        db.redeem_invite(&invite.code, guest.id).await.unwrap(),
        Redemption::AlreadyMember(space.id)
    );
}

#[tokio::test]
async fn a_revoked_invite_says_so_specifically() {
    let db = skip_without_db!();
    let owner = a_user(&db, "h3").await;
    let guest = a_user(&db, "g3").await;
    let space = db.create_space(&unique("rev-"), "The Cabin", owner.id).await.unwrap();
    let invite = db.create_invite(space.id, owner.id, 1).await.unwrap();

    db.revoke_invite(&invite.code).await.unwrap();

    // Specific, not generic: "revoked" and "expired" send someone to
    // different next steps, so the caller has to be able to tell them apart.
    assert_eq!(
        db.redeem_invite(&invite.code, guest.id).await.unwrap(),
        Redemption::Refused(InviteRefusal::Revoked)
    );
    assert_eq!(
        db.redeem_invite("no-such-code-00", guest.id).await.unwrap(),
        Redemption::Refused(InviteRefusal::Unknown)
    );
}

/// **The test this module exists for.**
///
/// "Only work once" has to hold when sixteen people click the same link at the
/// same instant. A check-then-update would let several of them observe
/// `use_count = 0` and all pass; the conditional UPDATE inside the transaction
/// is what makes exactly one win.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_single_use_invite_admits_exactly_one_of_sixteen_racing_redeemers() {
    let db = skip_without_db!();
    let owner = a_user(&db, "race").await;
    let space = db.create_space(&unique("race-"), "The Cabin", owner.id).await.unwrap();
    let invite = db.create_invite(space.id, owner.id, 1).await.unwrap();

    let mut guests = Vec::new();
    for i in 0..16 {
        guests.push(a_user(&db, &format!("r{i}")).await);
    }

    let mut handles = Vec::new();
    for guest in guests {
        let db = db.clone();
        let code = invite.code.clone();
        handles.push(tokio::spawn(async move { db.redeem_invite(&code, guest.id).await }));
    }

    let mut joined = 0;
    let mut exhausted = 0;
    for h in handles {
        match h.await.expect("task did not panic").expect("no database error") {
            Redemption::Joined(_) => joined += 1,
            Redemption::Refused(InviteRefusal::Exhausted) => exhausted += 1,
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    assert_eq!(joined, 1, "exactly one redeemer may win a single-use invite");
    assert_eq!(exhausted, 15);
}

/// The same race on a bounded multi-use invite: the bound is the bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_five_use_invite_admits_exactly_five_of_sixteen() {
    let db = skip_without_db!();
    let owner = a_user(&db, "race5").await;
    let space = db.create_space(&unique("race5-"), "The Cabin", owner.id).await.unwrap();
    let invite = db.create_invite(space.id, owner.id, 5).await.unwrap();

    let mut guests = Vec::new();
    for i in 0..16 {
        guests.push(a_user(&db, &format!("q{i}")).await);
    }

    let mut handles = Vec::new();
    for guest in guests {
        let db = db.clone();
        let code = invite.code.clone();
        handles.push(tokio::spawn(async move { db.redeem_invite(&code, guest.id).await }));
    }

    let joined = futures_count(handles).await;
    assert_eq!(joined, 5, "a five-use invite admits five, no matter the concurrency");
}

async fn futures_count(
    handles: Vec<tokio::task::JoinHandle<Result<Redemption, DbError>>>,
) -> usize {
    let mut joined = 0;
    for h in handles {
        if let Redemption::Joined(_) = h.await.unwrap().unwrap() {
            joined += 1;
        }
    }
    joined
}

// ===========================================================================
// Instance
// ===========================================================================

#[tokio::test]
async fn a_fresh_instance_is_invite_only() {
    let db = skip_without_db!();
    let settings = db.instance_settings().await.unwrap();
    assert_eq!(
        settings.registration_mode,
        hearth_db::RegistrationMode::InviteOnly,
        "the conservative default is the whole point"
    );
}
