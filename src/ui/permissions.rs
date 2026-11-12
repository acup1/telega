use super::*;

fn banned(rights: Option<&tl::enums::ChatBannedRights>) -> bool {
    rights.is_some_and(|tl::enums::ChatBannedRights::Rights(r)| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        (r.until_date == 0 || r.until_date as i64 > now as i64)
            && (r.view_messages || r.send_messages || r.send_plain)
    })
}

fn channel(channel: &tl::types::Channel) -> bool {
    if channel.creator {
        return true;
    }
    if banned(channel.banned_rights.as_ref()) {
        return false;
    }
    if channel.broadcast {
        return channel
            .admin_rights
            .as_ref()
            .is_some_and(|tl::enums::ChatAdminRights::Rights(r)| r.post_messages);
    }
    if channel.left && channel.join_to_send {
        return false;
    }
    channel.admin_rights.is_some() || !banned(channel.default_banned_rights.as_ref())
}

pub(super) fn can_write(peer: &Peer) -> bool {
    match peer {
        Peer::User(user) => matches!(&user.raw, tl::enums::User::User(user) if !user.deleted),
        Peer::Channel(peer) => channel(&peer.raw),
        Peer::Group(peer) => match &peer.raw {
            tl::enums::Chat::Chat(chat) => {
                !chat.left
                    && !chat.deactivated
                    && chat.migrated_to.is_none()
                    && (chat.creator
                        || chat.admin_rights.is_some()
                        || !banned(chat.default_banned_rights.as_ref()))
            }
            tl::enums::Chat::Channel(chat) => channel(chat),
            _ => false,
        },
        Peer::Community(_) => false,
    }
}

impl App {
    pub(super) fn can_compose(&self) -> bool {
        self.open_chat.is_some()
            && self
                .discussion
                .as_ref()
                .map(|v| &v.peer_object)
                .or_else(|| self.current_chat().map(|chat| &chat.peer_object))
                .is_some_and(can_write)
    }
    pub(super) fn require_write(&mut self) -> bool {
        if self.can_compose() {
            true
        } else {
            self.status = "You cannot send messages in this chat".into();
            false
        }
    }
}
