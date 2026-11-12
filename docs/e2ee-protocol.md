# Sigil: the group E2EE protocol — design

Status: **proposal; the protocol core is in `crates/sigil`** (groups, services,
keys, records, blobs, checkpoints, policy, actions). Nothing uses it yet.
Drafted 2026-11-08. This is the protocol layer only: how groups of devices share keys, how a service is
defined on top of a group, and what each kind of service puts on the wire.

Scope: groups and epochs, services, envelope formats, the server's contract,
late joiners, blobs, admin policy, third-party apps, and signed actions
(including push through the identity service). Out of scope: the identity
service itself (it supplies credentials, §4.2, and gets its own document),
what Idea Chat builds on top (which services, which history policy), and
hiding metadata beyond what §8 lists. Open questions are in §13.

Decided so far: no unsealed mode (every group this protocol makes is sealed);
`openmls` with its RustCrypto provider; a 10 s previous-epoch grace for now;
history through per-epoch state keys and links (§5.3); app state trusted
through co-signed checkpoints (§7.3).

---

## 1. The model in one page

A **group** is an MLS group: a set of devices that share a key schedule and
move together through **epochs**. Every change to the group — someone joins,
leaves, rotates a key — starts a new epoch with new keys.

A **service** is a named use of a group: `messages`, `presence`, `voice`,
`app/whiteboard/ops`. The protocol does not know what a service means. It only
knows which of three kinds it is:

| Kind | Delivery | Server stores | Sender authenticated as | Carried by | Examples |
| --- | --- | --- | --- | --- | --- |
| **Stateful** | reliable, ordered | ciphertext records | the individual member (signature) | API + live socket | chat messages, edits, reactions, app state |
| **Realtime, reliable** | reliable, ordered | nothing | the individual member (MLS) | live socket | typing, presence, call signalling |
| **Realtime, lossy** | best effort | nothing | someone in the group | relay datagrams | voice, video, screen |

Large payloads (attachments, images) are **blobs**: encrypted under their own
key, stored in object storage, and referenced from a stateful record that
carries the key (§7).

```text
Implementation   Idea Chat: messages · presence · voice · video · apps
                     │ define services, set policy
Protocol         services ─ stateful records │ reliable msgs │ SFrame │ blobs
                     │ per-service keys from the exporter
                 groups ─── MLS (RFC 9420): members, epochs, policy
                     │
Transport        API + live socket · call-relay (QUIC) · S3   — all opaque
```

## 2. Constraints that drive the design

1. **The server never holds a key.** Not for content, not for the room key,
   not as an escrow. It stores and routes ciphertext.
2. **The server cannot add anyone.** Membership changes are MLS commits, and
   members reject a commit its signer was not allowed to make (§9). The
   server can still refuse service; it cannot admit.
3. **No silent fallback.** A group is sealed from creation. There is no
   "no key" state that sends clear data; a missing key is an error that stops
   sending. (Today's `Option<RoomKey>` is exactly the downgrade this rules
   out.)
4. **Removal rotates keys.** Someone who leaves cannot read what comes
   after. Realtime keys change with every epoch (MLS does this); stored data
   gets a new state key with every commit (§5.3), whatever the service's
   policy says.
5. **Services are isolated.** One service's keys cannot be derived from
   another's, so a buggy or hostile app sees only its own traffic.

## 3. Terms

| Term | Meaning |
| --- | --- |
| Device | One installation; one MLS leaf. A person with three devices is three leaves. |
| Member | A device currently in the group's ratchet tree. |
| Leaf index | A device's position in the group's ratchet tree: a number assigned when the device is added, the same for every member, and fixed until that device is removed. A removed device's number can go to a later one, so a leaf index names a device only together with an epoch. Each member maps it to the credential in that position. |
| Epoch | One state of the group's key schedule, numbered from 0. |
| Service | A name plus a kind, registered in the group (§5). |
| Service id | The 16-bit number a service has in this group, used on the wire. |
| Host | The client process that holds MLS state and does all sealing. |
| App | Code that uses services through the host and never sees keys (§10). |

