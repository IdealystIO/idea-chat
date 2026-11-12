//! Stored records, and the signed actions that remove them.

use server::server;
use server_kit::Auth;

use crate::{Answer, Principal, Stored};

/// Store a sealed record. Returns its server sequence.
#[server(path = "sigil/post_record", tags(limit = "600/min"))]
pub async fn post_record(group: String, record: String, me: Auth<Principal>) -> Answer<u64> {
    use crate::srv::{device_of, ops};
    ops::post_record(&device_of(&me.0)?, &group, &record)
}

/// Records of one service after server sequence `after`, oldest first.
#[server(path = "sigil/records", tags(limit = "600/min"))]
pub async fn records(group: String, service: u16, after: u64, limit: u32, me: Auth<Principal>) -> Answer<Vec<Stored>> {
    use crate::srv::{device_of, ops};
    ops::records(&device_of(&me.0)?, &group, service, after, limit)
}

/// Act on a signed action (`record.delete`, `records.prune`). The action's
/// own signature says who asks; the session only gets it here.
#[server(path = "sigil/act", tags(limit = "60/min"))]
pub async fn act(action: String, me: Auth<Principal>) -> Answer<()> {
    crate::srv::device_of(&me.0)?;
    crate::srv::ops::act(&action)
}
