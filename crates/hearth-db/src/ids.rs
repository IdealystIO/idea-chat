//! Identifier policy.
//!
//! Every id is a UUIDv7, which sorts by creation time. For messages that
//! matters twice over: the primary key is also the ordering column and the
//! pagination cursor, so a channel page is one indexed range scan with no
//! `ORDER BY created_at` and no tie-breaking.

use time::OffsetDateTime;
use uuid::Uuid;

/// Mint a fresh id.
pub fn new_id() -> Uuid {
    Uuid::now_v7()
}

/// How far a client-minted id's embedded timestamp may sit from server time.
///
/// Clients mint their own message ids so a retry is idempotent and optimistic
/// echo can carry the final id (see `docs/data-model.md` §3). That trust has
/// to be bounded: without a check, a client could place a message at the top
/// of a channel forever, or far enough in the past to be invisible.
///
/// Five minutes is generous for real clock skew and useless as an attack.
pub const MAX_CLIENT_SKEW: time::Duration = time::Duration::minutes(5);

#[derive(Debug, PartialEq, Eq)]
pub enum IdError {
    /// Not a v7 UUID, so it carries no timestamp and cannot be ordered.
    NotV7,
    /// Timestamp too far from ours in either direction.
    Skewed,
}

/// Accept a client-minted id, or say why not.
pub fn check_client_id(id: Uuid, now: OffsetDateTime) -> Result<(), IdError> {
    if id.get_version_num() != 7 {
        return Err(IdError::NotV7);
    }
    let Some(ts) = id.get_timestamp() else { return Err(IdError::NotV7) };
    let (secs, nanos) = ts.to_unix();
    let minted = OffsetDateTime::from_unix_timestamp(secs as i64)
        .map_err(|_| IdError::Skewed)?
        + time::Duration::nanoseconds(nanos as i64);

    let delta = if minted > now { minted - now } else { now - minted };
    if delta > MAX_CLIENT_SKEW {
        return Err(IdError::Skewed);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_ids_sort_by_creation_order() {
        let mut ids: Vec<Uuid> = (0..64).map(|_| new_id()).collect();
        let ordered = ids.clone();
        ids.sort();
        assert_eq!(ids, ordered, "UUIDv7 must sort in mint order, or paging breaks");
    }

    #[test]
    fn a_fresh_client_id_is_accepted() {
        assert_eq!(check_client_id(new_id(), OffsetDateTime::now_utc()), Ok(()));
    }

    /// The attack this guards: pinning a message to the top of a channel by
    /// minting an id far in the future.
    #[test]
    fn a_client_cannot_mint_itself_a_position_in_the_future() {
        let now = OffsetDateTime::now_utc();
        let id = new_id();
        assert_eq!(check_client_id(id, now - time::Duration::hours(1)), Err(IdError::Skewed));
    }

    #[test]
    fn a_client_cannot_backdate_a_message_out_of_view() {
        let now = OffsetDateTime::now_utc();
        let id = new_id();
        assert_eq!(check_client_id(id, now + time::Duration::hours(1)), Err(IdError::Skewed));
    }

    #[test]
    fn a_non_v7_uuid_is_refused_because_it_carries_no_order() {
        let v4 = Uuid::parse_str("9f8e7d6c-5b4a-4392-8180-7f6e5d4c3b2a").unwrap();
        assert_eq!(check_client_id(v4, OffsetDateTime::now_utc()), Err(IdError::NotV7));
    }
}