## 4. Groups

### 4.1 MLS profile

- RFC 9420, ciphersuite `0x0001`
  (`MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519`).
- One group per room or conversation. A group id is 16 random bytes, chosen by
  the creator.
- Handshake messages (proposals, commits) are sent as `PublicMessage`, so the
  server can see the roster it is routing to (§8). Application messages are
  `PrivateMessage`.
- The server acts as the MLS delivery service: it stores each device's
  KeyPackages, orders commits, and delivers Welcomes (§8).

### 4.2 Credentials

A leaf's credential is a certificate from the identity service binding
`user@identity-domain` to the leaf's signature key, signed by the user's
identity key. Members verify it when a leaf is added and refuse an unverifiable
one. How certificates are issued, rotated and logged is the identity spec's
business; this protocol only needs "verify(credential) → user id or error".

### 4.3 Epochs and commits

- Exactly one commit is accepted per epoch. The server takes the first that
  arrives and rejects the rest; their senders refetch and retry.
- After a commit, members keep the previous epoch's keys for a short grace
  (proposed: 10 s) so realtime traffic in flight still opens, then delete them.
- Members compare MLS's `epoch_authenticator` out of band when they want
  assurance that they are in the same group as each other (a split group
  shows as differing authenticators).

## 5. Services

### 5.1 Defining one

```text
Service {
  name:    "messages" | "voice" | "app/<app id>/<name>"   // ≤ 64 bytes, ASCII
  kind:    Stateful | RealtimeReliable | RealtimeLossy
  version: u16
}
```

A group's services live in a GroupContext extension, `sigil_services` (type `0xF5A1`), mapping
service id → service. Adding or removing a service is a commit, so every
member agrees on the table and a service id means the same thing to all of
them.

### 5.2 Keys

Every per-service secret comes from the MLS exporter, once per epoch:

```text
service_secret(svc, epoch) = MLS-Exporter("sigil/svc/v1", svc.name ‖ svc.version, 32)
```

and from that, with HKDF-Expand:

| Kind | Derived | Used for |
| --- | --- | --- |
| Stateful | nothing from the epoch — see §5.3 | |
| Realtime, reliable | nothing — uses MLS `PrivateMessage` directly | §6.2 |
| Realtime, lossy | `sframe_base = Expand(secret, "sframe", 16)` | per-sender SFrame keys (§6.3) |

The exporter output is unique per name, version and epoch, so services
cannot reach each other's keys and old epochs cannot reach new ones.

### 5.3 State keys and links

Stored data must stay readable after the epoch that wrote it, so stateful
services don't use epoch secrets directly. Each epoch gives each stateful
service a **state key**, computed from the commit that started the epoch, so a
commit can't exist without the key that goes with it:

```text
state_key[e]    = MLS-Exporter("sigil/state/v1", name ‖ version, 32)   // in epoch e
record_key[e]   = HKDF-Expand(state_key[e], "record", 16)
record_nonce[e] = HKDF-Expand(state_key[e], "nonce", 12)
```

- **Every commit rotates.** Additions, removals, policy changes and
  key-update commits alike (§2.4). `rotate_every` (§9) asks members to send a
  key-update commit on a schedule.
- **Nothing to send, nothing to wait for.** Every member of the new epoch,
  newcomers included, computes `state_key[e]` from its own MLS state. A
  removed member can't, because they aren't in the epoch. Nobody waits on the
  committer, and the server can't strip a key out of a commit.
- **Members keep state keys, not epoch secrets.** MLS deletes each epoch's
  secrets as usual; a device keeps only the exported state keys of the epochs
  it can read.
- **Links carry history.** A `link` record (§6.1), written and sealed in epoch
  `e`, holds `state_key[e-1]` and **the roster of epoch `e-1`**: each leaf's
  credential and signature key. A holder of `state_key[e]` can therefore read
  and verify epoch `e-1`'s records, and follow that epoch's link further back.
  Holding an old key unlocks nothing newer. The committer posts the link as
  its first record in the new epoch. If it doesn't, any member holding both
  keys may; readers keep the first one that verifies.
