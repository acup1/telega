use super::*;
use grammers_client::message::InputReactions;
use grammers_session::types::PeerId;

pub(super) struct Record {
    pub target: i32,
    pub emoji: i64,
    pub sender: PeerId,
    pub own: bool,
}
pub(super) fn record(message: &Message) -> Option<Record> {
    if message.media().is_some()
        || matches!(&message.raw,tl::enums::Message::Message(raw) if raw.fwd_from.is_some())
    {
        return None;
    }
    let (target, emoji) = parse(
        message.text(),
        message.fmt_entities()?,
        message.reply_to_message_id(),
    )?;
    Some(Record {
        target,
        emoji,
        sender: message.sender_id()?,
        own: message.outgoing(),
    })
}
fn parse(
    text: &str,
    entities: &[tl::enums::MessageEntity],
    reply: Option<i32>,
) -> Option<(i32, i64)> {
    if text.is_empty()
        || text.chars().any(char::is_control)
        || text.chars().count() > 32
        || UnicodeSegmentation::graphemes(text, true).count() != 1
    {
        return None;
    }
    if entities.len() != 1 {
        return None;
    }
    let tl::enums::MessageEntity::TextUrl(entity) = &entities[0] else {
        return None;
    };
    if entity.offset != 0 || entity.length != text.encode_utf16().count() as i32 {
        return None;
    }
    let (prefix, target) = entity.url.split_once("&telega_reaction=")?;
    let target = target.parse::<i32>().ok().filter(|id| *id > 0)?;
    if reply != Some(target) {
        return None;
    }
    Some((target, crate::custom_emoji::compatible_id(prefix)?))
}
pub(super) fn hidden(message: &Message, messages: &[Message]) -> bool {
    record(message).is_some_and(|reaction| {
        messages
            .iter()
            .any(|target| target.id() == reaction.target && target.peer_id() == message.peer_id())
    })
}
pub(super) fn own(messages: &[Message], peer: PeerId, target: i32) -> Vec<reactions::Key> {
    let mut seen = HashSet::new();
    messages
        .iter()
        .filter(|message| message.peer_id() == peer)
        .filter_map(record)
        .filter(|r| r.target == target && r.own)
        .filter_map(|r| {
            seen.insert(r.emoji)
                .then_some(reactions::Key::Custom(r.emoji))
        })
        .collect()
}
pub(super) fn counts(
    messages: &[Message],
    message: &Message,
    counts: Vec<tl::enums::ReactionCount>,
) -> Vec<tl::enums::ReactionCount> {
    let records = messages
        .iter()
        .filter(|source| source.peer_id() == message.peer_id())
        .filter_map(record)
        .filter(|r| r.target == message.id());
    merge_counts(counts, records)
}

fn merge_counts(
    mut counts: Vec<tl::enums::ReactionCount>,
    records: impl Iterator<Item = Record>,
) -> Vec<tl::enums::ReactionCount> {
    let mut seen = HashSet::new();
    for r in records {
        if !seen.insert((r.sender, r.emoji)) {
            continue;
        }
        if let Some(existing)=counts.iter_mut().find(|count|matches!(count,tl::enums::ReactionCount::Count(count) if matches!(&count.reaction,tl::enums::Reaction::CustomEmoji(custom) if custom.document_id==r.emoji))) {
            let tl::enums::ReactionCount::Count(count)=existing;
            if !r.own || count.chosen_order.is_none() { count.count+=1; }
            if r.own { count.chosen_order=Some(0); }
        } else { counts.push(tl::types::ReactionCount { chosen_order:r.own.then_some(0),reaction:tl::types::ReactionCustomEmoji { document_id:r.emoji }.into(),count:1 }.into()); }
    }
    counts
}

pub(super) struct Event {
    view: u64,
    target: i32,
    native: Vec<reactions::Key>,
    result: std::result::Result<Changed, String>,
}
struct Changed {
    removed: Vec<i32>,
    fresh: Option<Box<Message>>,
}

