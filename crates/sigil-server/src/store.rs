//! What the server keeps, behind a trait so a database can stand in for
//! memory later. Everything stored is ciphertext or public: KeyPackages,
//! sealed records, Welcomes, and signed actions kept as tombstones.

use std::collections::{BTreeMap, HashMap, VecDeque};

use sigil::{GroupId, ServiceId};

use crate::DeviceId;

pub trait Store {
    fn put_key_package(&mut self, device: &DeviceId, key_package: Vec<u8>);
    /// Hand out one of `device`'s KeyPackages, removing it.
    fn take_key_package(&mut self, device: &DeviceId) -> Option<Vec<u8>>;
    fn key_packages_left(&self, device: &DeviceId) -> usize;

    /// Store a record; returns its server sequence, from 1, per (group,
    /// service).
    fn append(&mut self, group: GroupId, service: ServiceId, record: Vec<u8>) -> u64;
    /// Records after server sequence `after`, oldest first, at most `limit`.
    fn records(&self, group: GroupId, service: ServiceId, after: u64, limit: usize) -> Vec<(u64, Vec<u8>)>;
    fn record(&self, group: GroupId, service: ServiceId, seq: u64) -> Option<Vec<u8>>;
    /// Drop the records `drop` picks among those up to and including `through`.
    fn remove(&mut self, group: GroupId, service: ServiceId, through: u64, drop: &dyn Fn(u64, &[u8]) -> bool) -> usize;
    /// Keep a signed action that changed what's stored, so members can see
    /// who asked.
    fn tombstone(&mut self, group: GroupId, action: Vec<u8>);
    fn tombstones(&self, group: GroupId) -> Vec<Vec<u8>>;

    fn put_welcome(&mut self, device: &DeviceId, welcome: Vec<u8>);
    fn take_welcomes(&mut self, device: &DeviceId) -> Vec<Vec<u8>>;
}

/// One service's records: the last server sequence handed out, and the
/// records still kept.
type Log = (u64, BTreeMap<u64, Vec<u8>>);

#[derive(Default)]
pub struct MemoryStore {
    key_packages: HashMap<DeviceId, VecDeque<Vec<u8>>>,
    records: HashMap<(GroupId, ServiceId), Log>,
    tombstones: HashMap<GroupId, Vec<Vec<u8>>>,
    welcomes: HashMap<DeviceId, Vec<Vec<u8>>>,
}

impl Store for MemoryStore {
    fn put_key_package(&mut self, device: &DeviceId, key_package: Vec<u8>) {
        self.key_packages.entry(device.clone()).or_default().push_back(key_package);
    }

    fn take_key_package(&mut self, device: &DeviceId) -> Option<Vec<u8>> {
        self.key_packages.get_mut(device)?.pop_front()
    }

    fn key_packages_left(&self, device: &DeviceId) -> usize {
        self.key_packages.get(device).map_or(0, VecDeque::len)
    }

    fn append(&mut self, group: GroupId, service: ServiceId, record: Vec<u8>) -> u64 {
        let (last, map) = self.records.entry((group, service)).or_default();
        *last += 1;
        map.insert(*last, record);
        *last
    }

    fn records(&self, group: GroupId, service: ServiceId, after: u64, limit: usize) -> Vec<(u64, Vec<u8>)> {
        let Some((_, map)) = self.records.get(&(group, service)) else { return Vec::new() };
        map.range(after + 1..).take(limit).map(|(s, r)| (*s, r.clone())).collect()
    }

    fn record(&self, group: GroupId, service: ServiceId, seq: u64) -> Option<Vec<u8>> {
        self.records.get(&(group, service))?.1.get(&seq).cloned()
    }

    fn remove(&mut self, group: GroupId, service: ServiceId, through: u64, drop: &dyn Fn(u64, &[u8]) -> bool) -> usize {
        let Some((_, map)) = self.records.get_mut(&(group, service)) else { return 0 };
        let before = map.len();
        map.retain(|s, r| *s > through || !drop(*s, r));
        before - map.len()
    }

    fn tombstone(&mut self, group: GroupId, action: Vec<u8>) {
        self.tombstones.entry(group).or_default().push(action);
    }

    fn tombstones(&self, group: GroupId) -> Vec<Vec<u8>> {
        self.tombstones.get(&group).cloned().unwrap_or_default()
    }

    fn put_welcome(&mut self, device: &DeviceId, welcome: Vec<u8>) {
        self.welcomes.entry(device.clone()).or_default().push(welcome);
    }

    fn take_welcomes(&mut self, device: &DeviceId) -> Vec<Vec<u8>> {
        self.welcomes.remove(device).unwrap_or_default()
    }
}