- **Which links are posted.** After every commit, except one that adds a leaf
  to a service whose policy is `history: None` (§7.2).

## 6. Envelopes

### 6.1 Stateful records

```text
header (40 bytes, clear, authenticated):
  u8   version      = 1
  u8   kind         // data, link (§5.3), checkpoint or cosign (§7.3)
  [16] group_id
  u64  epoch        // the epoch it was written in: names its key and its signer's leaf
  u16  service_id
  u32  sender       // leaf index
  u64  seq          // per (sender, service), never reset
body:
  AES-128-GCM(record_key[epoch], nonce = record_nonce[epoch] ⊕ (sender ‖ seq),
              aad = header, plaintext)
signature (64 bytes):
  Ed25519(leaf signature key, "sigil/record/v1" ‖ header ‖ body)
```

- The signature makes the record attributable to one member, which a shared
  state key cannot.
- `seq` is per sender and never resets, so a reader can spot gaps: the server
  can withhold records, but not undetectably.
- The server gives each accepted record a **server sequence** per (group,
  service) for paging. It is for convenience only; nothing trusts it.
- Records are append-only. Edits and deletes are `data` records whose
  meaning the service defines; the other kinds are the protocol's own.

### 6.2 Realtime, reliable

An MLS `PrivateMessage` whose `authenticated_data` begins with the 2-byte
service id. MLS already provides confidentiality, per-sender authentication
and replay protection; the server fans it out to members currently online
and keeps nothing.

### 6.3 Realtime, lossy

SFrame (RFC 9605), suite `AES_128_CTR_HMAC_SHA256_80`, keyed as RFC 9605's
MLS key management describes, with `sframe_base` (§5.2) in place of the fixed
exporter label:

```text
KID = epoch (low 8 bits) ‖ sender leaf index ‖ context id
sender_key = HKDF(sframe_base, KID)
```

- The KID names the sender's **leaf**, not a random number, so a receiver knows
  which member a key belongs to without trusting the relay's numbering.
- Any member holding `sframe_base` could still forge another member's frames;
  that is the cost of datagram speed, and why reliable traffic is signed.
- The transport header the relay routes on (call-proto's kind and track,
  plus the service id) stays in the clear and is authenticated as SFrame
  metadata.

## 7. Blobs and late joiners

### 7.1 Blobs

1. The sender picks a random 256-bit content key.
2. The payload is padded with zeros to a multiple of 1 KiB, then encrypted in
   64 KiB chunks (AES-256-GCM; chunk index and a
   final-chunk flag in the nonce and AAD, so chunks cannot be reordered or
   truncated).
3. The ciphertext goes to object storage under a random id.
4. A stateful record carries `{blob id, content key, size, SHA-256 of the
   plaintext, content type}`.

The server learns a blob's padded size and nothing else. Who can read a blob
is exactly who can read the record that points to it.

### 7.2 Late joiners and new devices

Each stateful service's policy sets what a newcomer can read:

| `history` | Link across an addition | Newcomer reads |
| --- | --- | --- |
| `Full` | posted | everything still stored: they derive the current key and follow the links back |
| `None` | not posted | from the epoch their addition started |

A newcomer can write as soon as they have processed the Welcome, under
either policy. Links carry each earlier epoch's roster along with its key, so
following them back is enough to verify old records' signatures as well as
read them. With a trusted checkpoint (§7.3), only the epochs after it matter.

`None` is a policy members follow, not a barrier between members: any member
holding both keys could post the missing link, as they could forward old
plaintext. It does keep a newcomer from reading history unless a member
chooses to hand it over.

What rotation does and doesn't protect: a removed member can't read
anything written after they left. They may still hold old state keys, so
ciphertext from while they were a member stays readable to them, as it was
then. The server and outsiders read nothing at any point.

