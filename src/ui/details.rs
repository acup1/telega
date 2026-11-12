use super::*;
use grammers_client::Client;
use grammers_client::media::Media;

type Outcome<T> = std::result::Result<T, String>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SharedTab {
    Media,
    Files,
    Music,
    Voice,
    Links,
    Pinned,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ProfileTab {
    Gifts,
    Posts,
    Attachments,
}

impl SharedTab {
    fn filter(self) -> tl::enums::MessagesFilter {
        use tl::enums::MessagesFilter::*;
        match self {
            Self::Media => InputMessagesFilterPhotoVideo,
            Self::Files => InputMessagesFilterDocument,
            Self::Music => InputMessagesFilterMusic,
            Self::Voice => InputMessagesFilterRoundVoice,
            Self::Links => InputMessagesFilterUrl,
            Self::Pinned => InputMessagesFilterPinned,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Media => "Media",
            Self::Files => "Files",
            Self::Music => "Music",
            Self::Voice => "Voice",
            Self::Links => "Links",
            Self::Pinned => "Pinned",
        }
    }
}

pub(super) struct Details {
    generation: u64,
    info_generation: u64,
    tab: SharedTab,
    pub(super) section: ProfileTab,
    posts: Vec<Post>,
    post_previews: HashMap<i32, PreviewState>,
    post_request: u64,
    post_loading: bool,
    post_has_more: bool,
    pub(super) messages: Vec<Message>,
    previews: HashMap<i32, PreviewState>,
    about: String,
    pub(super) avatar: Option<PathBuf>,
    loading: bool,
    has_more: bool,
    scroll: usize,
}

pub(super) struct Post {
    id: i32,
    date: i32,
    caption: String,
    media: Media,
}

#[derive(Default)]
pub(super) struct Pins {
    pub(super) messages: Vec<Message>,
    pub(super) loading: bool,
    has_more: bool,
    pub(super) cursor: usize,
    request: u64,
}

pub(super) enum DetailsEvent {
    PostsPage {
        request: u64,
        result: Outcome<Vec<Post>>,
    },
    PostPreview {
        request: u64,
        id: i32,
        result: Outcome<media::MemoryImage>,
    },
    Page {
        generation: u64,
        tab: SharedTab,
        result: Outcome<Vec<Message>>,
    },
    Info {
        generation: u64,
        result: Outcome<String>,
    },
    Avatar {
        generation: u64,
        result: Outcome<Option<PathBuf>>,
    },
    Preview {
        generation: u64,
        message_id: i32,
        result: Outcome<media::MemoryImage>,
    },
    Pins {
        view_id: u64,
        request: u64,
        reset: bool,
        result: Outcome<Vec<Message>>,
    },
    PinChanged {
        view_id: u64,
        message_id: i32,
        pinned: bool,
        result: Outcome<()>,
    },
    Jump {
        view_id: u64,
        message_id: i32,
        result: Outcome<Vec<Message>>,
    },
    Newer {
        view_id: u64,
        result: Outcome<Vec<Message>>,
    },
    Saved(Outcome<PathBuf>),
    ThemeInstalled(Outcome<()>),
}

async fn shared_page(
    client: &Client,
    peer: PeerRef,
    tab: SharedTab,
    offset: i32,
) -> Result<Vec<Message>> {
    let mut iter = client
        .search_messages(peer)
        .filter(tab.filter())
        .offset_id(offset);
    let mut page = Vec::new();
    while page.len() < HISTORY_PAGE {
        let Some(message) = iter.next().await? else {
            break;
        };
        page.push(message);
    }
    Ok(page)
}

impl App {
    pub(super) fn start_circle(&mut self, message: &Message, ratio: f64) {
        self.stop_audio();
        if let Some(index) = self.message_index_by_id(message.id()) {
            self.message_index = index;
            self.keep_selected_visible();
        }
        if !self.tgp_supported {
            self.start_audio_message(message.clone(), ratio);
            return;
        }
        match media::InlinePlayback::new(self.telegram.client.clone(), message, false, true) {
            Ok(mut playback) => {
                if ratio > 0.0
                    && let Err(error) = playback.seek(ratio)
                {
                    self.set_error(error);
                    return;
                }
                self.inline_video = Some((message.id(), playback));
            }
            Err(error) => self.set_error(error),
        }
    }

    pub(super) fn toggle_circle(&mut self, message: &Message) {
        if let Some((id, playback)) = &mut self.inline_video
            && *id == message.id()
        {
            if let Err(error) = playback.toggle() {
                self.set_error(error);
            }
        } else {
            self.start_circle(message, 0.0);
        }
    }

    pub(super) fn tick_inline_playback(&mut self) -> bool {
        let mut changed = false;
        if let Some((_, playback)) = &mut self.inline_video {
            match playback.tick() {
                Ok(value) => changed |= value,
                Err(error) => {
                    self.inline_video = None;
                    self.set_error(error);
                    changed = true;
                }
            }
        }
        let mut errors = Vec::new();
        for (id, playback) in &mut self.gifs {
            match playback.tick() {
                Ok(value) => changed |= value,
                Err(error) => errors.push((*id, error)),
            }
        }
        for (id, error) in errors {
            self.gifs.remove(&id);
            self.gif_failures.insert(id);
            self.log_error(error);
            changed = true;
        }
        changed
    }

    pub(super) fn sync_inline_playback(&mut self, placements: &mut [ImagePlacement]) {
        if !self.settings.autoplay {
            self.gifs.clear();
        }
        let visible: HashSet<_> = placements.iter().map(|p| p.message_id).collect();
        self.gifs.retain(|id, _| visible.contains(id));
        for placement in placements {
            if let Some((id, playback)) = &self.inline_video
                && *id == placement.message_id
                && let Some(image) = &playback.frame
            {
                placement.asset = ImageAsset::Video(image.clone(), playback.serial);
                continue;
            }
            if self.settings.autoplay
                && !self.gifs.contains_key(&placement.message_id)
                && !self.gif_failures.contains(&placement.message_id)
                && let Some(message) = self
                    .messages
                    .iter()
                    .find(|m| m.id() == placement.message_id && media::is_gif(m))
            {
                match media::InlinePlayback::new(self.telegram.client.clone(), message, true, false)
                {
                    Ok(playback) => {
                        self.gifs.insert(placement.message_id, playback);
                    }
                    Err(error) => {
                        self.gif_failures.insert(placement.message_id);
                        self.log_error(error);
                    }
                }
            }
            if let Some(playback) = self.gifs.get(&placement.message_id)
                && let Some(image) = &playback.frame
            {
                placement.asset = ImageAsset::Video(image.clone(), playback.serial);
            }
        }
    }

    pub(super) fn close_details(&mut self) {
        self.details = None;
        self.gifts = gifts::State::default();
        self.details_generation = self.details_generation.wrapping_add(1);
        self.details_rect = None;
        self.saving = None;
    }

    pub(super) fn open_details(&mut self, tab: SharedTab) {
        if self.open_chat.is_none() {
            return;
        }
        self.close_details();
        self.message_menu = None;
        self.attachment_menu = false;
        self.composing = false;
        let generation = self.details_generation;
        self.details = Some(Details {
            generation,
            info_generation: generation,
            tab,
            section: ProfileTab::Attachments,
            posts: Vec::new(),
            post_previews: HashMap::new(),
            post_request: generation,
            post_loading: false,
            post_has_more: true,
            messages: Vec::new(),
            previews: HashMap::new(),
            about: String::new(),
            avatar: None,
            loading: false,
            has_more: true,
            scroll: 0,
        });
        self.load_shared_page();
        let Some(peer) = self.current_peer() else {
            return;
        };
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        tokio::spawn(async move {
            let result = profile_about(&client, peer)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(UiEvent::Details(DetailsEvent::Info { generation, result }));
        });
        if self.tgp_supported {
            let peer_object = self
                .discussion
                .as_ref()
                .map(|v| v.peer_object.clone())
                .or_else(|| self.current_chat().map(|c| c.peer_object.clone()));
            if let Some(peer_object) = peer_object {
                let client = self.telegram.client.clone();
                let cache = self.avatar_cache.clone();
                let tx = self.events_tx.clone();
                tokio::spawn(async move {
                    let result = media::prepare_avatar_size(client, peer_object, cache, true)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(UiEvent::Details(DetailsEvent::Avatar {
                        generation,
                        result,
                    }));
                });
            }
        }
    }

    pub(super) fn change_profile_tab(&mut self, section: ProfileTab) {
        let Some(panel) = self.details.as_mut() else {
            return;
        };
        if panel.section == section {
            return;
        }
        panel.section = section;
        panel.scroll = 0;
        if section == ProfileTab::Gifts {
            if let Some(peer) = self.current_peer() {
                self.gifts = gifts::State {
                    peer: Some(peer),
                    ..gifts::State::default()
                };
                self.load_gift_page();
            }
        } else if section == ProfileTab::Posts {
            self.load_posts_page();
        }
    }

    pub(super) fn load_posts_page(&mut self) {
        let Some(peer) = self.current_peer() else {
            return;
        };
        let Some(panel) = self.details.as_mut() else {
            return;
        };
        if panel.post_loading || !panel.post_has_more {
            return;
        }
        panel.post_loading = true;
        let request = panel.post_request;
        let offset = panel.posts.last().map_or(0, |post| post.id);
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        tokio::spawn(async move {
            let result = async {
                let response: tl::types::stories::Stories = client
                    .invoke(&tl::functions::stories::GetPinnedStories {
                        peer: peer.into(),
                        offset_id: offset,
                        limit: 30,
                    })
                    .await?
                    .into();
                let posts = response
                    .stories
                    .into_iter()
                    .filter_map(|item| {
                        let tl::enums::StoryItem::Item(item) = item else {
                            return None;
                        };
                        let media = Media::from_raw(item.media)?;
                        Some(Post {
                            id: item.id,
                            date: item.date,
                            caption: item.caption.unwrap_or_default(),
                            media,
                        })
                    })
                    .collect();
                Ok::<_, grammers_client::InvocationError>(posts)
            }
            .await
            .map_err(|error| error.to_string());
            let _ = tx.send(UiEvent::Details(DetailsEvent::PostsPage {
                request,
                result,
            }));
        });
    }

    pub(super) fn queue_post_previews(&mut self, hits: &[HitBox]) {
        if !self.tgp_supported {
            return;
        }
        let cache = self.view_cache().join("posts");
        let visible: Vec<_> = hits
            .iter()
            .filter_map(|hit| {
                if let HitTarget::ProfilePost(id) = hit.target {
                    Some(id)
                } else {
                    None
                }
            })
            .collect();
        let Some(panel) = self.details.as_mut() else {
            return;
        };
        if panel.section != ProfileTab::Posts {
            return;
        }
        let request = panel.post_request;
        let jobs: Vec<_> = panel
            .posts
            .iter()
            .filter(|post| {
                visible.contains(&post.id) && !panel.post_previews.contains_key(&post.id)
            })
            .map(|post| (post.id, post.media.clone()))
            .collect();
        for (id, source) in jobs {
            panel.post_previews.insert(id, PreviewState::Loading);
            let client = self.telegram.client.clone();
            let tx = self.events_tx.clone();
            let cache = cache.clone();
            let workers = self.preview_workers.clone();
            tokio::spawn(async move {
                let result = match workers.acquire_owned().await {
                    Ok(_permit) => {
                        let _ = fs::create_dir_all(&cache);
                        media::prepare_media_thumbnail(
                            client,
                            source,
                            cache.join(format!("{id}.jpg")),
                        )
                        .await
                        .map_err(|error| error.to_string())
                    }
                    Err(_) => Err("post preview worker stopped".into()),
                };
                let _ = tx.send(UiEvent::Details(DetailsEvent::PostPreview {
                    request,
                    id,
                    result,
                }));
            });
        }
    }

    pub(super) fn change_shared_tab(&mut self, tab: SharedTab) {
        let Some(panel) = self.details.as_mut() else {
            return;
        };
        if panel.tab == tab {
            return;
        }
        panel.tab = tab;
        panel.messages.clear();
        panel.previews.clear();
        panel.scroll = 0;
        panel.loading = false;
        panel.has_more = true;
        // Each tab selection owns its requests, including rapid A -> B -> A changes.
        self.details_generation = self.details_generation.wrapping_add(1);
        panel.generation = self.details_generation;
        self.load_shared_page();
    }

    pub(super) fn load_shared_page(&mut self) {
        let Some(peer) = self.current_peer() else {
            return;
        };
        let Some(panel) = self.details.as_mut() else {
            return;
        };
        if panel.loading || !panel.has_more {
            return;
        }
        panel.loading = true;
        let generation = panel.generation;
        let tab = panel.tab;
        let offset = panel.messages.last().map_or(0, Message::id);
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        tokio::spawn(async move {
            let result = shared_page(&client, peer, tab, offset)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(UiEvent::Details(DetailsEvent::Page {
                generation,
                tab,
                result,
            }));
        });
    }

    pub(super) fn scroll_details(&mut self, down: bool) {
        let Some(panel) = self.details.as_mut() else {
            return;
        };
        if panel.section == ProfileTab::Gifts {
            if down {
                if self.gifts.selected + 1 < self.gifts.entries.len() {
                    self.gifts.selected += 1;
                } else {
                    self.load_gift_page();
                }
            } else {
                self.gifts.selected = self.gifts.selected.saturating_sub(1);
            }
            return;
        }
        if panel.section == ProfileTab::Posts {
            let rows = panel.posts.len().div_ceil(3);
            panel.scroll = if down {
                (panel.scroll + 1).min(rows.saturating_sub(1))
            } else {
                panel.scroll.saturating_sub(1)
            };
            if down && panel.scroll + 8 >= rows {
                self.load_posts_page();
            }
            return;
        }
        let rows = if panel.tab == SharedTab::Media {
            panel.messages.len().div_ceil(3)
        } else {
            panel.messages.len()
        };
        panel.scroll = if down {
            (panel.scroll + 1).min(rows.saturating_sub(1))
        } else {
            panel.scroll.saturating_sub(1)
        };
        if down && panel.scroll + 8 >= rows {
            self.load_shared_page();
        }
    }

    fn queue_shared_previews(&mut self) {
        if !self.tgp_supported {
            return;
        }
        let cache = self.view_cache();
        let Some(panel) = self.details.as_mut() else {
            return;
        };
        let generation = panel.generation;
        for message in panel
            .messages
            .iter()
            .filter(|m| media::has_inline_preview(m))
        {
            let message_id = message.id();
            if panel.previews.contains_key(&message_id) {
                continue;
            }
            if let Some(PreviewState::Ready(asset)) = self.previews.get(&message_id) {
                panel
                    .previews
                    .insert(message_id, PreviewState::Ready(asset.clone()));
                continue;
            }
            panel.previews.insert(message_id, PreviewState::Loading);
            let client = self.telegram.client.clone();
            let cache = cache.join(format!("shared-{generation}"));
            let tx = self.events_tx.clone();
            let workers = self.preview_workers.clone();
            let message = message.clone();
            tokio::spawn(async move {
                let result = match workers.acquire_owned().await {
                    Ok(_permit) => media::prepare_inline_preview(client, message, cache)
                        .await
                        .map_err(|e| e.to_string()),
                    Err(_) => Err("preview worker stopped".to_owned()),
                };
                let _ = tx.send(UiEvent::Details(DetailsEvent::Preview {
                    generation,
                    message_id,
                    result,
                }));
            });
        }
    }

    pub(super) fn load_pins(&mut self, reset: bool) {
        if self.pins.loading && !reset {
            return;
        }
        let Some(peer) = self.current_peer() else {
            return;
        };
        if self.open_chat.is_none() {
            return;
        }
        self.pins.loading = true;
        self.pins.request = self.pins.request.wrapping_add(1);
        let request = self.pins.request;
        let offset = if reset {
            0
        } else {
            self.pins.messages.last().map_or(0, Message::id)
        };
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        let view_id = self.view_id;
        tokio::spawn(async move {
            let result = shared_page(&client, peer, SharedTab::Pinned, offset)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(UiEvent::Details(DetailsEvent::Pins {
                view_id,
                request,
                reset,
                result,
            }));
        });
    }

    pub(super) fn is_pinned(&self, message_id: i32) -> bool {
        self.pins.messages.iter().any(|m| m.id() == message_id)
            || self
                .messages
                .iter()
                .any(|m| m.id() == message_id && m.pinned())
    }

    pub(super) fn toggle_pin(&mut self, index: usize) {
        let Some(message) = self.messages.get(index).cloned() else {
            return;
        };
        let message_id = message.id();
        let pinned = !self.is_pinned(message_id);
        self.message_menu = None;
        self.status = if pinned { "Pinning..." } else { "Unpinning..." }.to_owned();
        let view_id = self.view_id;
        let tx = self.events_tx.clone();
        tokio::spawn(async move {
            let result = if pinned {
                message.pin().await
            } else {
                message.unpin().await
            }
            .map_err(|e| e.to_string());
            let _ = tx.send(UiEvent::Details(DetailsEvent::PinChanged {
                view_id,
                message_id,
                pinned,
                result,
            }));
        });
    }

    pub(super) fn next_pin(&mut self) {
        let Some(message_id) = self.pins.messages.get(self.pins.cursor).map(Message::id) else {
            return;
        };
        self.pins.cursor = (self.pins.cursor + 1) % self.pins.messages.len().max(1);
        let more = self.pins.cursor == 0 && self.pins.has_more;
        self.jump_to_message(message_id);
        if more {
            self.load_pins(false);
        }
    }

    pub(super) fn jump_to_message(&mut self, message_id: i32) {
        self.close_details();
        self.message_menu = None;
        if let Some(index) = self.message_index_by_id(message_id) {
            self.select_message(index);
            self.keep_selected_visible();
            return;
        }
        let Some(peer) = self.current_peer() else {
            return;
        };
        if let Some(view) = &mut self.discussion {
            view.full_chat = true;
        }
        self.messages_loading = true;
        self.view_id = self.view_id.wrapping_add(1);
        self.read_poll_pending = false;
        self.reaction_state = reactions::State::default();
        self.loading_older = false;
        self.loading_newer = false;
        self.pins.loading = false;
        let view_id = self.view_id;
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        self.status = "Opening message...".to_owned();
        tokio::spawn(async move {
            let result = async {
                let (older, newer) = tokio::try_join!(
                    Telegram::messages_before(
                        &client,
                        peer,
                        HISTORY_PAGE,
                        message_id.saturating_add(1)
                    ),
                    Telegram::messages_after(&client, peer, HISTORY_PAGE, message_id)
                )?;
                let mut messages = older;
                messages.extend(newer);
                messages.sort_by_key(Message::id);
                messages.dedup_by_key(|m| m.id());
                anyhow::ensure!(
                    messages.iter().any(|m| m.id() == message_id),
                    "message was deleted or is inaccessible"
                );
                Ok::<_, anyhow::Error>(messages)
            }
            .await
            .map_err(|e| e.to_string());
            let _ = tx.send(UiEvent::Details(DetailsEvent::Jump {
                view_id,
                message_id,
                result,
            }));
        });
    }

    pub(super) fn jump_to_latest(&mut self) {
        self.message_menu = None;
        self.scroll_from_bottom = 0;
        self.message_index = self.messages.len().saturating_sub(1);
        // Even an ordinary loaded tail can be stale. Latest always refreshes
        // from Telegram, independent of the pagination flags of this window.
        let (Some(chat_index), Some(peer)) = (self.open_chat, self.current_peer()) else {
            return;
        };
        let comments = self
            .discussion
            .as_ref()
            .filter(|d| !d.full_chat)
            .map(|d| (d.channel_peer, d.post_id));
        self.stop_audio();
        self.gifs.clear();
        self.gif_failures.clear();
        self.view_id = self.view_id.wrapping_add(1);
        self.read_poll_pending = false;
        self.loading_older = false;
        self.loading_newer = false;
        self.has_newer_messages = false;
        self.messages_loading = true;
        self.messages.clear();
        self.previews.clear();
        self.custom_emoji.clear();
        self.reaction_state = reactions::State::default();
        self.pins.loading = false;
        self.status = "Opening latest messages…".to_owned();
        let view_id = self.view_id;
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        tokio::spawn(async move {
            let result = if let Some((channel, post_id)) = comments {
                Telegram::discussion(&client, channel, post_id, HISTORY_PAGE, 0)
                    .await
                    .map(|d| d.messages)
            } else {
                Telegram::messages_with(&client, peer, HISTORY_PAGE).await
            }
            .map_err(|e| e.to_string());
            let _ = tx.send(UiEvent::MessagesLoaded {
                chat_index,
                view_id,
                result,
            });
        });
        self.request_reaction_catalog();
    }

    pub(super) fn load_newer(&mut self) {
        if !self.has_newer_messages || self.loading_newer {
            return;
        }
        let Some(peer) = self.current_peer() else {
            return;
        };
        let Some(offset) = self.messages.last().map(Message::id) else {
            return;
        };
        self.loading_newer = true;
        let view_id = self.view_id;
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        tokio::spawn(async move {
            let result = Telegram::messages_after(&client, peer, HISTORY_PAGE, offset)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(UiEvent::Details(DetailsEvent::Newer { view_id, result }));
        });
    }

    pub(super) fn details_event(&mut self, event: DetailsEvent) {
        match event {
            DetailsEvent::PostsPage { request, result }
                if self
                    .details
                    .as_ref()
                    .is_some_and(|panel| panel.post_request == request) =>
            {
                let panel = self.details.as_mut().unwrap();
                panel.post_loading = false;
                match result {
                    Ok(page) => {
                        let count = page.len();
                        let known: HashSet<_> = panel.posts.iter().map(|post| post.id).collect();
                        let old_len = panel.posts.len();
                        panel
                            .posts
                            .extend(page.into_iter().filter(|post| !known.contains(&post.id)));
                        panel.post_has_more = count == 30 && panel.posts.len() > old_len;
                    }
                    Err(error) => self.set_error(error),
                }
            }
            DetailsEvent::PostPreview {
                request,
                id,
                result,
            } if self
                .details
                .as_ref()
                .is_some_and(|panel| panel.post_request == request) =>
            {
                if let Err(error) = &result {
                    self.log_error(error);
                }
                if let Some(panel) = self.details.as_mut() {
                    panel.post_previews.insert(
                        id,
                        match result {
                            Ok(image) => PreviewState::Ready(ImageAsset::Memory(image)),
                            Err(_) => PreviewState::Failed,
                        },
                    );
                }
            }
            DetailsEvent::Page {
                generation,
                tab,
                result,
            } if self
                .details
                .as_ref()
                .is_some_and(|p| p.generation == generation && p.tab == tab) =>
            {
                let panel = self.details.as_mut().unwrap();
                panel.loading = false;
                match result {
                    Ok(page) => {
                        panel.has_more = page.len() == HISTORY_PAGE;
                        let ids: HashSet<_> = panel.messages.iter().map(Message::id).collect();
                        panel
                            .messages
                            .extend(page.into_iter().filter(|m| !ids.contains(&m.id())));
                        self.queue_shared_previews();
                    }
                    Err(error) => self.set_error(error),
                }
            }
            DetailsEvent::Info { generation, result }
                if self
                    .details
                    .as_ref()
                    .is_some_and(|p| p.info_generation == generation) =>
            {
                match result {
                    Ok(about) => {
                        if let Some(panel) = &mut self.details {
                            panel.about = about;
                        }
                    }
                    Err(error) => self.set_error(error),
                }
            }
            DetailsEvent::Avatar { generation, result }
                if self
                    .details
                    .as_ref()
                    .is_some_and(|p| p.info_generation == generation) =>
            {
                match result {
                    Ok(avatar) => {
                        if let Some(panel) = &mut self.details {
                            panel.avatar = avatar;
                        }
                    }
                    Err(error) => self.set_error(error),
                }
            }
            DetailsEvent::Preview {
                generation,
                message_id,
                result,
            } if generation == self.details_generation => {
                if let Err(error) = &result {
                    self.log_error(error);
                }
                if let Some(panel) = &mut self.details {
                    panel.previews.insert(
                        message_id,
                        match result {
                            Ok(image) => PreviewState::Ready(ImageAsset::Memory(image)),
                            Err(_) => PreviewState::Failed,
                        },
                    );
                }
            }
            DetailsEvent::Pins {
                view_id,
                request,
                reset,
                result,
            } if view_id == self.view_id && request == self.pins.request => {
                self.pins.loading = false;
                match result {
                    Ok(page) => {
                        self.pins.has_more = page.len() == HISTORY_PAGE;
                        if reset {
                            self.pins.messages.clear();
                        }
                        let ids: HashSet<_> = self.pins.messages.iter().map(Message::id).collect();
                        self.pins
                            .messages
                            .extend(page.into_iter().filter(|m| !ids.contains(&m.id())));
                        self.pins.cursor = self
                            .pins
                            .cursor
                            .min(self.pins.messages.len().saturating_sub(1));
                        if reset && !self.pins.has_more {
                            let pinned_ids: HashSet<_> =
                                self.pins.messages.iter().map(Message::id).collect();
                            for message in &mut self.messages {
                                if let tl::enums::Message::Message(raw) = &mut message.raw {
                                    raw.pinned = pinned_ids.contains(&raw.id);
                                }
                            }
                        }
                    }
                    Err(error) => self.set_error(error),
                }
            }
            DetailsEvent::PinChanged {
                view_id,
                message_id,
                pinned,
                result,
            } if view_id == self.view_id => match result {
                Ok(()) => {
                    for message in self.messages.iter_mut().filter(|m| m.id() == message_id) {
                        if let tl::enums::Message::Message(raw) = &mut message.raw {
                            raw.pinned = pinned;
                        }
                    }
                    if !pinned {
                        self.pins.messages.retain(|m| m.id() != message_id);
                    }
                    self.load_pins(true);
                    if self
                        .details
                        .as_ref()
                        .is_some_and(|p| p.tab == SharedTab::Pinned)
                    {
                        self.open_details(SharedTab::Pinned);
                    }
                    self.status = if pinned {
                        "Message pinned"
                    } else {
                        "Message unpinned"
                    }
                    .to_owned();
                    self.status_error = false;
                }
                Err(error) => self.set_error(error),
            },
            DetailsEvent::Jump {
                view_id,
                message_id,
                result,
            } if view_id == self.view_id => {
                self.messages_loading = false;
                match result {
                    Ok(messages) => {
                        self.messages = messages;
                        self.previews.retain(|id, state| {
                            matches!(state, PreviewState::Ready(_))
                                && self.messages.iter().any(|m| m.id() == *id)
                        });
                        self.custom_emoji.clear();
                        self.message_index = self.message_index_by_id(message_id).unwrap_or(0);
                        self.keep_selected_visible();
                        self.has_more_messages = true;
                        self.has_newer_messages = true;
                        if let Some(index) = self.open_chat {
                            self.queue_previews(index);
                        }
                        self.status = "Ready".to_owned();
                        self.status_error = false;
                        self.request_reaction_catalog();
                    }
                    Err(error) => self.set_error(error),
                }
                self.apply_pending_live();
            }
            DetailsEvent::Newer { view_id, result } if view_id == self.view_id => {
                self.loading_newer = false;
                match result {
                    Ok(page) => {
                        self.has_newer_messages = page.len() == HISTORY_PAGE;
                        let ids: HashSet<_> = self.messages.iter().map(Message::id).collect();
                        let page: Vec<_> = page
                            .into_iter()
                            .filter(|m| !ids.contains(&m.id()))
                            .collect();
                        self.scroll_from_bottom += page.len();
                        self.messages.extend(page);
                        if let Some(index) = self.open_chat {
                            self.queue_previews(index);
                        }
                    }
                    Err(error) => self.set_error(error),
                }
            }
            DetailsEvent::Saved(result) => match result {
                Ok(path) => {
                    self.status = format!("Saved: {}", path.display());
                    self.status_error = false;
                }
                Err(error) => self.set_error(error),
            },
            DetailsEvent::ThemeInstalled(Err(error)) => self.set_error(error),
            _ => {}
        }
    }

    pub(super) fn apply_theme_preview(&mut self) {
        let Some(theme) = self.theme_preview.take() else {
            return;
        };
        if let Err(error) = theme.save(&self.settings_root) {
            self.set_error(error);
            return;
        }
        self.status = format!("Theme: {}", theme.name);
        self.status_error = false;
        if let Some(remote) = theme.remote.clone() {
            let client = self.telegram.client.clone();
            let tx = self.events_tx.clone();
            let color = theme.palette.chat;
            let dark =
                u32::from(color.r) * 299 + u32::from(color.g) * 587 + u32::from(color.b) * 114
                    < 128_000;
            tokio::spawn(async move {
                let result = async {
                    client
                        .invoke(&tl::functions::account::SaveTheme {
                            theme: remote.clone(),
                            unsave: false,
                        })
                        .await?;
                    client
                        .invoke(&tl::functions::account::InstallTheme {
                            dark,
                            theme: Some(remote),
                            format: Some("tdesktop".to_owned()),
                            base_theme: None,
                        })
                        .await?;
                    Ok::<_, anyhow::Error>(())
                }
                .await
                .map_err(|e| e.to_string());
                let _ = tx.send(UiEvent::Details(DetailsEvent::ThemeInstalled(result)));
            });
        }
        self.theme = theme;
    }

    pub(super) fn save_shared_file(&mut self) {
        let Some(message) = self.saving.take() else {
            return;
        };
        let path = PathBuf::from(std::mem::take(&mut self.input));
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        self.status = "Downloading file...".to_owned();
        tokio::spawn(async move {
            let result = async {
                let media = message.media().context("attachment is unavailable")?;
                // Never overwrite an existing destination, including a racing writer.
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&path)?;
                let download = async {
                    let mut iter = client.iter_download(&media);
                    while let Some(chunk) = iter.next().await? {
                        file.write_all(&chunk)?;
                    }
                    file.sync_all()?;
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                drop(file);
                if let Err(error) = download {
                    let _ = fs::remove_file(&path);
                    return Err(error);
                }
                Ok(path)
            }
            .await
            .map_err(|e: anyhow::Error| e.to_string());
            let _ = tx.send(UiEvent::Details(DetailsEvent::Saved(result)));
        });
    }
}

async fn profile_about(client: &Client, peer: PeerRef) -> Result<String> {
    use grammers_session::types::PeerKind;
    match peer.id.kind() {
        PeerKind::User => {
            let raw: tl::types::users::UserFull = client
                .invoke(&tl::functions::users::GetFullUser { id: peer.into() })
                .await?
                .into();
            let tl::enums::UserFull::Full(user) = raw.full_user;
            Ok(user.about.unwrap_or_default())
        }
        PeerKind::Channel => {
            let raw: tl::types::messages::ChatFull = client
                .invoke(&tl::functions::channels::GetFullChannel {
                    channel: peer.into(),
                })
                .await?
                .into();
            Ok(raw.full_chat.about())
        }
        PeerKind::Chat => {
            let raw: tl::types::messages::ChatFull = client
                .invoke(&tl::functions::messages::GetFullChat {
                    chat_id: peer.id.bare_id().context("chat id is unavailable")?,
                })
                .await?
                .into();
            Ok(raw.full_chat.about())
        }
    }
}

pub(super) fn placeholder(window: &mut Window, app: &App, rect: Rect) {
    let glyph = if app.nerd_fonts { "\u{f03e}" } else { "!" };
    let width = if app.nerd_fonts { 2 } else { 1 };
    put(
        window,
        rect.y + rect.height / 2,
        rect.x + (rect.width - width).max(0) / 2,
        glyph,
        CellStyle::foreground(app.theme.palette.muted),
        rect.width.min(width),
    );
}

fn hit(window: &Window, x: i32, y: i32, width: i32, height: i32, target: HitTarget) -> HitBox {
    HitBox {
        rect: Rect {
            x: window.x + 1 + x,
            y: window.y + 1 + y,
            width,
            height,
        },
        target,
    }
}

pub(super) fn draw_pin_bar(window: &mut Window, app: &App, hitboxes: &mut Vec<HitBox>) -> bool {
    let Some(message) = app.pins.messages.get(app.pins.cursor) else {
        return false;
    };
    let width = window.width - 4;
    let summary = message_summary(message);
    put(
        window,
        3,
        2,
        &format!("Pinned · {summary}"),
        CellStyle::foreground(app.theme.palette.accent),
        width - 9,
    );
    put(
        window,
        3,
        width - 6,
        "[list]",
        CellStyle::foreground(app.theme.palette.accent),
        6,
    );
    hitboxes.push(hit(
        window,
        2,
        3,
        (width - 10).max(1),
        1,
        HitTarget::PinnedNext,
    ));
    hitboxes.push(hit(window, (width - 6).max(0), 3, 6, 1, HitTarget::Pins));
    true
}

fn message_summary(message: &Message) -> String {
    if !message.text().trim().is_empty() {
        return message.text().replace(['\n', '\r'], " ");
    }
    match message.media() {
        Some(Media::Document(document)) => document.name().unwrap_or("File").to_owned(),
        Some(Media::Photo(_)) => "Photo".to_owned(),
        Some(Media::Sticker(_)) => "Sticker".to_owned(),
        _ => "Message".to_owned(),
    }
}

pub(super) fn draw_details(
    window: &mut Window,
    app: &App,
    hitboxes: &mut Vec<HitBox>,
    placements: &mut Vec<ImagePlacement>,
) {
    let Some(panel) = &app.details else { return };
    let width = (window.width - 4).max(1);
    let text = CellStyle::foreground(app.theme.palette.text);
    let muted = CellStyle::foreground(app.theme.palette.muted);
    let accent = CellStyle::foreground(app.theme.palette.accent);
    put(
        window,
        0,
        1,
        if panel.tab == SharedTab::Pinned {
            app.settings.label(
                "lng_settings_events_pinned",
                "Pinned messages",
                "Закреплённые сообщения",
            )
        } else {
            app.settings.ui("Profile")
        },
        text,
        width - 4,
    );
    put(window, 0, width - 1, "×", accent, 2);
    hitboxes.push(hit(window, width - 2, 0, 3, 1, HitTarget::CloseDetails));
    let compact = panel.tab == SharedTab::Pinned || window.height < 23;
    let tabs_y = if compact { 3 } else { 12 };
    let user_profile = app
        .discussion
        .as_ref()
        .map(|v| &v.peer_object)
        .or_else(|| app.current_chat().map(|c| &c.peer_object))
        .is_some_and(|peer| matches!(peer, Peer::User(_)));
    put(window, 1, 1, app.view_name(), text, width);
    if !compact {
        let (columns, rows) = media::square_cells(12, 5);
        let x = 1 + (width - columns as i32) / 2;
        hitboxes.push(hit(
            window,
            x,
            2,
            columns as i32,
            rows as i32,
            HitTarget::AvatarGallery,
        ));
        if let Some(path) = &panel.avatar {
            placements.push(ImagePlacement {
                message_id: -90_000_000,
                asset: ImageAsset::Persistent(path.clone()),
                row: (window.y + 3).max(0) as u16,
                column: (window.x + 1 + x).max(0) as u16,
                rows,
                columns,
                crop: false,
            });
        } else if !app.tgp_supported {
            placeholder(
                window,
                app,
                Rect {
                    x,
                    y: 2,
                    width: columns as i32,
                    height: rows as i32,
                },
            );
        }
        put(window, 7, 1, app.view_presence(), muted, width);
        let object = app
            .discussion
            .as_ref()
            .map(|v| &v.peer_object)
            .or_else(|| app.current_chat().map(|c| &c.peer_object));
        if let Some(object) = object {
            if let Some(username) = object.username() {
                let tag = format!("@{username}");
                put(window, 8, 1, &tag, accent, width);
                hitboxes.push(hit(
                    window,
                    1,
                    8,
                    display_width(&tag) as i32,
                    1,
                    HitTarget::CopyUsername(tag),
                ));
            }
            if let Peer::User(user) = object
                && let Some(phone) = user.phone()
            {
                put(window, 9, 1, &format!("+{phone}"), muted, width);
            }
        }
        for (row, line) in wrap_text(&panel.about, width as usize)
            .iter()
            .take(2)
            .enumerate()
        {
            put(window, 10 + row as i32, 1, line, text, width);
        }
    }
    if user_profile && panel.tab != SharedTab::Pinned {
        let choices = [
            ProfileTab::Gifts,
            ProfileTab::Posts,
            ProfileTab::Attachments,
        ];
        let tab_width = (width / 3).max(1);
        for (index, section) in choices.into_iter().enumerate() {
            let x = 1 + index as i32 * tab_width;
            let label = match section {
                ProfileTab::Gifts => app.settings.ui("Gifts"),
                ProfileTab::Posts => app.settings.ui("Posts"),
                ProfileTab::Attachments => app.settings.label("", "Attachments", "Вложения"),
            };
            put(
                window,
                tabs_y,
                x,
                label,
                if panel.section == section {
                    accent
                } else {
                    muted
                },
                tab_width,
            );
            hitboxes.push(hit(
                window,
                x,
                tabs_y,
                tab_width,
                1,
                HitTarget::ProfileTab(section),
            ));
        }
        match panel.section {
            ProfileTab::Gifts => {
                gifts::draw_embedded(window, app, hitboxes, placements, tabs_y + 2);
                return;
            }
            ProfileTab::Posts => {
                let tile_width = ((width - 2) / 3).max(1);
                for (index, post) in panel.posts.iter().enumerate().skip(panel.scroll * 3) {
                    let y = tabs_y + 2 + ((index / 3 - panel.scroll) * 6) as i32;
                    if y + 5 >= window.height - 3 {
                        break;
                    }
                    let x = 1 + index as i32 % 3 * (tile_width + 1);
                    if let Some(PreviewState::Ready(asset)) = panel.post_previews.get(&post.id) {
                        placements.push(ImagePlacement {
                            message_id: -1_820_000_000
                                - (post.id.unsigned_abs() % 100_000_000) as i32,
                            asset: asset.clone(),
                            row: (window.y + 1 + y).max(0) as u16,
                            column: (window.x + 1 + x).max(0) as u16,
                            rows: 4,
                            columns: tile_width as u32,
                            crop: true,
                        });
                    } else if !app.tgp_supported {
                        placeholder(
                            window,
                            app,
                            Rect {
                                x,
                                y,
                                width: tile_width,
                                height: 4,
                            },
                        );
                    }
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;
                    let days = now.saturating_sub(i64::from(post.date)) / 86_400;
                    let date = match days {
                        0 => app.settings.ui("Today").to_owned(),
                        1 => app.settings.ui("Yesterday").to_owned(),
                        days => format!("{days} days ago"),
                    };
                    put(window, y + 4, x, &date, muted, tile_width);
                    if !post.caption.is_empty() {
                        put(window, y + 5, x, &post.caption, text, tile_width);
                    }
                    hitboxes.push(hit(
                        window,
                        x,
                        y,
                        tile_width,
                        6,
                        HitTarget::ProfilePost(post.id),
                    ));
                }
                if panel.posts.is_empty() {
                    put(
                        window,
                        tabs_y + 2,
                        1,
                        if panel.post_loading {
                            app.settings
                                .label("", "Loading publications…", "Загрузка публикаций…")
                        } else {
                            app.settings.ui("No publications")
                        },
                        muted,
                        width,
                    );
                }
                if panel.post_has_more {
                    let y = window.height - 4;
                    put(
                        window,
                        y,
                        1,
                        if panel.post_loading {
                            app.settings
                                .label("lng_profile_loading", "Loading…", "Загрузка…")
                        } else {
                            app.settings.ui("Load more")
                        },
                        accent,
                        width,
                    );
                    hitboxes.push(hit(window, 1, y, width, 1, HitTarget::ProfilePostsMore));
                }
                return;
            }
            ProfileTab::Attachments => {}
        }
    }
    let tabs = [
        SharedTab::Media,
        SharedTab::Files,
        SharedTab::Music,
        SharedTab::Voice,
        SharedTab::Links,
        SharedTab::Pinned,
    ];
    let tab_width = (width / 3).max(1);
    for (index, tab) in tabs.into_iter().enumerate() {
        let x = 1 + index as i32 % 3 * tab_width;
        let y = tabs_y
            + i32::from(user_profile && panel.tab != SharedTab::Pinned) * 2
            + index as i32 / 3;
        put(
            window,
            y,
            x,
            app.settings.ui(tab.label()),
            if panel.tab == tab { accent } else { muted },
            tab_width,
        );
        hitboxes.push(hit(window, x, y, tab_width, 1, HitTarget::SharedTab(tab)));
    }
    let top = tabs_y + i32::from(user_profile && panel.tab != SharedTab::Pinned) * 2 + 3;
    let bottom = window.height - 4;
    if panel.tab == SharedTab::Media {
        let tile_width = ((width - 2) / 3).max(1);
        for (index, message) in panel.messages.iter().enumerate().skip(panel.scroll * 3) {
            let y = top + ((index / 3 - panel.scroll) * 5) as i32;
            if y + 4 > bottom {
                break;
            }
            let x = 1 + index as i32 % 3 * (tile_width + 1);
            if let Some(PreviewState::Ready(asset)) = panel.previews.get(&message.id()) {
                placements.push(ImagePlacement {
                    message_id: -100_000_000 - (message.id().unsigned_abs() % 400_000_000) as i32,
                    asset: asset.clone(),
                    row: (window.y + 1 + y).max(0) as u16,
                    column: (window.x + 1 + x).max(0) as u16,
                    rows: 4,
                    columns: tile_width as u32,
                    crop: true,
                });
            } else if !app.tgp_supported {
                placeholder(
                    window,
                    app,
                    Rect {
                        x,
                        y,
                        width: tile_width,
                        height: 4,
                    },
                );
            }
            hitboxes.push(hit(
                window,
                x,
                y,
                tile_width,
                4,
                HitTarget::SharedMessage(index),
            ));
        }
    } else {
        for (row, (index, message)) in panel
            .messages
            .iter()
            .enumerate()
            .skip(panel.scroll)
            .enumerate()
        {
            let y = top + row as i32 * 2;
            if y + 1 >= bottom {
                break;
            }
            put(window, y, 1, &message_summary(message), text, width);
            put(
                window,
                y + 1,
                1,
                &message.date().strftime("%d.%m.%Y %H:%M").to_string(),
                muted,
                width,
            );
            hitboxes.push(hit(window, 1, y, width, 2, HitTarget::SharedMessage(index)));
            if media::is_audio(message) || media::is_circle(message) {
                let snapshot = app.audio_snapshot(message.id());
                let duration = snapshot.duration.max(media::audio_duration(message));
                let total = format_clock(duration);
                let elapsed = format_clock(snapshot.position);
                let button = if snapshot.loading {
                    "[...]"
                } else if snapshot.active && !snapshot.paused {
                    "[||]"
                } else {
                    "[>]"
                };
                let bar_width = (width - 21).max(3) as usize;
                let played = (snapshot.position / duration.max(1.0) * bar_width as f64) as usize;
                let bar: String = (0..bar_width)
                    .map(|i| if i < played { '━' } else { '─' })
                    .collect();
                put(
                    window,
                    y + 1,
                    1,
                    &format!("{button} {elapsed} {bar} {total}"),
                    accent,
                    width - 5,
                );
                let timeline_x = window.x + 2 + button.len() as i32 + 1 + elapsed.len() as i32 + 1;
                hitboxes.push(hit(
                    window,
                    1,
                    y + 1,
                    width - 5,
                    1,
                    HitTarget::SharedAudio {
                        index,
                        timeline_x,
                        timeline_width: bar_width as i32,
                    },
                ));
                if let Some((id, playback)) = &app.inline_video
                    && *id == message.id()
                    && let Some(image) = &playback.frame
                {
                    let (columns, rows) = media::square_cells(4, 2);
                    placements.push(ImagePlacement {
                        message_id: -100_000_000
                            - (message.id().unsigned_abs() % 400_000_000) as i32,
                        asset: ImageAsset::Video(image.clone(), playback.serial),
                        row: (window.y + 1 + y).max(0) as u16,
                        column: (window.x + width - 3).max(0) as u16,
                        rows,
                        columns,
                        crop: false,
                    });
                }
            }
        }
    }
    if panel.messages.is_empty() {
        put(
            window,
            top,
            1,
            if panel.loading {
                app.settings.ui("Loading...")
            } else {
                app.settings.ui("No attachments")
            },
            muted,
            width,
        );
    }
    if panel.has_more || panel.loading {
        put(
            window,
            bottom,
            1,
            if panel.loading {
                app.settings.ui("Loading...")
            } else {
                app.settings.ui("Load more")
            },
            accent,
            width,
        );
        hitboxes.push(hit(window, 1, bottom, width, 1, HitTarget::SharedMore));
    }
    if app.saving.is_some() {
        put(
            window,
            window.height - 3,
            1,
            &format!("Save to: {}█", app.input),
            accent,
            width,
        );
    }
}

pub(super) async fn open_shared(
    index: usize,
    app: &mut App,
    terminal: &mut Terminal,
    windows: &mut WindowManager,
) -> Result<()> {
    let Some(panel) = &app.details else {
        return Ok(());
    };
    let Some(message) = panel.messages.get(index).cloned() else {
        return Ok(());
    };
    if panel.tab == SharedTab::Pinned {
        app.jump_to_message(message.id());
        return Ok(());
    }
    if let Some((_, _, source)) = theme_links(&message).first() {
        app.request_theme_preview(source.clone());
        return Ok(());
    }
    if media::is_audio(&message) || media::is_circle(&message) {
        app.toggle_audio_message(&message);
        return Ok(());
    }
    if media::is_sticker(&message) {
        app.open_sticker_pack(&message);
        return Ok(());
    }
    if media::is_gif(&message) {
        app.jump_to_message(message.id());
        return Ok(());
    }
    if panel.tab == SharedTab::Files {
        let name = match message.media() {
            Some(Media::Document(doc)) => doc.name().unwrap_or("attachment").to_owned(),
            _ => "attachment".to_owned(),
        };
        let name = std::path::Path::new(&name)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("attachment");
        app.input = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(name)
            .to_string_lossy()
            .into_owned();
        app.saving = Some(message);
        return Ok(());
    }
    if message.media().is_none() {
        app.jump_to_message(message.id());
        return Ok(());
    }
    let messages = panel.messages.clone();
    let preview = match panel.previews.get(&message.id()) {
        Some(PreviewState::Ready(ImageAsset::Memory(image))) => Some(image.clone()),
        _ => None,
    };
    let peer = app.current_peer().context("no chat selected")?;
    app.stop_audio();
    let cache = app.view_cache();
    let mut backdrop = if app.tgp_supported {
        Some(media::viewer_backdrop(terminal)?)
    } else {
        None
    };
    let viewer = media::ViewerContext {
        terminal,
        windows,
        tgp_supported: app.tgp_supported,
        nerd_fonts: app.nerd_fonts,
    };
    let result = if media::opens_in_gallery(&message) {
        media::open_gallery(
            &app.telegram.client,
            peer,
            &messages,
            index,
            &cache,
            preview,
            viewer,
        )
        .await
        .map(|_| "Media viewer closed".to_owned())
    } else {
        media::open(&app.telegram.client, &message, &cache, preview, viewer).await
    };
    if let Some(backdrop) = &mut backdrop {
        backdrop.delete(&mut terminal.stdout)?;
    }
    media::clear_viewer_layers(terminal)?;
    match result {
        Ok(status) => {
            app.status = status;
            app.status_error = false;
        }
        Err(error) => app.set_error(error),
    }
    Ok(())
}

/// UTF-16 ranges, matching Telegram entities (including hidden text links).
fn theme_links(message: &Message) -> Vec<(usize, usize, String)> {
    theme_link_ranges(
        message.text(),
        message.fmt_entities().map_or(&[], Vec::as_slice),
    )
}

fn theme_link_ranges(
    text: &str,
    entities: &[tl::enums::MessageEntity],
) -> Vec<(usize, usize, String)> {
    let mut links = Vec::new();
    for entity in entities {
        if let tl::enums::MessageEntity::TextUrl(entity) = entity
            && theme::is_theme_link(&entity.url)
        {
            links.push((
                entity.offset.max(0) as usize,
                entity.length.max(0) as usize,
                entity.url.clone(),
            ));
        }
    }
    for prefix in theme::LINK_PREFIXES {
        for (start, _) in text.match_indices(prefix) {
            let source: String = text[start..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || ".:/?=&_-%#".contains(*c))
                .collect();
            if !theme::is_theme_link(&source) {
                continue;
            }
            let start = text[..start].encode_utf16().count();
            let length = source.encode_utf16().count();
            if !links
                .iter()
                .any(|(offset, span, _)| *offset <= start && start < offset + span)
            {
                links.push((start, length, source));
            }
        }
    }
    links
}

fn mention_link_ranges(
    text: &str,
    entities: &[tl::enums::MessageEntity],
) -> Vec<(usize, usize, String)> {
    let mut links = Vec::new();
    for entity in entities {
        if let tl::enums::MessageEntity::Mention(mention) = entity
            && let Some(source) =
                crate::custom_emoji::slice_utf16(text, mention.offset, mention.length)
            && let Some(name) = source.strip_prefix('@')
            && !name.is_empty()
            && name.len() <= 32
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            links.push((
                mention.offset as usize,
                mention.length as usize,
                name.to_owned(),
            ));
        }
    }
    // Plain @usernames may arrive without an entity in imported messages.
    for (start, _) in text.match_indices('@') {
        let name: String = text[start + 1..]
            .chars()
            .take(32)
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty()
            || text[..start]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '@')
        {
            continue;
        }
        let offset = text[..start].encode_utf16().count();
        let length = 1 + name.len();
        if !links
            .iter()
            .any(|(at, span, _)| *at <= offset && offset < at + span)
        {
            links.push((offset, length, name));
        }
    }
    links
}

