use grammers_client::update::Update;
use grammers_session::types::{PeerId, PeerKind};

use super::*;

#[derive(Default)]
pub(super) struct State {
    view_id: u64,
    pending: Vec<Event>,
}

impl State {
    fn enqueue(&mut self, view_id: u64, event: Event) {
        if self.view_id != view_id {
            self.pending.clear();
            self.view_id = view_id;
        }
        self.pending.push(event);
    }

    fn take(&mut self, view_id: u64) -> Vec<Event> {
        let pending = std::mem::take(&mut self.pending);
        if self.view_id == view_id {
            pending
        } else {
            Vec::new()
        }
    }
}

pub(super) enum Event {
    InboxRead {
        peer: PeerId,
        maximum: i32,
        unread: i32,
    },
    Reactions {
        peer: PeerId,
        id: i32,
        reactions: tl::enums::MessageReactions,
    },
    Message {
        message: Box<Message>,
        edited: bool,
    },
    Deleted {
        channel: Option<i64>,
        ids: Vec<i32>,
    },
    Error(String),
}

impl App {
    pub(super) async fn start_updates(
        &mut self,
    ) -> Result<(
        tokio::sync::oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    )> {
        let mut updates = self.telegram.take_updates().await?;
        let tx = self.events_tx.clone();
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            loop {
                let update =
                    tokio::select! { _=&mut stopped=>break, update=updates.next()=>update };
                let event = match update {
                    Ok(Update::NewMessage(message)) => Some(Event::Message {
                        message: Box::new(message.into_inner()),
                        edited: false,
                    }),
                    Ok(Update::MessageEdited(message)) => Some(Event::Message {
                        message: Box::new(message.into_inner()),
                        edited: true,
                    }),
                    Ok(Update::MessageDeleted(deleted)) => Some(Event::Deleted {
                        channel: deleted.channel_id(),
                        ids: deleted.into_messages(),
                    }),
                    Ok(Update::Raw(raw)) => match raw.raw {
                        tl::enums::Update::ReadHistoryInbox(update) => Some(Event::InboxRead {
                            peer: update.peer.into(),
                            maximum: update.max_id,
                            unread: update.still_unread_count,
                        }),
                        tl::enums::Update::ReadChannelInbox(update) => {
                            PeerId::channel(update.channel_id).map(|peer| Event::InboxRead {
                                peer,
                                maximum: update.max_id,
                                unread: update.still_unread_count,
                            })
                        }
                        tl::enums::Update::MessageReactions(update) => Some(Event::Reactions {
                            peer: update.peer.into(),
                            id: update.msg_id,
                            reactions: update.reactions,
                        }),
                        _ => None,
                    },
                    Err(error) => {
                        let _ = tx.send(UiEvent::Live(Event::Error(error.to_string())));
                        tokio::select! { _=&mut stopped=>break,_=tokio::time::sleep(Duration::from_secs(2))=>{} }
                        None
                    }
                    _ => None,
                };
                if let Some(event) = event
                    && tx.send(UiEvent::Live(event)).is_err()
                {
                    break;
                }
            }
            if let Err(error) = updates.sync_update_state().await {
                let _ = tx.send(UiEvent::Live(Event::Error(error.to_string())));
            }
        });
        Ok((stop, task))
    }

    fn live_matches(&self, message: &Message) -> bool {
        if self
            .current_peer()
            .is_none_or(|peer| peer.id != message.peer_id())
        {
            return false;
        }
        self.discussion.as_ref().is_none_or(|view| view.full_chat || message.id()==view.root_id ||
            matches!(message.reply_header(),Some(tl::enums::MessageReplyHeader::Header(header)) if header.reply_to_top_id==Some(view.root_id) || header.reply_to_msg_id==Some(view.root_id)))
    }

    pub(super) fn live_event(&mut self, event: Event) {
        match event {
            Event::InboxRead {
                peer,
                maximum,
                unread,
            } => self.apply_inbox_update(peer, maximum, unread),
            Event::Reactions {
                peer,
                id,
                reactions,
            } => {
                if self.current_peer().is_none_or(|p| p.id != peer) {
                    return;
                }
                if self.messages_loading {
                    self.live_state.enqueue(
                        self.view_id,
                        Event::Reactions {
                            peer,
                            id,
                            reactions,
                        },
                    );
                    return;
                }
                if let Some(index) = self.message_index_by_id(id)
                    && let tl::enums::Message::Message(raw) = &mut self.messages[index].raw
                {
                    raw.reactions = Some(reactions);
                }
            }
            Event::Message { message, edited } => {
                let message = *message;
                if let Some(chat) = self
                    .chats
                    .iter_mut()
                    .find(|chat| chat.peer.id == message.peer_id())
                    && updates_chat_preview(
                        chat.last_message.as_ref().map(Message::id),
                        message.id(),
                        edited,
                    )
                {
                    let is_new = chat
                        .last_message
                        .as_ref()
                        .is_none_or(|last| message.id() > last.id());
                    if is_new
                        && !edited
                        && !message.outgoing()
                        && message.id() > chat.read_inbox_max_id
                    {
                        chat.unread = chat.unread.saturating_add(1);
                    }
                    chat.last = message.text().replace('\n', " ");
                    chat.last_message = Some(message.clone());
                }
                if !self.live_matches(&message) {
                    return;
                }
                if self.messages_loading {
                    self.live_state.enqueue(
                        self.view_id,
                        Event::Message {
                            message: Box::new(message),
                            edited,
                        },
                    );
                    return;
                }
                self.merge_live(message, edited);
            }
            Event::Deleted { channel, ids } => {
                let Some(peer) = self.current_peer() else {
                    return;
                };
                if !deletion_matches(peer.id, channel) {
                    return;
                }
                if self.messages_loading {
                    self.live_state
                        .enqueue(self.view_id, Event::Deleted { channel, ids });
                    return;
                }
                for id in ids {
                    self.compatible_reactions
                        .retain(|message| message.id() != id || message.peer_id() != peer.id);
                    if let Some(index) = self.message_index_by_id(id) {
                        self.remove_message(index);
                    }
                }
            }
            Event::Error(error) => self.log_error(error),
        }
    }

    pub(super) fn merge_live(&mut self, message: Message, edited: bool) {
        self.remember_compatible(message.clone());
        if let Some(index) = self.message_index_by_id(message.id()) {
            if edited {
                self.previews.remove(&message.id());
                self.custom_emoji.remove(&message.id());
            }
            self.messages[index] = message;
        } else if !edited
            && !self.has_newer_messages
            && self
                .messages
                .last()
                .is_none_or(|last| message.id() > last.id())
        {
            let (scroll, selection) = after_append(
                self.scroll_from_bottom,
                self.message_index,
                self.messages.len(),
            );
            self.messages.push(message);
            self.scroll_from_bottom = scroll;
            self.message_index = if compat::hidden(self.messages.last().unwrap(), &self.messages) {
                self.message_index
            } else {
                selection
            };
        } else {
            return;
        }
        if let Some(index) = self.open_chat {
            self.queue_previews_near(index);
        }
    }

    pub(super) fn apply_pending_live(&mut self) {
        let pending = self.live_state.take(self.view_id);
        for event in pending {
            self.live_event(event);
        }
    }
}

