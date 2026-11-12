use super::*;
use grammers_session::types::PeerKind;

#[derive(Default)]
pub(super) struct State {
    view_id: u64,
    sources: HashMap<PeerId, Source>,
    opening: Option<PeerId>,
}

enum Source {
    Loading,
    Ready(Box<Chat>),
    Unavailable,
}

pub(super) struct Preview {
    peer: Option<PeerId>,
    pub text: String,
}

pub(super) struct Event {
    view_id: u64,
    peer: PeerId,
    result: std::result::Result<Box<Chat>, String>,
}

fn source_id(header: &tl::types::MessageFwdHeader) -> Option<PeerId> {
    header.from_id.clone().map(PeerId::from).or_else(|| {
        // A hidden author must stay unlinked, including in Saved Messages.
        (header.from_name.is_none() && header.saved_from_name.is_none())
            .then(|| header.saved_from_peer.clone().map(PeerId::from))
            .flatten()
    })
}

fn message_source(message: &Message) -> Option<PeerId> {
    let tl::enums::MessageFwdHeader::Header(header) = message.forward_header()?;
    source_id(&header)
}

pub(super) fn preview(app: &App, message: &Message) -> Option<Preview> {
    let tl::enums::MessageFwdHeader::Header(header) = message.forward_header()?;
    let peer = source_id(&header);
    let known = peer.and_then(|id| {
        app.chats
            .iter()
            .find(|chat| chat.peer.id == id)
            .or_else(|| {
                if app.forwards.view_id == app.view_id {
                    match app.forwards.sources.get(&id) {
                        Some(Source::Ready(chat)) => Some(chat.as_ref()),
                        _ => None,
                    }
                } else {
                    None
                }
            })
    });
    Some(header_preview(
        &header,
        known.map(|chat| chat.name.as_str()),
        &app.settings,
    ))
}

fn header_preview(
    header: &tl::types::MessageFwdHeader,
    known_name: Option<&str>,
    settings: &settings::State,
) -> Preview {
    let peer = source_id(header);
    let name = known_name
        .or(header.from_name.as_deref())
        .or(header.saved_from_name.as_deref())
        .unwrap_or_else(|| match peer.map(PeerId::kind) {
            Some(PeerKind::Channel) => settings.label("", "Channel", "Канал"),
            Some(PeerKind::Chat) => settings.label("", "Group", "Группа"),
            Some(PeerKind::User) => settings.label("", "User", "Пользователь"),
            None => settings.label("", "Hidden sender", "Скрытый отправитель"),
        });
    let name: String = name.chars().filter(|c| !c.is_control()).take(256).collect();
    let prefix = if peer.is_some_and(|peer| peer.kind() != PeerKind::User) {
        settings.label("", "Forwarded from", "Переслано из")
    } else {
        settings.label("", "Forwarded from", "Переслано от")
    };
    Preview {
        peer,
        text: format!("{prefix} {name}"),
    }
}

impl App {
    fn reset_forward_sources(&mut self) {
        if self.forwards.view_id != self.view_id {
            self.forwards = State {
                view_id: self.view_id,
                ..State::default()
            };
        }
    }

    fn request_forward_source(&mut self, peer: PeerId) {
        self.reset_forward_sources();
        if self.forwards.sources.contains_key(&peer) {
            return;
        }
        if let Some(chat) = self.chats.iter().find(|chat| chat.peer.id == peer) {
            self.forwards
                .sources
                .insert(peer, Source::Ready(Box::new(chat.clone())));
            return;
        }
        let Some(context) = self.current_peer() else {
            return;
        };
        let Some(message) = self
            .messages
            .iter()
            .find(|message| message_source(message) == Some(peer))
        else {
            return;
        };
        let request = self
            .telegram
            .resolve_forward_source(peer, context, message.id());
        self.forwards.sources.insert(peer, Source::Loading);
        let tx = self.events_tx.clone();
        let view_id = self.view_id;
        tokio::spawn(async move {
            let result = tokio::time::timeout(Duration::from_secs(20), request)
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map(Box::new).map_err(|error| error.to_string()));
            let _ = tx.send(UiEvent::ForwardSource(Event {
                view_id,
                peer,
                result,
            }));
        });
    }

    pub(super) fn queue_forward_sources(&mut self, hits: &[HitBox]) {
        self.reset_forward_sources();
        for hit in hits {
            if let HitTarget::ForwardSource(peer) = hit.target {
                self.request_forward_source(peer);
            }
        }
    }

    pub(super) fn open_forward_source(&mut self, peer: PeerId) {
        self.reset_forward_sources();
        if let Some(chat) = self.chats.iter().find(|chat| chat.peer.id == peer).cloned() {
            self.open_remote_peer(chat);
            return;
        }
        if let Some(Source::Ready(chat)) = self.forwards.sources.get(&peer) {
            let chat = chat.as_ref().clone();
            self.open_remote_peer(chat);
            return;
        }
        if matches!(self.forwards.sources.get(&peer), Some(Source::Unavailable)) {
            self.forwards.sources.remove(&peer);
        }
        self.forwards.opening = Some(peer);
        self.request_forward_source(peer);
    }

    pub(super) fn forward_source_event(&mut self, event: Event) {
        if event.view_id != self.view_id || self.forwards.view_id != self.view_id {
            return;
        }
        let opening = self.forwards.opening == Some(event.peer);
        if opening {
            self.forwards.opening = None;
        }
        match event.result {
            Ok(chat) => {
                self.forwards
                    .sources
                    .insert(event.peer, Source::Ready(chat.clone()));
                if opening {
                    self.open_remote_peer(*chat);
                }
            }
            Err(error) => {
                self.forwards
                    .sources
                    .insert(event.peer, Source::Unavailable);
                if opening {
                    self.set_error(error);
                } else {
                    self.log_error(error);
                }
            }
        }
    }
}