impl App {
    pub(super) fn remember_compatible(&mut self, message: Message) {
        self.compatible_reactions
            .retain(|old| old.id() != message.id() || old.peer_id() != message.peer_id());
        if record(&message).is_some() {
            self.compatible_reactions.push(message);
        }
    }
    pub(super) fn update_legacy_reactions(&mut self, index: usize, selected: Vec<reactions::Key>) {
        let Some(peer) = self.current_peer() else {
            return;
        };
        let message = self.messages[index].clone();
        let target = message.id();
        if !self.reaction_state.busy.insert(target) {
            return;
        }
        let old: Vec<_> = self
            .compatible_reactions
            .iter()
            .filter(|m| {
                m.peer_id() == peer.id && record(m).is_some_and(|r| r.own && r.target == target)
            })
            .cloned()
            .collect();
        let native = selected;
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        let view = self.view_id;
        self.message_menu = None;
        self.reaction_state.picker = None;
        tokio::spawn(async move {
            let result = async {
                let previous = reactions::selection(&message);
                let mut removed = Vec::new();
                if previous != native {
                    message
                        .react(
                            InputReactions::from(
                                native.iter().map(reactions::Key::raw).collect::<Vec<_>>(),
                            )
                            .add_to_recent(),
                        )
                        .await?;
                }
                // Only remove old replies after Telegram accepts the native
                // selection (or when removing a legacy-only reaction).
                for previous in old {
                    previous.delete().await?;
                    removed.push(previous.id());
                }
                let fresh = client
                    .get_messages_by_id(peer, &[target])
                    .await
                    .ok()
                    .and_then(|mut m| m.pop().flatten())
                    .map(Box::new);
                Ok::<_, anyhow::Error>(Changed { removed, fresh })
            }
            .await
            .map_err(|e| e.to_string());
            let _ = tx.send(UiEvent::CompatibleReaction(Event {
                view,
                target,
                native,
                result,
            }));
        });
    }

    pub(super) fn compatible_reaction_event(&mut self, event: Event) {
        if event.view != self.view_id {
            return;
        }
        self.reaction_state.busy.remove(&event.target);
        match event.result {
            Ok(changed) => {
                if let Some(index) = self.message_index_by_id(event.target) {
                    if let Some(fresh) = changed.fresh {
                        self.messages[index] = *fresh;
                    } else {
                        reactions::apply_selection(&mut self.messages[index], &event.native);
                    }
                }
                let peer = self.current_peer().map(|peer| peer.id);
                for id in changed.removed {
                    self.compatible_reactions
                        .retain(|message| message.id() != id || Some(message.peer_id()) != peer);
                    if let Some(index) = self.message_index_by_id(id) {
                        self.messages.remove(index);
                    }
                }
                self.status = "Reactions updated".into();
                self.status_error = false;
            }
            Err(error) => {
                self.reaction_error(error);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_carriers_do_not_inflate_counts_and_own_reaction_can_be_removed() {
        let records = vec![
            Record {
                target: 1,
                emoji: 9,
                sender: PeerId::user(1).unwrap(),
                own: true,
            },
            Record {
                target: 1,
                emoji: 9,
                sender: PeerId::user(1).unwrap(),
                own: true,
            },
            Record {
                target: 1,
                emoji: 9,
                sender: PeerId::user(2).unwrap(),
                own: false,
            },
        ];
        let counts = merge_counts(Vec::new(), records.into_iter());
        assert_eq!(counts.len(), 1);
        let tl::enums::ReactionCount::Count(count) = &counts[0];
        assert_eq!(count.count, 2);
        assert!(count.chosen_order.is_some());
        assert!(merge_counts(Vec::new(), std::iter::empty()).is_empty());
    }
    #[test]
    fn compatible_reactions_require_an_exact_entity_and_matching_reply() {
        let entity: tl::enums::MessageEntity = tl::types::MessageEntityTextUrl {
            offset: 0,
            length: 5,
            url: "tg://emoji?id=123&telega_reaction=45".into(),
        }
        .into();
        assert_eq!(
            parse("👨‍💻", std::slice::from_ref(&entity), Some(45)),
            Some((45, 123))
        );
        assert_eq!(parse("👨‍💻", std::slice::from_ref(&entity), Some(46)), None);
        assert_eq!(
            parse("unrelated text", std::slice::from_ref(&entity), Some(45)),
            None
        );
        assert_eq!(parse("👨‍💻", &[], Some(45)), None);
    }
}
