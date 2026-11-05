//! Who may do what. One copy of the rules, run by the server to decide and by
//! the client to decide what to offer — so a button the server would refuse
//! is never drawn.

use crate::{Channel, Id, Role, Space};

/// What someone may do in a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Access {
    /// Not theirs to know about: a conversation they are not in, or a space
    /// they are not a member of.
    None,
    /// They can see it exists, but not read it.
    Locked,
    /// They can read but not post.
    ReadOnly,
    /// Read and post.
    Full,
}

impl Access {
    pub fn can_read(self) -> bool {
        self >= Access::ReadOnly
    }

    pub fn can_post(self) -> bool {
        self == Access::Full
    }
}

/// What `user` may do in `channel`, given the space it is in (if any).
///
/// A thread inherits its parent channel's rules — pass the parent's — except
/// that an archived thread is read-only to everyone.
pub fn channel_access(user: Id, channel: &Channel, space: Option<&Space>) -> Access {
    if channel.kind.is_private() {
        return if channel.members.contains(&user) { Access::Full } else { Access::None };
    }
    let Some(role) = space.and_then(|s| s.role_of(user)) else {
        return Access::None;
    };
    if role < channel.view {
        Access::Locked
    } else if role < channel.post || channel.archived {
        Access::ReadOnly
    } else {
        Access::Full
    }
}

/// Changing a space's name, channels, categories and invites.
pub fn can_manage_space(role: Role) -> bool {
    role >= Role::Admin
}

/// Removing someone from a space, or changing their role. Only downward:
/// an admin can remove a member but not another admin, and nobody can touch
/// the owner.
pub fn can_moderate(actor: Role, target: Role) -> bool {
    actor >= Role::Admin && actor > target
}

/// Giving someone a role. The owner can make admins; nobody can make an
/// owner (ownership is transferred, not granted).
pub fn can_grant(actor: Role, target_now: Role, to: Role) -> bool {
    to != Role::Owner && can_moderate(actor, target_now) && actor > to
}

/// Deleting a message: its author always, a space's admins in their space.
pub fn can_delete_message(user: Id, author: Id, role: Option<Role>) -> bool {
    user == author || role.is_some_and(|r| r >= Role::Admin)
}

/// Editing a message: only its author.
pub fn can_edit_message(user: Id, author: Id) -> bool {
    user == author
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChannelKind, Member};
    use std::collections::BTreeMap;

    fn id(n: u128) -> Id {
        Id::from_u128(n)
    }

    fn space(members: &[(u128, Role)]) -> Space {
        Space {
            id: id(100),
            name: "s".into(),
            icon: None,
            owner: id(1),
            members: members.iter().map(|&(u, r)| (id(u), Member { role: r, joined_at: 0 })).collect::<BTreeMap<_, _>>(),
            categories: vec![],
            created_at: 0,
        }
    }

    fn channel(kind: ChannelKind, view: Role, post: Role) -> Channel {
        Channel {
            id: id(200),
            kind,
            space: Some(id(100)),
            name: "c".into(),
            topic: String::new(),
            category: None,
            position: 0,
            parent: None,
            starter: None,
            view,
            post,
            members: vec![],
            archived: false,
            last_message: None,
            message_count: 0,
            created_by: id(1),
            created_at: 0,
        }
    }

    #[test]
    fn a_member_below_the_view_role_sees_the_channel_locked() {
        let s = space(&[(1, Role::Owner), (2, Role::Member)]);
        let c = channel(ChannelKind::Text, Role::Admin, Role::Admin);
        assert_eq!(channel_access(id(2), &c, Some(&s)), Access::Locked);
        assert_eq!(channel_access(id(1), &c, Some(&s)), Access::Full);
    }

    #[test]
    fn a_member_below_the_post_role_reads_only() {
        let s = space(&[(2, Role::Member), (3, Role::Admin)]);
        let c = channel(ChannelKind::Text, Role::Member, Role::Admin);
        assert_eq!(channel_access(id(2), &c, Some(&s)), Access::ReadOnly);
        assert_eq!(channel_access(id(3), &c, Some(&s)), Access::Full);
    }

    #[test]
    fn a_stranger_has_no_access_to_a_space_channel() {
        let s = space(&[(2, Role::Member)]);
        let c = channel(ChannelKind::Text, Role::Member, Role::Member);
        assert_eq!(channel_access(id(9), &c, Some(&s)), Access::None);
    }

    #[test]
    fn an_archived_thread_is_read_only_even_to_its_owner() {
        let s = space(&[(1, Role::Owner)]);
        let mut c = channel(ChannelKind::Thread, Role::Member, Role::Member);
        c.archived = true;
        assert_eq!(channel_access(id(1), &c, Some(&s)), Access::ReadOnly);
    }

    #[test]
    fn a_direct_conversation_is_open_only_to_its_members() {
        let mut c = channel(ChannelKind::Direct, Role::Member, Role::Member);
        c.space = None;
        c.members = vec![id(1), id(2)];
        assert_eq!(channel_access(id(1), &c, None), Access::Full);
        assert_eq!(channel_access(id(3), &c, None), Access::None);
    }

    #[test]
    fn moderation_only_reaches_downward() {
        assert!(can_moderate(Role::Owner, Role::Admin));
        assert!(can_moderate(Role::Admin, Role::Member));
        assert!(!can_moderate(Role::Admin, Role::Admin));
        assert!(!can_moderate(Role::Member, Role::Member));
        assert!(!can_moderate(Role::Admin, Role::Owner));
    }

    #[test]
    fn only_the_owner_makes_admins_and_nobody_makes_owners() {
        assert!(can_grant(Role::Owner, Role::Member, Role::Admin));
        assert!(!can_grant(Role::Admin, Role::Member, Role::Admin));
        assert!(!can_grant(Role::Owner, Role::Member, Role::Owner));
        assert!(can_grant(Role::Owner, Role::Admin, Role::Member));
    }
}
