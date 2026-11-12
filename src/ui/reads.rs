use super::*;
use grammers_session::types::PeerKind;

#[derive(Default)]
pub(super) struct State {
    peers: HashMap<PeerId, Progress>,
}

struct Progress {
    peer: PeerRef,
    seen: i32,
    confirmed: i32,
    pending: Option<i32>,
    retry_at: Instant,
}

impl State {
    fn observe(&mut self, peer: PeerRef, seen: i32, confirmed: i32) {
        let progress = self.peers.entry(peer.id).or_insert_with(|| Progress {
            peer,
            seen: 0,
            confirmed: 0,
            pending: None,
            retry_at: Instant::now(),
        });
        progress.peer = peer;
        progress.seen = progress.seen.max(seen);
        progress.confirmed = progress.confirmed.max(confirmed);
    }

    fn requests(&mut self, now: Instant) -> Vec<(PeerRef, i32)> {
        self.peers
            .values_mut()
            .filter_map(|progress| {
                if progress.pending.is_some()
                    || progress.seen <= progress.confirmed
                    || progress.retry_at > now
                {
                    return None;
                }
                progress.pending = Some(progress.seen);
                Some((progress.peer, progress.seen))
            })
            .collect()
    }

    fn completed(&mut self, peer: PeerId, maximum: i32, success: bool, now: Instant) {
        if let Some(progress) = self.peers.get_mut(&peer)
            && progress.pending == Some(maximum)
        {
            progress.pending = None;
            if success {
                progress.confirmed = progress.confirmed.max(maximum);
            } else {
                progress.retry_at = now + Duration::from_secs(3);
            }
        }
    }

    fn confirm(&mut self, peer: PeerId, maximum: i32) {
        if let Some(progress) = self.peers.get_mut(&peer) {
            progress.confirmed = progress.confirmed.max(maximum);
        }
    }

    fn seen(&self, peer: PeerId) -> i32 {
        self.peers.get(&peer).map_or(0, |progress| progress.seen)
    }

    pub(super) fn refreshed_unread(&self, current: &Chat, incoming: &Chat) -> i32 {
        let last = current.last_message.as_ref().map(Message::id);
        let incoming_last = incoming.last_message.as_ref().map(Message::id);
        if last > incoming_last && last.is_some_and(|last| incoming.read_inbox_max_id < last) {
            return current.unread;
        }
        unread_from_snapshot(
            current.read_inbox_max_id,
            current.unread,
            self.seen(current.peer.id),
            incoming.read_inbox_max_id,
            incoming_last,
            incoming.unread,
        )
    }
}

pub(super) struct Event {
    peer: PeerId,
    maximum: i32,
    result: std::result::Result<(), String>,
}

#[derive(Default)]
pub(super) struct DialogReadState {
    pub outbox: i32,
    pub inbox: i32,
    pub unread: i32,
}

fn unread_from_snapshot(
    confirmed: i32,
    current: i32,
    seen: i32,
    incoming_read: i32,
    incoming_last: Option<i32>,
    incoming_unread: i32,
) -> i32 {
    if incoming_last.is_some_and(|last| last <= seen.max(confirmed)) {
        0
    } else if incoming_read < confirmed {
        current
    } else {
        incoming_unread.max(0)
    }
}

impl App {
    pub(super) fn observe_visible_reads(&mut self) -> bool {
        if self.open_chat.is_none()
            || self.messages_loading
            || self.discussion.as_ref().is_some_and(|view| !view.full_chat)
        {
            return false;
        }
        let Some(peer) = self.current_peer() else {
            return false;
        };
        let maximum = self
            .hitboxes
            .iter()
            .filter_map(|hit| {
                let HitTarget::Message(index) = hit.target else {
                    return None;
                };
                let message = self.messages.get(index)?;
                let group = message.grouped_id();
                Some(
                    self.messages
                        .iter()
                        .skip(index)
                        .take_while(|candidate| {
                            candidate.id() == message.id()
                                || (group.is_some() && candidate.grouped_id() == group)
                        })
                        .map(Message::id)
                        .max()
                        .unwrap_or(message.id()),
                )
            })
            .max()
            .unwrap_or(0);
        if maximum <= 0 {
            return false;
        }
        let chat = self.chats.iter_mut().find(|chat| chat.peer.id == peer.id);
        let confirmed = chat.as_ref().map_or(0, |chat| chat.read_inbox_max_id);
        self.inbox_reads.observe(peer, maximum, confirmed);
        if let Some(chat) = chat
            && chat.unread > 0
            && chat
                .last_message
                .as_ref()
                .is_some_and(|last| last.id() <= maximum)
        {
            chat.unread = 0;
            return true;
        }
        false
    }

