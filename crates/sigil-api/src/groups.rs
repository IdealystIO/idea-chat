//! KeyPackages, groups, commits, Welcomes, reliable messages and relay
//! tickets. Every byte string is base64url; group ids are hex.

use server::server;
use server_kit::Auth;

use crate::{Answer, Principal, RelayTicket};

/// Publish KeyPackages for this device, for others to add it with.
#[server(path = "sigil/publish_key_packages", tags(limit = "30/min"))]
pub async fn publish_key_packages(packages: Vec<String>, me: Auth<Principal>) -> Answer<()> {
    use crate::srv::{device_of, ops};
    ops::publish_key_packages(&device_of(&me.0)?, &packages)
}

/// One of `device`'s KeyPackages, consumed. `None` when it has run out.
#[server(path = "sigil/claim_key_package", tags(limit = "60/min"))]
pub async fn claim_key_package(device: String, me: Auth<Principal>) -> Answer<Option<String>> {
    crate::srv::device_of(&me.0)?;
    crate::srv::ops::claim_key_package(&device)
}

/// Register a group this device just created, from its GroupInfo. Returns
/// the group id.
#[server(path = "sigil/create_group", tags(limit = "10/min"))]
pub async fn create_group(group_info: String, me: Auth<Principal>) -> Answer<String> {
    use crate::srv::{device_of, ops};
    ops::create_group(&device_of(&me.0)?, &group_info)
}

/// Send a commit. On success it is the group's new epoch, which is
/// returned; merge it locally (`Group::confirm`). On `Stale`, another commit
/// won: process it and try again.
#[server(path = "sigil/commit", tags(limit = "120/min"))]
pub async fn commit(group: String, commit: String, welcome: Option<String>, me: Auth<Principal>) -> Answer<u64> {
    use crate::srv::{device_of, ops};
    ops::commit(&device_of(&me.0)?, &group, &commit, welcome.as_deref())
}

/// Welcomes waiting for this device.
#[server(path = "sigil/welcomes")]
pub async fn welcomes(me: Auth<Principal>) -> Answer<Vec<String>> {
    Ok(crate::srv::ops::welcomes(&crate::srv::device_of(&me.0)?))
}

/// Deliver a reliable realtime message to the group's other devices.
#[server(path = "sigil/send", tags(limit = "600/min"))]
pub async fn send(group: String, message: String, me: Auth<Principal>) -> Answer<()> {
    use crate::srv::{device_of, ops};
    ops::send(&device_of(&me.0)?, &group, &message)
}

/// Where and how to join a lossy service's relay room. Fetch again for a
/// fresh grant.
#[server(path = "sigil/relay_ticket", tags(limit = "60/min"))]
pub async fn relay_ticket(group: String, service: u16, me: Auth<Principal>) -> Answer<RelayTicket> {
    use crate::srv::{device_of, ops};
    ops::relay_ticket(&device_of(&me.0)?, &group, service)
}