fn after_append(scroll: usize, selected: usize, len: usize) -> (usize, usize) {
    if scroll == 0 {
        (0, len)
    } else {
        (scroll.saturating_add(1), selected)
    }
}

fn updates_chat_preview(current: Option<i32>, incoming: i32, edited: bool) -> bool {
    if edited {
        current == Some(incoming)
    } else {
        current.is_none_or(|current| incoming >= current)
    }
}

fn deletion_matches(peer: PeerId, channel: Option<i64>) -> bool {
    match channel {
        Some(id) => Some(peer) == PeerId::channel(id),
        None => peer.kind() != PeerKind::Channel,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arrivals_follow_latest_but_keep_scrolled_history_anchored() {
        assert_eq!(after_append(0, 9, 10), (0, 10));
        assert_eq!(after_append(6, 3, 10), (7, 3));
        assert!(deletion_matches(PeerId::channel(123).unwrap(), Some(123)));
        assert!(!deletion_matches(PeerId::channel(456).unwrap(), Some(123)));
        assert!(!deletion_matches(PeerId::channel(123).unwrap(), None));
        assert!(deletion_matches(PeerId::user(123).unwrap(), None));
    }

    #[test]
    fn catch_up_does_not_replace_a_newer_chat_preview_with_an_older_message() {
        assert!(!updates_chat_preview(Some(200), 150, false));
        assert!(updates_chat_preview(Some(200), 201, false));
        assert!(updates_chat_preview(Some(200), 200, true));
        assert!(!updates_chat_preview(Some(200), 150, true));
        assert!(updates_chat_preview(None, 150, false));
        assert!(!updates_chat_preview(None, 150, true));
    }

    #[test]
    fn history_load_retains_ordered_deletions_and_reactions_only_for_its_view() {
        let mut state = State::default();
        state.enqueue(
            3,
            Event::Deleted {
                channel: None,
                ids: vec![42],
            },
        );
        state.enqueue(
            3,
            Event::Reactions {
                peer: PeerId::user(123).unwrap(),
                id: 43,
                reactions: tl::types::MessageReactions {
                    min: false,
                    can_see_list: true,
                    reactions_as_tags: false,
                    results: Vec::new(),
                    recent_reactions: None,
                    top_reactors: None,
                }
                .into(),
            },
        );
        let events = state.take(3);
        assert!(
            matches!(&events[..], [Event::Deleted { ids, .. }, Event::Reactions { id: 43, .. }] if ids == &[42])
        );
        assert!(state.take(3).is_empty());

        state.enqueue(
            3,
            Event::Deleted {
                channel: None,
                ids: vec![44],
            },
        );
        assert!(state.take(4).is_empty());
        state.enqueue(
            4,
            Event::Deleted {
                channel: None,
                ids: vec![45],
            },
        );
        state.enqueue(
            5,
            Event::Deleted {
                channel: None,
                ids: vec![46],
            },
        );
        assert!(matches!(&state.take(5)[..], [Event::Deleted { ids, .. }] if ids == &[46]));
    }
}
