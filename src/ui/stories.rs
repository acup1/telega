use super::*;
use grammers_session::types::PeerId;

#[derive(Clone)]
pub(super) struct Entry {
    pub peer: PeerRef,
    pub name: String,
    pub unread: bool,
}

#[derive(Default)]
pub(super) struct State {
    pub entries: Vec<Entry>,
    pub loading: bool,
    pub refreshed: Option<Instant>,
    pub popup: bool,
    pub selected: usize,
    pub offset: usize,
}

impl App {
    pub(super) fn scroll_story_bar(
        &mut self,
        mouse_x: u16,
        mouse_y: u16,
        forward: bool,
        screen_width: u16,
    ) -> bool {
        if self.active_folder != 0 || self.stories.entries.is_empty() {
            return false;
        }
        let layout = pane_layout(screen_width, self.sidebar_is_compact());
        let x = i32::from(mouse_x);
        let y = i32::from(mouse_y);
        if x < layout.rail || x >= layout.chat_x || !(3..7).contains(&y) {
            return false;
        }
        let capacity = bar_capacity(layout.chats, self.sidebar_is_compact());
        let last = self.stories.entries.len().saturating_sub(capacity);
        self.stories.offset = if forward {
            (self.stories.offset + 1).min(last)
        } else {
            self.stories.offset.saturating_sub(1)
        };
        true
    }

    pub(super) fn poll_stories(&mut self) {
        if self.stories.loading
            || self
                .stories
                .refreshed
                .is_some_and(|at| at.elapsed() < Duration::from_secs(90))
        {
            return;
        }
        self.stories.loading = true;
        self.stories.refreshed = Some(Instant::now());
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        tokio::spawn(async move {
            let result = async {
                let mut entries = Vec::new();
                let mut seen = HashSet::new();
                let mut state = None;
                loop {
                    let response = client
                        .invoke(&tl::functions::stories::GetAllStories {
                            next: state.is_some(),
                            hidden: false,
                            state: state.clone(),
                        })
                        .await?;
                    let tl::enums::stories::AllStories::Stories(all) = response else {
                        break;
                    };
                    let mut peers: HashMap<PeerId, (PeerRef, String)> = HashMap::new();
                    for user in all.users {
                        let peer = PeerRef::from(user.clone());
                        let name = match user {
                            tl::enums::User::User(user) => user
                                .first_name
                                .or(user.username)
                                .unwrap_or_else(|| "User".into()),
                            _ => "User".into(),
                        };
                        peers.insert(peer.id, (peer, name));
                    }
                    for chat in all.chats {
                        let peer = PeerRef::from(chat.clone());
                        let name = match chat {
                            tl::enums::Chat::Chat(chat) => chat.title,
                            tl::enums::Chat::Channel(chat) => chat.title,
                            tl::enums::Chat::Community(chat) => chat.title,
                            _ => "Channel".into(),
                        };
                        peers.insert(peer.id, (peer, name));
                    }
                    for stories in all.peer_stories {
                        let tl::enums::PeerStories::Stories(stories) = stories;
                        let id = PeerId::from(stories.peer);
                        if let Some((peer, name)) = peers.get(&id)
                            && !stories.stories.is_empty()
                            && seen.insert(id)
                        {
                            let max_read = stories.max_read_id.unwrap_or_default();
                            let unread = stories.stories.iter().any(|story| story.id() > max_read);
                            entries.push(Entry {
                                peer: *peer,
                                name: name.clone(),
                                unread,
                            });
                        }
                    }
                    if !all.has_more || state.as_deref() == Some(all.state.as_str()) {
                        break;
                    }
                    state = Some(all.state);
                }
                Ok(entries)
            }
            .await
            .map_err(|error: grammers_client::InvocationError| error.to_string());
            let _ = tx.send(UiEvent::StoriesLoaded(result));
        });
    }

    pub(super) fn stories_event(&mut self, result: std::result::Result<Vec<Entry>, String>) {
        self.stories.loading = false;
        match result {
            Ok(entries) => {
                self.stories.entries = entries;
                self.stories.selected = self
                    .stories
                    .selected
                    .min(self.stories.entries.len().saturating_sub(1));
                self.stories.offset = self
                    .stories
                    .offset
                    .min(self.stories.entries.len().saturating_sub(1));
            }
            Err(error) => self.log_error(error),
        }
    }
}

pub(super) fn bar_height(app: &App) -> i32 {
    if app.active_folder != 0 {
        0
    } else if app.stories.entries.is_empty() {
        2
    } else {
        4
    }
}