A new device of an existing member joins as a new leaf, and one of the
member's other devices sends it every state key that member holds, linked or
not, so a person's devices all read the same history. If every device is
lost, history is unreadable unless the person kept an encrypted key backup,
which is the identity service's concern.

### 7.3 Checkpoints: trusting state without replaying it

A long-lived stateful service builds up far more records than its state is
worth. A **checkpoint** lets a reader trust the state as of one record by
checking signatures on it, without applying any record before it.

**State as chunks.** The service turns its state into chunks of at most
64 KiB, each under a path it chooses. The protocol fixes how they're hashed:

```text
leaf  = SHA-256(0x00 ‖ u32 len(path) ‖ path ‖ chunk)
node  = SHA-256(0x01 ‖ left ‖ right)       // leaves sorted by path; odd one carried up
root  = the tree's top node                // SHA-256(0x02) for empty state
```

Chunks are stored as blobs (§7.1). A **manifest** blob lists each chunk's
path, leaf hash, blob id and content key.

**Order.** The state after server sequence `n` is the state from the
previous trusted checkpoint (or empty), with every `data` record after it up
to `n` applied in server-sequence order. The server chooses that order, but
it is the same order for every member, so honest members compute the same
root. Members who can't agree have been shown different orders, which is a
fork, or one of them has a bug; either way it surfaces.

**Records.**

```text
checkpoint body: { after: n, root, manifest: blob ref }     // kind checkpoint
cosign body:     { checkpoint: (server seq, SHA-256 of the record), root }   // kind cosign
```

- A member posts a checkpoint when the policy's `checkpoint_every` is due,
  if its role is in `may_checkpoint`. If several post for the same `n`, the
  first the server accepted counts.
- Every other member holding the state at `n` computes its own root. If it
  matches, it posts a cosign. If it differs, it posts a cosign carrying its
  own root: a **dispute**.
- A checkpoint is **trusted** when matching signatures (the poster's plus
  cosigns) come from enough distinct people to meet the policy's
  `trust_checkpoint`. In a group with fewer people than the threshold, it
  needs all of them.
- A dispute doesn't decide anything. People see a warning naming who signed
  each root, and newcomers start from the previous undisputed trusted
  checkpoint until the next one replaces it.

**A reader starting from a checkpoint:**

1. Takes the newest trusted checkpoint and verifies its signatures.
2. Fetches the manifest and checks that its leaves produce `root`.
3. Fetches the chunks it needs, now or lazily, and checks each against its
   leaf.
4. Applies the `data` records after `n`.

Its keys come from following links back only as far as the checkpoint's
epoch.

**Pruning.** Once a checkpoint is trusted, the `data` records it covers can
be deleted with a `records.prune` action (§11.1).

**What it guarantees.** The server can't forge a checkpoint, a cosign or a
chunk. One dishonest member can't get a false state trusted, and their
signed checkpoint is evidence of the attempt. People who together meet the
threshold can get any state trusted; that is why the threshold is policy.
Whether a state makes sense for the app is the app's to judge. The protocol
makes sure everyone sees the same state and knows who vouched for it.

**What services must do.** The same state must always produce the same
chunks, byte for byte, or honest members won't agree on a root. That is the
hardest part for a service author, so the crate ships a test harness that
replays records in different processes and compares roots.

## 8. The server's contract

| The server must | The server can | The server cannot |
| --- | --- | --- |
| store KeyPackages and hand each out once | see the roster, who sends to which group and service, when, and how much | read any record, message, frame or blob |
| accept one commit per epoch, in order | drop, delay or withhold traffic (detectable via `seq` for records) | add a member or forge a commit (§9) |
| store stateful records and blobs; page records by server sequence | refuse service to anyone | forge a record or message from a member |
| fan out reliable messages to online members | split the group by delivering different commits (visible as members failing to decrypt each other, and in `epoch_authenticator`) | derive any key, past or present |
| relay lossy datagrams | rate-limit by size and count | read who sent a lossy frame beyond its leaf index |