pub(super) fn draw_text_links(
    window: &Window,
    message: &Message,
    lines: &[String],
    origin: (i32, i32),
    width: i32,
    hitboxes: &mut Vec<HitBox>,
    positions: Option<&HashMap<usize, code::Position>>,
) {
    let links: Vec<_> = theme_links(message)
        .into_iter()
        .map(|(offset, length, source)| (offset, length, HitTarget::PreviewTheme(source)))
        .chain(
            mention_link_ranges(
                message.text(),
                message.fmt_entities().map_or(&[], Vec::as_slice),
            )
            .into_iter()
            .map(|(offset, length, name)| (offset, length, HitTarget::Mention(name))),
        )
        .collect();
    if links.is_empty() {
        return;
    }
    if let Some(positions) = positions {
        for (offset, position) in positions {
            if position.code || position.row >= lines.len() {
                continue;
            }
            if let Some((_, _, target)) = links.iter().find(|(start, length, _)| {
                *offset < start + length && offset + position.length > *start
            }) {
                hitboxes.push(hit(
                    window,
                    origin.1 + position.column as i32,
                    origin.0 + position.row as i32,
                    position.cells as i32,
                    1,
                    target.clone(),
                ));
            }
        }
        return;
    }
    let original = message.text();
    let leading = original[..original.len() - original.trim_start().len()]
        .encode_utf16()
        .count();
    let mut offset = leading;
    let mut row = 0usize;
    let mut column = 0usize;
    for grapheme in UnicodeSegmentation::graphemes(original.trim(), true) {
        let length = grapheme.encode_utf16().count();
        if grapheme == "\n" {
            row += 1;
            column = 0;
            offset += length;
            continue;
        }
        let cells = display_width(grapheme).max(1);
        if column + cells > width.max(1) as usize && column > 0 {
            row += 1;
            column = 0;
            if grapheme.chars().all(char::is_whitespace) {
                offset += length;
                continue;
            }
        }
        if row >= lines.len() {
            break;
        }
        if let Some((_, _, target)) = links
            .iter()
            .find(|(start, len, _)| offset < start + len && offset + length > *start)
        {
            hitboxes.push(hit(
                window,
                origin.1 + column as i32,
                origin.0 + row as i32,
                cells as i32,
                1,
                target.clone(),
            ));
        }
        column += cells;
        offset += length;
    }
}