pub(super) fn draw_bar(
    window: &mut Window,
    app: &App,
    hits: &mut Vec<HitBox>,
    images: &mut Vec<ImagePlacement>,
) {
    if app.active_folder != 0 {
        return;
    }
    put(
        window,
        2,
        2,
        app.settings.ui("Stories"),
        CellStyle::foreground(app.theme.palette.accent),
        window.width.saturating_sub(5),
    );
    if app.stories.entries.is_empty() {
        put(
            window,
            3,
            2,
            if app.stories.loading {
                app.settings
                    .label("lng_profile_loading", "Loading…", "Загрузка…")
            } else {
                app.settings.ui("No active stories")
            },
            CellStyle::foreground(app.theme.palette.muted),
            window.width.saturating_sub(5),
        );
        return;
    }
    let compact = app.sidebar_is_compact();
    let capacity = bar_capacity(window.width, compact);
    let start = app
        .stories
        .offset
        .min(app.stories.entries.len().saturating_sub(capacity));
    for (slot, (index, entry)) in app
        .stories
        .entries
        .iter()
        .enumerate()
        .skip(start)
        .take(capacity)
        .enumerate()
    {
        let x = 2 + slot as i32 * 7;
        let y = 3;
        if let Some(chat_index) = app
            .chats
            .iter()
            .position(|chat| chat.peer.id == entry.peer.id)
            && let Some(AvatarState::Ready(avatar)) = app.avatars.get(&chat_index)
        {
            let (columns, rows) = media::square_cells(4, 2);
            images.push(ImagePlacement {
                message_id: -1_600_000_000 - index as i32,
                asset: ImageAsset::Persistent(avatar.round.clone()),
                row: (window.y + 1 + y) as u16,
                column: (window.x + 1 + x) as u16,
                rows,
                columns,
                crop: false,
            });
        } else {
            put(
                window,
                y,
                x,
                &entry.name.chars().next().unwrap_or('?').to_string(),
                CellStyle::foreground(app.theme.palette.accent),
                2,
            );
        }
        let color = if entry.unread {
            app.theme.palette.accent
        } else {
            app.theme.palette.muted
        };
        put(
            window,
            y + 2,
            x,
            &fit_label(&entry.name, 5),
            CellStyle::foreground(color),
            5,
        );
        hits.push(HitBox {
            rect: Rect {
                x: window.x + 1 + x,
                y: window.y + 1 + y,
                width: 6,
                height: 3,
            },
            target: HitTarget::Story(index),
        });
    }
    if app.stories.entries.len() > capacity {
        let x = window.width - 4;
        put(
            window,
            3,
            x,
            "+",
            CellStyle::foreground(app.theme.palette.accent),
            2,
        );
        hits.push(HitBox {
            rect: Rect {
                x: window.x + 1 + x,
                y: window.y + 4,
                width: 3,
                height: 3,
            },
            target: HitTarget::StoryMore,
        });
    }
}

fn bar_capacity(width: i32, compact: bool) -> usize {
    if compact {
        1
    } else {
        ((width - 5) / 7).max(1) as usize
    }
}

pub(super) fn draw_popup(window: &mut Window, app: &App, hits: &mut Vec<HitBox>) {
    if !app.stories.popup {
        return;
    }
    window.border = 0;
    window.border_style = CellStyle::new(
        Some(app.theme.palette.border),
        Some(app.theme.palette.window),
    );
    window.name = Some(format!(" {} ", app.settings.ui("Stories")));
    window.clear();
    clear_area(window);
    hits.push(HitBox {
        rect: Rect {
            x: window.x,
            y: window.y,
            width: window.width,
            height: window.height,
        },
        target: HitTarget::StoryPanel,
    });
    put(
        window,
        0,
        2,
        app.settings.ui("Stories"),
        CellStyle::foreground(app.theme.palette.accent),
        window.width - 6,
    );
    put(
        window,
        0,
        window.width - 4,
        "×",
        CellStyle::foreground(app.theme.palette.accent),
        2,
    );
    hits.push(HitBox {
        rect: Rect {
            x: window.x + window.width - 4,
            y: window.y + 1,
            width: 3,
            height: 1,
        },
        target: HitTarget::StoryClose,
    });
    if app.stories.entries.is_empty() {
        put(
            window,
            2,
            2,
            if app.stories.loading {
                app.settings
                    .label("", "Loading stories…", "Загрузка историй…")
            } else {
                app.settings.ui("No available stories")
            },
            CellStyle::foreground(app.theme.palette.muted),
            window.width - 6,
        );
    }
    let visible = window.height.saturating_sub(4).max(0) as usize;
    let start = app
        .stories
        .selected
        .saturating_sub(visible.saturating_sub(1));
    for (row, (index, entry)) in app
        .stories
        .entries
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .enumerate()
    {
        let y = row as i32 + 1;
        let prefix = if index == app.stories.selected {
            "›"
        } else {
            " "
        };
        put(
            window,
            y,
            1,
            prefix,
            CellStyle::foreground(app.theme.palette.accent),
            2,
        );
        put(
            window,
            y,
            3,
            &entry.name,
            CellStyle::foreground(if entry.unread {
                app.theme.palette.accent
            } else {
                app.theme.palette.text
            }),
            window.width - 6,
        );
        hits.push(HitBox {
            rect: Rect {
                x: window.x + 1,
                y: window.y + 1 + y,
                width: window.width - 2,
                height: 1,
            },
            target: HitTarget::Story(index),
        });
    }
}