## 9. Admin policy

Group policy is a second GroupContext extension, `sigil_policy` (type `0xF5A2`):

```text
Policy {
  roles:    map<user id, Role>                 // owner, admin, member, …
  may_add:      set<Role>
  may_remove:   set<Role>
  may_change_policy: set<Role>
  services: map<service id, {
    may_send:         set<Role>
    // stateful only:
    history:          Full | None                   // §7.2
    rotate_every:     duration | none               // key-update commits on a schedule
    checkpoint_every: records: u32 | none           // §7.3
    may_checkpoint:   set<Role>
    trust_checkpoint: { people: u8, or_any_of: set<Role> }   // e.g. 2 people, or 1 admin
  }>
}
```

Policy can ask for more rotations; it can't prevent the one every commit
brings (§5.3).

- Every member validates every commit against the policy of the epoch it
  applies to. A commit adding or removing a leaf, or changing the policy, from
  a signer whose role is not allowed is rejected, so the epoch never forms.
- Policy changes are themselves commits, so the policy's history is the
  group's history: tamper-evident and agreed by every member.
- `may_send` is enforced in two places: members drop records and messages from
  a role not allowed to send, and the server and relay are told, so they can
  refuse them before fan-out.
- Server-side bans still work and still matter: the server can stop serving a
  person. It cannot let anyone in.

## 10. Third-party apps

- The host holds all MLS state and keys. An app never receives a key, a
  secret or the exporter.
- An app declares a manifest: its id and its services (name, kind). Its
  service names are namespaced `app/<app id>/…`, and the host only seals and
  opens those services on its behalf.
- An app runs in a sandbox (a WASM module or an isolated iframe) and talks to
  the host through `send(service, bytes)` and `on_receive(service, bytes,
  sender)`.
- The worst a hostile app can do is send garbage on its own services, which
  `may_send` can shut off.

## 11. Actions

A server can't read content, so it can't decide on its own whom to act for.
An **action** is a signed request from one device asking one server, or the
identity service, to do one specific thing. Nothing is done in a member's
name without that member's key.

```text
Action {
  version:    u8
  kind:       string        // "record.delete", "push.notify", …
  issuer:     credential    // the device's leaf credential (§4.2)
  audience:   string        // the one server or identity service that may act on it
  group_id:   [16] | none   // set when the action concerns a group
  epoch:      u64  | none
  body:       bytes         // defined by the kind
  nonce:      [16]
  issued_at:  u64
  expires_at: u64           // minutes after issued_at, not days
}
signature = Ed25519(device signature key, "sigil/action/v1" ‖ Action)
```

A receiver acts only when all of these hold:

- the signature verifies against `issuer`, and the credential verifies (§4.2);
- `audience` is the receiver itself, so an action meant for one server can't
  be replayed at another;
- it hasn't expired, and its nonce hasn't been seen before (receivers keep
  seen nonces until expiry);
- for a group action, the issuer is a member at `epoch` and the group's
  policy (§9) allows the kind. Servers can check this because commits are
  `PublicMessage` (§4.1), so the server follows the same policy members do.

Actions are a registry of kinds, like services. Three are defined here.

### 11.1 Server actions

`record.delete` asks the server to drop a stored record's ciphertext (and any
blob it points to). The body names the record by group, service and server
sequence. The server accepts it from the record's sender, or from a role the
policy allows to moderate, and keeps the signed action as a tombstone, so
members can see who asked for a deletion. The server could always drop data
silently; the action is what lets everyone tell a legitimate deletion from
one the server made up.

`records.prune` asks the server to drop the `data` records up to a server
sequence that a trusted checkpoint covers (§7.3). Checkpoints and cosigns are
sealed, so the server can't judge trust itself; it takes a prune from someone
the policy's `trust_checkpoint.or_any_of` already trusts on their own, or from
a role that may remove members. Members can audit every prune: the signed
action is kept as a tombstone.