pub(super) fn draw(
    window: &mut Window,
    preview: &Preview,
    origin: (i32, i32),
    width: i32,
    theme: &theme::Theme,
    hits: &mut Vec<HitBox>,
) {
    let label = fit_label(&preview.text, width.max(0) as usize);
    put(
        window,
        origin.0,
        origin.1,
        &label,
        CellStyle::foreground(theme.palette.accent),
        width,
    );
    if let Some(peer) = preview.peer {
        let width = display_width(&label) as i32;
        if width > 0 {
            hits.push(HitBox {
                rect: Rect {
                    x: window.x + 1 + origin.1,
                    y: window.y + 1 + origin.0,
                    width,
                    height: 1,
                },
                target: HitTarget::ForwardSource(peer),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> tl::types::MessageFwdHeader {
        tl::types::MessageFwdHeader {
            imported: false,
            saved_out: false,
            from_id: None,
            from_name: None,
            date: 0,
            channel_post: None,
            post_author: None,
            saved_from_peer: None,
            saved_from_msg_id: None,
            saved_from_id: None,
            saved_from_name: None,
            saved_date: None,
            psa_type: None,
        }
    }

    #[test]
    fn original_user_and_channel_take_precedence_over_saved_chat() {
        let mut header = header();
        header.saved_from_peer = Some(tl::types::PeerChat { chat_id: 90 }.into());
        header.from_id = Some(tl::types::PeerUser { user_id: 42 }.into());
        let user = header_preview(&header, Some("Alice"), &settings::State::default());
        assert_eq!(user.peer, PeerId::user(42));
        assert_eq!(user.text, "Forwarded from Alice");
        header.from_id = Some(tl::types::PeerChannel { channel_id: 42 }.into());
        header.post_author = Some("Signed author".into());
        let channel = header_preview(&header, Some("News"), &settings::State::default());
        assert_eq!(channel.peer, PeerId::channel(42));
        assert_eq!(channel.text, "Forwarded from News");
        assert_ne!(user.peer, channel.peer);
        header.from_id = None;
        assert_eq!(source_id(&header), PeerId::chat(90));
    }

    #[test]
    fn private_forward_names_are_visible_without_a_link_to_the_saved_chat() {
        let mut header = header();
        header.saved_from_peer = Some(tl::types::PeerChat { chat_id: 90 }.into());
        header.from_name = Some("Hidden Alice".into());
        let preview = header_preview(&header, None, &settings::State::default());
        assert_eq!(preview.peer, None);
        assert_eq!(preview.text, "Forwarded from Hidden Alice");
        header.from_name = None;
        header.saved_from_name = Some("Imported Alice".into());
        assert_eq!(source_id(&header), None);
    }

    #[test]
    fn forward_header_has_its_own_clipped_one_row_mouse_target() {
        let mut header = header();
        header.from_id = Some(tl::types::PeerChannel { channel_id: 42 }.into());
        let preview = header_preview(
            &header,
            Some("Very long channel name"),
            &settings::State::default(),
        );
        let mut window = Window::new(0);
        window.x = 5;
        window.y = 7;
        window.width = 60;
        window.height = 12;
        let mut hits = Vec::new();
        draw(
            &mut window,
            &preview,
            (3, 4),
            18,
            &theme::Theme::default(),
            &mut hits,
        );
        assert_eq!(hits.len(), 1);
        assert!(
            matches!(hits[0].target, HitTarget::ForwardSource(peer) if Some(peer) == PeerId::channel(42))
        );
        assert_eq!(hits[0].rect.x, 10);
        assert_eq!(hits[0].rect.y, 11);
        assert_eq!(hits[0].rect.height, 1);
        assert!(hits[0].rect.width <= 18);
        assert!(hits[0].rect.contains(10, 11));
        assert!(!hits[0].rect.contains(10, 12));
        header.from_id = None;
        header.from_name = Some("Private".into());
        let private = header_preview(&header, None, &settings::State::default());
        hits.clear();
        draw(
            &mut window,
            &private,
            (3, 4),
            18,
            &theme::Theme::default(),
            &mut hits,
        );
        assert!(hits.is_empty());
    }
}
