use grammers_session::types::PeerId;

use super::*;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct Target {
    peer: PeerId,
    id: i32,
}

#[derive(Default)]
pub(super) struct State {
    view_id: u64,
    originals: HashMap<Target, Original>,
}

enum Original {
    Loading,
    Ready(Box<Message>),
    Missing,
    Failed,
}

pub(super) enum Event {
    Loaded {
        view_id: u64,
        targets: Vec<Target>,
        result: std::result::Result<Vec<Option<Message>>, String>,
    },
    Open {
        view_id: u64,
        result: std::result::Result<Box<Discussion>, String>,
    },
}

pub(super) struct Preview {
    target: Option<Target>,
    pub author: String,
    pub text: String,
}

fn target_for(header: &tl::types::MessageReplyHeader, peer: PeerId) -> Option<Target> {
    Some(Target {
        peer: header
            .reply_to_peer_id
            .clone()
            .map(PeerId::from)
            .unwrap_or(peer),
        id: header.reply_to_msg_id.filter(|id| *id > 0)?,
    })
}

fn single_line(text: &str) -> String {
    text.chars()
        .take(512)
        .filter(|c| !c.is_control() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn summary(message: &Message, settings: &settings::State) -> String {
    let text = single_line(message.text());
    if !text.is_empty() {
        return text;
    }
    let fallback = if media::is_sticker(message) {
        "Sticker"
    } else if media::is_circle(message) {
        "Video message"
    } else if media::is_audio(message) {
        "Audio / voice message"
    } else if media::is_gif(message) {
        "GIF"
    } else if media::has_inline_preview(message) {
        "Photo / video"
    } else if message.media().is_some() {
        "Attachment"
    } else {
        "Service message"
    };
    settings.ui(fallback).to_owned()
}

pub(super) fn preview(app: &App, message: &Message) -> Option<Preview> {
    let tl::enums::MessageReplyHeader::Header(header) = message.reply_header()? else {
        return None;
    };
    let target = target_for(&header, message.peer_id());
    let quote = header
        .quote_text
        .as_deref()
        .map(single_line)
        .filter(|s| !s.is_empty());
    if target.is_none() && quote.is_none() && header.reply_from.is_none() {
        return None;
    }
    let cached = target.and_then(|target| {
        (app.replies.view_id == app.view_id)
            .then(|| app.replies.originals.get(&target))
            .flatten()
    });
    let original = target
        .and_then(|target| {
            app.messages
                .iter()
                .find(|m| m.id() == target.id && m.peer_id() == target.peer)
        })
        .or(match cached {
            Some(Original::Ready(message)) => Some(message.as_ref()),
            _ => None,
        });
    let author = original
        .map(|m| {
            if m.outgoing() {
                app.settings.ui("You")
            } else {
                Telegram::peer_name(m.sender())
            }
            .to_owned()
        })
        .or_else(|| {
            header.reply_from.as_ref().and_then(|from| {
                let tl::enums::MessageFwdHeader::Header(from) = from;
                from.from_name.clone().or_else(|| from.post_author.clone())
            })
        })
        .unwrap_or_else(|| app.settings.ui("Reply to message").to_owned());
    let text = quote
        .or_else(|| original.map(|message| summary(message, &app.settings)))
        .unwrap_or_else(|| {
            match cached {
                Some(Original::Missing) => app.settings.ui("Deleted message"),
                Some(Original::Failed) => app.settings.ui("Message unavailable"),
                _ => app.settings.ui("Loading message…"),
            }
            .to_owned()
        });
    Some(Preview {
        target,
        author: single_line(&author),
        text,
    })
}

impl App {
    pub(super) fn queue_reply_previews(&mut self, hitboxes: &[HitBox]) {
        if self.replies.view_id != self.view_id {
            self.replies = State {
                view_id: self.view_id,
                ..State::default()
            };
        }
        let Some(peer) = self.current_peer() else {
            return;
        };
        let mut local = Vec::new();
        let mut external = Vec::new();
        for hitbox in hitboxes {
            let HitTarget::Reply(target) = hitbox.target else {
                continue;
            };
            if self.replies.originals.contains_key(&target)
                || self
                    .messages
                    .iter()
                    .any(|m| m.id() == target.id && m.peer_id() == target.peer)
            {
                continue;
            }
            self.replies.originals.insert(target, Original::Loading);
            if target.peer == peer.id {
                local.push(target);
            } else if let Some(message) = self.messages.iter().find(|message| {
                matches!(message.reply_header(), Some(tl::enums::MessageReplyHeader::Header(h))
                    if target_for(&h, message.peer_id()) == Some(target))
            }) {
                external.push((target, message.clone()));
            }
        }
        let view_id = self.view_id;
        for targets in local.chunks(100) {
            let targets = targets.to_vec();
            let ids: Vec<_> = targets.iter().map(|target| target.id).collect();
            let client = self.telegram.client.clone();
            let tx = self.events_tx.clone();
            tokio::spawn(async move {
                let result = tokio::time::timeout(
                    Duration::from_secs(20),
                    client.get_messages_by_id(peer, &ids),
                )
                .await
                .map_err(|e| e.to_string())
                .and_then(|result| result.map_err(|e| e.to_string()));
                let _ = tx.send(UiEvent::Replies(Event::Loaded {
                    view_id,
                    targets,
                    result,
                }));
            });
        }
        for (target, message) in external {
            let tx = self.events_tx.clone();
            tokio::spawn(async move {
                let result = tokio::time::timeout(Duration::from_secs(20), message.get_reply())
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|result| result.map_err(|e| e.to_string()))
                    .map(|original| {
                        vec![original.filter(|m| m.id() == target.id && m.peer_id() == target.peer)]
                    });
                let _ = tx.send(UiEvent::Replies(Event::Loaded {
                    view_id,
                    targets: vec![target],
                    result,
                }));
            });
        }
    }

    pub(super) fn replies_event(&mut self, event: Event) {
        match event {
            Event::Loaded {
                view_id,
                targets,
                result,
            } if view_id == self.view_id && view_id == self.replies.view_id => match result {
                Ok(messages) => {
                    for (target, message) in targets.into_iter().zip(messages) {
                        self.replies.originals.insert(
                            target,
                            message.map_or(Original::Missing, |m| Original::Ready(Box::new(m))),
                        );
                    }
                }
                Err(error) => {
                    self.log_error(error);
                    for target in targets {
                        self.replies.originals.insert(target, Original::Failed);
                    }
                }
            },
            Event::Open { view_id, result } if view_id == self.view_id => match result {
                Ok(discussion) => {
                    let id = discussion.root_id;
                    self.enter_discussion(*discussion, true);
                    self.has_newer_messages = true;
                    self.jump_to_message(id);
                }
                Err(error) => self.set_error(error),
            },
            _ => {}
        }
    }

    pub(super) fn open_reply(&mut self, target: Target) {
        if self.current_peer().is_some_and(|p| p.id == target.peer) {
            self.jump_to_message(target.id);
            return;
        }
        if self
            .discussion
            .as_ref()
            .is_some_and(|d| d.channel_peer.id == target.peer)
        {
            self.leave_discussion();
            self.jump_to_message(target.id);
            return;
        }
        if let Some(index) = self
            .chats
            .iter()
            .position(|chat| chat.peer.id == target.peer)
        {
            self.open_chat(index);
            self.jump_to_message(target.id);
            return;
        }
        let Some(Original::Ready(original)) = self.replies.originals.get(&target) else {
            self.status = "Original message is not available yet".to_owned();
            return;
        };
        let original = original.as_ref().clone();
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        let view_id = self.view_id;
        self.status = "Opening original message…".to_owned();
        tokio::spawn(async move {
            let result = async {
                // get_reply() may retain the source chat in fetched_in; use
                // the original message's actual peer, not that lookup context.
                let peer_object = original
                    .peer()
                    .cloned()
                    .context("original chat is inaccessible")?;
                let peer = peer_object
                    .to_ref()
                    .await
                    .map_err(anyhow::Error::msg)?
                    .context("original chat is inaccessible")?;
                anyhow::ensure!(
                    peer.id == target.peer,
                    "original chat does not match the reply"
                );
                let name = Telegram::peer_name(Some(&peer_object)).to_owned();
                let (mut messages, newer) = tokio::try_join!(
                    Telegram::messages_before(
                        &client,
                        peer,
                        HISTORY_PAGE,
                        target.id.saturating_add(1)
                    ),
                    Telegram::messages_after(&client, peer, HISTORY_PAGE, target.id)
                )?;
                messages.extend(newer);
                messages.sort_by_key(Message::id);
                messages.dedup_by_key(|m| m.id());
                anyhow::ensure!(
                    messages.iter().any(|m| m.id() == target.id),
                    "original message is inaccessible"
                );
                Ok::<_, anyhow::Error>(Box::new(Discussion {
                    peer,
                    peer_object,
                    name,
                    root_id: target.id,
                    post_id: target.id,
                    messages,
                    has_more: true,
                }))
            }
            .await
            .map_err(|e| e.to_string());
            let _ = tx.send(UiEvent::Replies(Event::Open { view_id, result }));
        });
    }
}

pub(super) fn draw(
    window: &mut Window,
    preview: &Preview,
    origin: (i32, i32),
    width: i32,
    theme: &theme::Theme,
    hitboxes: &mut Vec<HitBox>,
) {
    let (row, column) = origin;
    for offset in 0..2 {
        window.posstyled(
            row + offset,
            column,
            '▎',
            CellStyle::foreground(theme.palette.accent),
        );
    }
    put(
        window,
        row,
        column + 2,
        &preview.author,
        CellStyle::foreground(theme.palette.accent),
        width - 2,
    );
    put(
        window,
        row + 1,
        column + 2,
        &preview.text,
        CellStyle::foreground(theme.palette.muted),
        width - 2,
    );
    if let Some(target) = preview.target {
        hitboxes.push(HitBox {
            rect: Rect {
                x: window.x + 1 + column,
                y: window.y + 1 + row,
                width,
                height: 2,
            },
            target: HitTarget::Reply(target),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_snippets_are_single_line_and_control_free() {
        assert_eq!(
            single_line("  Привет\n👨‍💻\tмир\u{1b}[31m  "),
            "Привет 👨‍💻 мир[31m"
        );
        assert!(single_line(&"Я".repeat(100_000)).chars().count() <= 512);
    }

    #[test]
    fn reply_preview_has_a_separate_two_row_hitbox_above_message_text() {
        let mut window = Window::new(0);
        window.x = 10;
        window.y = 3;
        window.width = 60;
        window.height = 20;
        let target = Target {
            peer: PeerId::user(1).unwrap(),
            id: 42,
        };
        let preview = Preview {
            target: Some(target),
            author: "Alice".to_owned(),
            text: "Original message".to_owned(),
        };
        let mut hitboxes = Vec::new();
        draw(
            &mut window,
            &preview,
            (4, 2),
            30,
            &theme::Theme::default(),
            &mut hitboxes,
        );
        assert_eq!(hitboxes.len(), 1);
        let hitbox = &hitboxes[0];
        assert!(matches!(hitbox.target, HitTarget::Reply(value) if value == target));
        assert!(hitbox.rect.contains(13, 8));
        assert!(hitbox.rect.contains(42, 9));
        assert!(
            !hitbox.rect.contains(13, 10),
            "body text must keep its own message-menu hitbox"
        );
        assert!(!hitbox.rect.contains(43, 8));
    }

    #[test]
    fn reply_targets_include_the_chat_not_only_the_message_id() {
        let peer = PeerId::user(1).unwrap();
        let mut header = tl::types::MessageReplyHeader {
            reply_to_scheduled: false,
            forum_topic: false,
            quote: false,
            reply_to_ephemeral: false,
            reply_to_msg_id: Some(42),
            reply_to_peer_id: None,
            reply_from: None,
            reply_media: None,
            reply_to_top_id: None,
            quote_text: None,
            quote_entities: None,
            quote_offset: None,
            todo_item_id: None,
            poll_option: None,
            quote_rich: None,
        };
        assert_eq!(target_for(&header, peer), Some(Target { peer, id: 42 }));
        header.reply_to_peer_id = Some(tl::types::PeerChannel { channel_id: 7 }.into());
        assert_ne!(target_for(&header, peer).unwrap().peer, peer);
        header.reply_to_msg_id = None;
        header.reply_to_top_id = Some(100);
        assert!(
            target_for(&header, peer).is_none(),
            "a topic root is not the replied message"
        );
    }
}
