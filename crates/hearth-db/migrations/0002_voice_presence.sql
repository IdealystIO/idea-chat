-- Where people are, in voice.
--
-- Owned by the chat server rather than derived from the relay fleet. Two
-- reasons, and the second is the one that matters: a relay is a thing that
-- forwards bytes, and asking it who is present makes it part of the app's data
-- model; and presence read from the fleet disappears the moment the fleet is
-- unreachable, so a sidebar would go blank for a reason that has nothing to do
-- with the people in it.
--
-- The trade this makes is that presence is asserted by clients rather than
-- observed from sockets, which is how ghosts happen — a tab closed badly
-- leaves someone standing in a room forever. `seen_at` is the answer: a row is
-- only presence while it is fresh, so the worst case is a stale entry for one
-- heartbeat interval rather than until someone notices.
CREATE TABLE voice_presence (
    -- One row per person, not per connection: you are in one call at a time,
    -- and making that the primary key means joining a second room cannot leave
    -- you standing in the first. The alternative — a row per (room, user) —
    -- allows a state the product does not have.
    user_id     uuid PRIMARY KEY REFERENCES users (id) ON DELETE CASCADE,
    channel_id  uuid NOT NULL REFERENCES channels (id) ON DELETE CASCADE,
    -- When they joined THIS room, so a sidebar can order by arrival. Preserved
    -- across heartbeats; reset when the room changes.
    joined_at   timestamptz NOT NULL DEFAULT now(),
    -- Last heartbeat. Everything reading this table filters on it.
    seen_at     timestamptz NOT NULL DEFAULT now()
);

-- The read path is "who is in the rooms of this space", which reaches this
-- table by channel. Partial on freshness would need a constant `now()`, so the
-- index is plain and the filter happens on the rows it returns.
CREATE INDEX voice_presence_channel ON voice_presence (channel_id, joined_at);