    pub(super) fn poll_inbox_reads(&mut self) {
        for (peer, maximum) in self.inbox_reads.requests(Instant::now()) {
            let client = self.telegram.client.clone();
            let tx = self.events_tx.clone();
            tokio::spawn(async move {
                let result = tokio::time::timeout(Duration::from_secs(15), async {
                    if peer.id.kind() == PeerKind::Channel {
                        client
                            .invoke(&tl::functions::channels::ReadHistory {
                                channel: peer.into(),
                                max_id: maximum,
                            })
                            .await
                            .map(drop)
                    } else {
                        client
                            .invoke(&tl::functions::messages::ReadHistory {
                                peer: peer.into(),
                                max_id: maximum,
                            })
                            .await
                            .map(drop)
                    }
                })
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()));
                let _ = tx.send(UiEvent::InboxReadSent(Event {
                    peer: peer.id,
                    maximum,
                    result,
                }));
            });
        }
    }

    pub(super) fn inbox_read_sent(&mut self, event: Event) {
        self.inbox_reads.completed(
            event.peer,
            event.maximum,
            event.result.is_ok(),
            Instant::now(),
        );
        match event.result {
            Ok(()) => {
                if let Some(chat) = self
                    .chats
                    .iter_mut()
                    .find(|chat| chat.peer.id == event.peer)
                {
                    chat.read_inbox_max_id = chat.read_inbox_max_id.max(event.maximum);
                }
            }
            Err(error) => self.log_error(error),
        }
    }

    pub(super) fn apply_inbox_update(&mut self, peer: PeerId, maximum: i32, unread: i32) {
        self.inbox_reads.confirm(peer, maximum);
        if let Some(chat) = self.chats.iter_mut().find(|chat| chat.peer.id == peer) {
            chat.unread = unread_from_snapshot(
                chat.read_inbox_max_id,
                chat.unread,
                self.inbox_reads.seen(peer),
                maximum,
                chat.last_message.as_ref().map(Message::id),
                unread,
            );
            chat.read_inbox_max_id = chat.read_inbox_max_id.max(maximum);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_requests_coalesce_and_only_retry_failures_after_a_delay() {
        let peer = PeerRef {
            id: PeerId::user(7).unwrap(),
            auth: Default::default(),
        };
        let mut state = State::default();
        state.observe(peer, 50, 20);
        let now = Instant::now();
        assert_eq!(state.requests(now), vec![(peer, 50)]);
        state.observe(peer, 70, 20);
        assert!(state.requests(now).is_empty());
        state.completed(peer.id, 50, true, now);
        assert_eq!(state.requests(now), vec![(peer, 70)]);
        state.completed(peer.id, 70, false, now);
        assert!(state.requests(now + Duration::from_secs(2)).is_empty());
        assert_eq!(
            state.requests(now + Duration::from_secs(3)),
            vec![(peer, 70)]
        );
        state.completed(peer.id, 70, true, now);
        assert!(state.requests(now + Duration::from_secs(4)).is_empty());
    }

    #[test]
    fn stale_dialog_snapshots_cannot_restore_a_read_badge() {
        assert_eq!(unread_from_snapshot(10, 0, 50, 10, Some(50), 12), 0);
        assert_eq!(unread_from_snapshot(50, 0, 50, 10, Some(50), 12), 0);
        assert_eq!(unread_from_snapshot(50, 1, 50, 10, Some(51), 13), 1);
        assert_eq!(unread_from_snapshot(50, 1, 50, 50, Some(51), 1), 1);
        assert_eq!(unread_from_snapshot(50, 1, 50, 51, Some(51), 0), 0);
    }
}