### 11.2 Push notifications through the identity service

The identity service is the one place that knows a person's devices across
servers, so it holds their push tokens. A server must not be able to make it
push anything on its own. A push therefore needs two signatures that the
server cannot produce: the **recipient's** consent and the **sender's**
request.

```text
Bob's device ── push grant (signed by Bob) ────────────▶ server S   (once, renewable)
Alice's device ─ record + push.notify (signed by Alice) ▶ server S
server S ─────── push.notify + Bob's grant ────────────▶ identity service
identity service checks both, then ─── wake-up ────────▶ Bob's devices
Bob's device ─── fetch and decrypt from S ─────────────▶ server S
```

1. **Grant.** When Bob's device joins a group on server S, it signs a push
   grant: audience the identity service; body "server S may request pushes
   to me for group G, until T". It gives the grant to S and renews it before
   T.
2. **Request.** When Alice's device sends a record that should notify people
   (a DM, a mention, `@everyone`), it also signs a `push.notify` action:
   audience the identity service; body `{server S, group G, hash of the
   record, recipients}`. The sender's client decides recipients, because the
   server can't read mentions.
3. **Forward.** S sends the action, with each recipient's grant, to the
   identity service, authenticating as S.
4. **Check.** The identity service verifies Alice's action as above, checks
   each recipient's grant names S and G and is current, and sends each
   recipient a push.
5. **Content.** The push carries no content: only "something new from server
   S in group G". Bob's device fetches the record from S and decrypts it. A
   preview, if wanted, is sealed by Alice to Bob's device key inside the
   action body, so only Bob's device can read it.

What this prevents: S can't push without a fresh action from a real member,
can't push anyone who hasn't granted it, can't push for a group the grant
doesn't name, and can't replay an action. What it doesn't: S can drop pushes,
and the identity service learns that Alice notified Bob through S at that
time (§13).

## 12. What carries over from today

| Today | Becomes |
| --- | --- |
| `call-e2ee` (SFrame, RFC vectors, sealer/opener, replay window) | §6.3 nearly as is: keys from `sframe_base`, KID from the leaf index instead of a random u32, epochs actually used |
| `call-proto` header and kind/track routing | the clear transport header, plus a service id |
| `call-relay` and `call-auth` grants | lossy transport; grants gain the service id so `may_send` reaches the relay |
| `chat-keys` (X25519 seal, fingerprint) | replaced by MLS and identity-service credentials |
| `media.rs` key request/delivery | replaced by Welcome and commits |
| `messages.body` in Postgres, search index | replaced by stateful records; search moves to the client |
| presigned S3 uploads | blobs (§7.1), same transport |

What this closes from the current code's known gaps: the server naming the
key a room key is sealed to (credentials are verified by members), the
empty-key downgrade (§2.3), no rekey on leave or kick (§2.4), and relay
sender renumbering (§6.3 KIDs name leaves).

## 13. Open questions

Settled: no unsealed mode for now; `openmls` (with `openmls_rust_crypto`, so
no AWS dependency); 10 s grace for now, to be measured against relay
reconnects; checkpoint frequency is policy (`checkpoint_every`), and a
disputed checkpoint shows a warning (§7.3).

1. **Large groups.** Every commit adds a link, so a busy room builds long link
   chains, and each link carries a roster. Checkpoints bound how far a newcomer
   walks back, but batching membership changes into fewer commits may still
   be wanted.
2. **Metadata.** Records carry the sender's leaf index in the clear. Hiding
   it (MLS-style sender data encryption) costs complexity for something the
   server mostly learns from the session anyway.
3. **Push metadata.** The identity service sees who notified whom, through
   which server, and when. Recipients could be named by grant id instead of
   user id, but the sender's signature still names the sender.
4. **Server identity.** How a server authenticates to the identity service
   when forwarding actions (step 3 of §11.2): mutual TLS, or a key published
   at the server's domain.
5. **Web key storage.** On the web, device keys sit in page storage today.