pub(super) fn draw_theme_preview(window: &mut Window, app: &App, hitboxes: &mut Vec<HitBox>) {
    let Some(theme) = &app.theme_preview else {
        return;
    };
    let palette = theme.palette;
    // Preview owns a separate pane; the current theme and saved settings remain untouched.
    window.border_style = theme.style(palette.border, palette.window);
    for y in 0..window.height.saturating_sub(2) {
        for x in 0..window.width.saturating_sub(2) {
            window.posstyled(y, x, ' ', theme.style(palette.text, palette.chat));
        }
    }
    let width = window.width.saturating_sub(6);
    put(
        window,
        1,
        2,
        &format!(
            "{} · {}",
            app.settings.label("", "Theme preview", "Предпросмотр темы"),
            theme.name
        ),
        theme.style(palette.text, palette.window),
        width,
    );
    put(
        window,
        3,
        2,
        "Telegram",
        theme.style(palette.accent, palette.chat),
        width,
    );
    put(
        window,
        4,
        2,
        app.settings.label("lng_status_online", "online", "в сети"),
        theme.style(palette.muted, palette.chat),
        width,
    );
    put(
        window,
        7,
        3,
        app.settings
            .label("", "A message from your friend", "Сообщение от друга"),
        theme.style(palette.incoming, palette.chat),
        width - 2,
    );
    put(
        window,
        9,
        5,
        app.settings.label("", "Your reply", "Ваш ответ"),
        theme.style(palette.outgoing, palette.chat),
        width - 4,
    );
    let apply = format!(
        "[ {} ]",
        app.settings.label("", "Apply theme", "Применить тему")
    );
    let cancel = format!("[ {} ]", app.settings.ui("Cancel"));
    let cancel_x = 2 + display_width(&apply) as i32 + 3;
    put(
        window,
        12,
        2,
        &apply,
        theme.style(palette.accent, palette.chat),
        width,
    );
    put(
        window,
        12,
        cancel_x,
        &cancel,
        theme.style(palette.accent, palette.chat),
        (width - cancel_x).max(0),
    );
    put(
        window,
        14,
        2,
        app.settings.label(
            "",
            "Enter apply · Esc cancel",
            "Enter — применить · Esc — отмена",
        ),
        theme.style(palette.muted, palette.chat),
        width,
    );
    hitboxes.push(hit(
        window,
        2,
        12,
        (display_width(&apply) as i32).min(width),
        1,
        HitTarget::ApplyTheme,
    ));
    hitboxes.push(hit(
        window,
        cancel_x,
        12,
        (display_width(&cancel) as i32).min((width - cancel_x).max(0)),
        1,
        HitTarget::CancelTheme,
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_links_keep_utf16_ranges_and_distinct_hidden_and_visible_links() {
        let text = "😀 тема https://t.me/addtheme/Night_1";
        let entities = vec![
            tl::types::MessageEntityTextUrl {
                offset: 3,
                length: 4,
                url: "tg://addtheme?slug=Hidden".to_owned(),
            }
            .into(),
        ];
        let links = theme_link_ranges(text, &entities);
        assert_eq!(
            links.len(),
            2,
            "nested t.me prefix must not duplicate a link"
        );
        assert_eq!(links[0], (3, 4, "tg://addtheme?slug=Hidden".to_owned()));
        assert_eq!(links[1].0, "😀 тема ".encode_utf16().count());
        assert_eq!(links[1].2, "https://t.me/addtheme/Night_1");
        assert!(theme_link_ranges("https://example.org/addtheme/foo", &[]).is_empty());
    }

    #[test]
    fn mention_links_use_utf16_ranges_and_ignore_email_addresses() {
        let text = "😀 hi @alice and mail@example.org";
        let offset = "😀 hi ".encode_utf16().count() as i32;
        let entities = vec![tl::types::MessageEntityMention { offset, length: 6 }.into()];
        assert_eq!(
            mention_link_ranges(text, &entities),
            vec![(offset as usize, 6, "alice".to_owned())]
        );
        assert!(mention_link_ranges("mail@example.org", &[]).is_empty());
        assert_eq!(
            mention_link_ranges("hey @bob", &[]),
            vec![(4, 4, "bob".to_owned())]
        );
    }
}
