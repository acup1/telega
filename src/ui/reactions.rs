use super::*;
use grammers_client::Client;
use grammers_client::message::InputReactions;
use grammers_session::types::PeerKind;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) enum Key {
    Emoji(String),
    Custom(i64),
}

impl Key {
    fn from_raw(raw: &tl::enums::Reaction) -> Option<Self> {
        match raw {
            tl::enums::Reaction::Emoji(e) => Some(Self::Emoji(e.emoticon.clone())),
            tl::enums::Reaction::CustomEmoji(e) => Some(Self::Custom(e.document_id)),
            _ => None,
        }
    }
    pub(super) fn raw(&self) -> tl::enums::Reaction {
        match self {
            Self::Emoji(emoticon) => tl::types::ReactionEmoji {
                emoticon: emoticon.clone(),
            }
            .into(),
            Self::Custom(document_id) => tl::types::ReactionCustomEmoji {
                document_id: *document_id,
            }
            .into(),
        }
    }
}

pub(super) struct Picker {
    message_id: i32,
    scroll: usize,
    pack: Option<usize>,
}
struct Pack {
    name: String,
    id: i64,
    hash: i64,
    choices: Option<Vec<Key>>,
    loading: bool,
}
pub(super) struct Catalog {
    choices: Vec<Key>,
    packs: Vec<Pack>,
    max_user: usize,
}
enum Visual {
    Loading,
    Ready(Option<media::StickerFrames>, String),
    Failed,
}

#[derive(Default)]
pub(super) struct State {
    pub picker: Option<Picker>,
    catalog: Option<Catalog>,
    loading: bool,
    visuals: HashMap<i64, Visual>,
    pub(super) busy: HashSet<i32>,
}

pub(super) enum Event {
    Catalog {
        view_id: u64,
        result: std::result::Result<Catalog, String>,
    },
    Pack {
        view_id: u64,
        index: usize,
        result: std::result::Result<Vec<Key>, String>,
    },
    Visual {
        view_id: u64,
        id: i64,
        result: std::result::Result<(Option<media::StickerFrames>, String), String>,
    },
    Sent {
        view_id: u64,
        message_id: i32,
        selected: Vec<Key>,
        result: std::result::Result<Option<Box<Message>>, String>,
    },
}

fn allows(policy: &tl::enums::ChatReactions, key: &Key) -> bool {
    match policy {
        tl::enums::ChatReactions::None => false,
        tl::enums::ChatReactions::All(all) => all.allow_custom || matches!(key, Key::Emoji(_)),
        tl::enums::ChatReactions::Some(some) => some
            .reactions
            .iter()
            .any(|r| Key::from_raw(r).as_ref() == Some(key)),
    }
}

async fn policy(client: &Client, peer: PeerRef) -> Result<tl::enums::ChatReactions> {
    let full = match peer.id.kind() {
        PeerKind::User => return Ok(tl::types::ChatReactionsAll { allow_custom: true }.into()),
        PeerKind::Channel => {
            let full: tl::types::messages::ChatFull = client
                .invoke(&tl::functions::channels::GetFullChannel {
                    channel: peer.into(),
                })
                .await?
                .into();
            full.full_chat
        }
        PeerKind::Chat => {
            let full: tl::types::messages::ChatFull = client
                .invoke(&tl::functions::messages::GetFullChat {
                    chat_id: peer.id.bare_id().context("missing chat id")?,
                })
                .await?
                .into();
            full.full_chat
        }
    };
    Ok(match full {
        tl::enums::ChatFull::Full(full) => full.available_reactions,
        tl::enums::ChatFull::ChannelFull(full) => full.available_reactions,
        _ => None,
    }
    .unwrap_or(tl::enums::ChatReactions::None))
}

async fn catalog(client: &Client, peer: PeerRef) -> Result<Catalog> {
    let (available, allowed, me) = tokio::try_join!(
        async {
            Ok::<_, anyhow::Error>(
                client
                    .invoke(&tl::functions::messages::GetAvailableReactions { hash: 0 })
                    .await?,
            )
        },
        policy(client, peer),
        async { Ok::<_, anyhow::Error>(client.get_me().await?) }
    )?;
    let premium = matches!(me.raw, tl::enums::User::User(user) if user.premium);
    let mut choices = Vec::new();
    if let Ok(tl::enums::messages::Reactions::Reactions(recent)) = client
        .invoke(&tl::functions::messages::GetRecentReactions {
            limit: 100,
            hash: 0,
        })
        .await
    {
        choices.extend(recent.reactions.iter().filter_map(Key::from_raw));
    }
    if let Ok(tl::enums::messages::Reactions::Reactions(top)) = client
        .invoke(&tl::functions::messages::GetTopReactions {
            limit: 100,
            hash: 0,
        })
        .await
    {
        choices.extend(top.reactions.iter().filter_map(Key::from_raw));
    }
    if let tl::enums::messages::AvailableReactions::Reactions(available) = available {
        for reaction in available.reactions {
            let tl::enums::AvailableReaction::Reaction(reaction) = reaction;
            if !reaction.inactive {
                choices.push(Key::Emoji(reaction.reaction));
            }
        }
    }
    if let tl::enums::ChatReactions::Some(some) = &allowed {
        choices.extend(some.reactions.iter().filter_map(Key::from_raw));
    }
    let mut unique = HashSet::new();
    choices.retain(|key| allows(&allowed, key) && unique.insert(key.clone()));
    let mut packs = Vec::new();
    if matches!(&allowed, tl::enums::ChatReactions::All(all) if all.allow_custom)
        && let Ok(tl::enums::messages::AllStickers::Stickers(installed)) = client
            .invoke(&tl::functions::messages::GetEmojiStickers { hash: 0 })
            .await
    {
        for set in installed.sets {
            let tl::enums::StickerSet::Set(set) = set;
            packs.push(Pack {
                name: set.title,
                id: set.id,
                hash: set.access_hash,
                choices: None,
                loading: false,
            });
        }
    }
    let mut max_user = 1;
    if matches!(&allowed,tl::enums::ChatReactions::All(all) if all.allow_custom)
        && let Ok(tl::enums::messages::FeaturedStickers::Stickers(featured)) = client
            .invoke(&tl::functions::messages::GetFeaturedEmojiStickers { hash: 0 })
            .await
    {
        for covered in featured.sets {
            let tl::enums::StickerSet::Set(set) = covered.set();
            if !packs.iter().any(|pack| pack.id == set.id) {
                packs.push(Pack {
                    name: set.title,
                    id: set.id,
                    hash: set.access_hash,
                    choices: None,
                    loading: false,
                });
            }
        }
    }
    if let Ok(tl::enums::help::AppConfig::Config(config)) = client
        .invoke(&tl::functions::help::GetAppConfig { hash: 0 })
        .await
        && let tl::enums::Jsonvalue::JsonObject(object) = config.config
    {
        let name = if premium {
            "reactions_user_max_premium"
        } else {
            "reactions_user_max_default"
        };
        for value in object.value {
            let tl::enums::JsonobjectValue::JsonObjectValue(value) = value;
            if value.key == name
                && let tl::enums::Jsonvalue::JsonNumber(number) = value.value
            {
                max_user = (number.value as usize).clamp(1, 100);
            }
        }
    }
    Ok(Catalog {
        choices,
        packs,
        max_user,
    })
}

fn counts(message: &Message) -> &[tl::enums::ReactionCount] {
    match &message.raw {
        tl::enums::Message::Message(raw) => match &raw.reactions {
            Some(tl::enums::MessageReactions::Reactions(reactions)) => &reactions.results,
            _ => &[],
        },
        _ => &[],
    }
}

pub(super) fn selection(message: &Message) -> Vec<Key> {
    let mut selected: Vec<_> = counts(message)
        .iter()
        .filter_map(|r| {
            let tl::enums::ReactionCount::Count(r) = r;
            Some((r.chosen_order?, Key::from_raw(&r.reaction)?))
        })
        .collect();
    selected.sort_by_key(|(order, _)| *order);
    selected.into_iter().map(|(_, key)| key).collect()
}

fn toggled(mut current: Vec<Key>, clicked: Option<Key>, maximum: usize) -> Vec<Key> {
    let Some(clicked) = clicked else {
        return Vec::new();
    };
    if let Some(index) = current.iter().position(|key| *key == clicked) {
        current.remove(index);
    } else {
        current.push(clicked);
        let remove = current.len().saturating_sub(maximum.max(1));
        current.drain(..remove);
    }
    current
}

pub(super) fn apply_selection(message: &mut Message, selected: &[Key]) {
    let tl::enums::Message::Message(raw) = &mut message.raw else {
        return;
    };
    let reactions = raw.reactions.get_or_insert_with(|| {
        tl::types::MessageReactions {
            min: false,
            can_see_list: true,
            reactions_as_tags: false,
            results: Vec::new(),
            recent_reactions: None,
            top_reactors: None,
        }
        .into()
    });
    let tl::enums::MessageReactions::Reactions(reactions) = reactions;
    for reaction in &mut reactions.results {
        let tl::enums::ReactionCount::Count(reaction) = reaction;
        if !matches!(reaction.reaction, tl::enums::Reaction::Paid)
            && reaction.chosen_order.take().is_some()
        {
            reaction.count = reaction.count.saturating_sub(1);
        }
    }
    for (order, key) in selected.iter().enumerate() {
        if let Some(reaction) = reactions.results.iter_mut().find(|r| {
            let tl::enums::ReactionCount::Count(r) = r;
            Key::from_raw(&r.reaction).as_ref() == Some(key)
        }) {
            let tl::enums::ReactionCount::Count(reaction) = reaction;
            reaction.count += 1;
            reaction.chosen_order = Some(order as i32);
        } else {
            reactions.results.push(
                tl::types::ReactionCount {
                    reaction: key.raw(),
                    count: 1,
                    chosen_order: Some(order as i32),
                }
                .into(),
            );
        }
    }
    reactions.results.retain(|r| {
        let tl::enums::ReactionCount::Count(r) = r;
        r.count > 0
    });
}

impl App {
    pub(super) fn request_reaction_catalog(&mut self) {
        if self.reaction_state.loading || self.reaction_state.catalog.is_some() {
            return;
        }
        let Some(peer) = self.current_peer() else {
            return;
        };
        self.reaction_state.loading = true;
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        let view_id = self.view_id;
        tokio::spawn(async move {
            let result = catalog(&client, peer).await.map_err(|e| e.to_string());
            let _ = tx.send(UiEvent::Reactions(Event::Catalog { view_id, result }));
        });
    }

    pub(super) fn choose_reaction(&mut self, message_id: i32, clicked: Option<Key>) {
        let Some(index) = self.message_index_by_id(message_id) else {
            return;
        };
        let existing = compat::own(
            &self.compatible_reactions,
            self.messages[index].peer_id(),
            message_id,
        );
        // Legacy reply-carriers may be removed/migrated, never created again.
        // Account restrictions are checked by sendReaction, not by this UI.
        if !existing.is_empty() {
            let mut current = selection(&self.messages[index]);
            for key in existing {
                if !current.contains(&key) {
                    current.push(key);
                }
            }
            let maximum = self
                .reaction_state
                .catalog
                .as_ref()
                .map_or(1, |c| c.max_user);
            self.update_legacy_reactions(index, toggled(current, clicked, maximum));
            return;
        }
        if !self.reaction_state.busy.insert(message_id) {
            return;
        }
        let message = self.messages[index].clone();
        let maximum = self
            .reaction_state
            .catalog
            .as_ref()
            .map_or(1, |c| c.max_user);
        let selected = toggled(selection(&message), clicked, maximum);
        let tx = self.events_tx.clone();
        let view_id = self.view_id;
        self.message_menu = None;
        self.reaction_state.picker = None;
        let client = self.telegram.client.clone();
        let peer = self.current_peer();
        tokio::spawn(async move {
            let reactions: Vec<_> = selected.iter().map(Key::raw).collect();
            let result = match message
                .react(InputReactions::from(reactions).add_to_recent())
                .await
            {
                Ok(()) => {
                    let fresh = if let Some(peer) = peer {
                        client
                            .get_messages_by_id(peer, &[message_id])
                            .await
                            .ok()
                            .and_then(|mut v| v.pop().flatten())
                            .map(Box::new)
                    } else {
                        None
                    };
                    Ok(fresh)
                }
                Err(error) => Err(error.to_string()),
            };
            let _ = tx.send(UiEvent::Reactions(Event::Sent {
                view_id,
                message_id,
                selected,
                result,
            }));
        });
    }

    pub(super) fn open_reaction_picker(&mut self, message_id: i32) {
        self.message_menu = None;
        self.reaction_state.picker = Some(Picker {
            message_id,
            scroll: 0,
            pack: None,
        });
        self.request_reaction_catalog();
    }

    pub(super) fn reaction_error(&mut self, error: String) {
        let premium_required = error.contains("PREMIUM_ACCOUNT_REQUIRED");
        self.set_error(error);
        if premium_required {
            self.status = "Telegram requires Premium for this reaction · no message sent".into();
        }
    }

    fn reaction_pack(&mut self, index: Option<usize>) {
        if let Some(picker) = &mut self.reaction_state.picker {
            picker.pack = index;
            picker.scroll = 0;
        }
        let Some(index) = index else { return };
        let Some(pack) = self
            .reaction_state
            .catalog
            .as_mut()
            .and_then(|c| c.packs.get_mut(index))
        else {
            return;
        };
        if pack.loading || pack.choices.is_some() {
            return;
        }
        pack.loading = true;
        let set = tl::types::InputStickerSetId {
            id: pack.id,
            access_hash: pack.hash,
        }
        .into();
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        let view_id = self.view_id;
        tokio::spawn(async move {
            let result = async {
                let result = client
                    .invoke(&tl::functions::messages::GetStickerSet {
                        stickerset: set,
                        hash: 0,
                    })
                    .await?;
                let tl::enums::messages::StickerSet::Set(set) = result else {
                    return Ok(Vec::new());
                };
                Ok::<_, anyhow::Error>(
                    set.documents
                        .iter()
                        .filter_map(|d| match d {
                            tl::enums::Document::Document(d) => Some(Key::Custom(d.id)),
                            _ => None,
                        })
                        .collect(),
                )
            }
            .await
            .map_err(|e| e.to_string());
            let _ = tx.send(UiEvent::Reactions(Event::Pack {
                view_id,
                index,
                result,
            }));
        });
    }

    pub(super) fn handle_reaction_picker_input(&mut self, key: i32, terminal: &Terminal) -> bool {
        if self.reaction_state.picker.is_none() {
            return false;
        }
        if key == MOUSE_EVENT {
            let mouse = terminal.getmouse();
            if mouse.event == MouseEvent::LEFT
                && matches!(self.hit_test(mouse.x, mouse.y), Some(HitTarget::Composer))
            {
                self.reaction_state.picker = None;
                return false;
            }
        }
        match key {
            27 => self.reaction_state.picker = None,
            KEY_UP => self.scroll_reactions(false),
            KEY_DOWN => self.scroll_reactions(true),
            MOUSE_EVENT => {
                let mouse = terminal.getmouse();
                match mouse.event {
                    MouseEvent::WHEELUP => self.scroll_reactions(false),
                    MouseEvent::WHEELDOWN => self.scroll_reactions(true),
                    MouseEvent::LEFT => match self.hit_test(mouse.x, mouse.y) {
                        Some(HitTarget::Reaction {
                            message_id,
                            reaction,
                        }) => self.choose_reaction(message_id, Some(reaction)),
                        Some(HitTarget::ReactionPack(index)) => self.reaction_pack(index),
                        Some(HitTarget::RemoveReaction(message_id)) => {
                            self.choose_reaction(message_id, None)
                        }
                        Some(HitTarget::CloseReactions) => self.reaction_state.picker = None,
                        _ => {}
                    },
                    _ => {}
                }
            }
            _ => {}
        }
        true
    }

    fn scroll_reactions(&mut self, down: bool) {
        if let Some(picker) = &mut self.reaction_state.picker {
            let count = self.reaction_state.catalog.as_ref().map_or(0, |c| {
                picker
                    .pack
                    .and_then(|i| c.packs.get(i))
                    .and_then(|p| p.choices.as_ref())
                    .map_or(c.choices.len(), Vec::len)
            });
            picker.scroll = if down {
                (picker.scroll + 1).min(count.saturating_sub(1))
            } else {
                picker.scroll.saturating_sub(1)
            };
        }
    }

    pub(super) fn queue_reaction_visuals(&mut self, hitboxes: &[HitBox]) {
        for id in hitboxes.iter().filter_map(|h| match &h.target {
            HitTarget::Reaction {
                reaction: Key::Custom(id),
                ..
            } => Some(*id),
            _ => None,
        }) {
            if self.reaction_state.visuals.contains_key(&id) {
                continue;
            }
            self.reaction_state.visuals.insert(id, Visual::Loading);
            let client = self.telegram.client.clone();
            let cache = self.view_cache();
            let tx = self.events_tx.clone();
            let view_id = self.view_id;
            let workers = self.preview_workers.clone();
            let graphics = self.tgp_supported;
            tokio::spawn(async move {
                let result = match workers.acquire_owned().await {
                    Ok(_permit) => media::prepare_reaction_emoji(client, id, cache, graphics)
                        .await
                        .map_err(|e| e.to_string()),
                    Err(_) => Err("preview worker stopped".to_owned()),
                };
                let _ = tx.send(UiEvent::Reactions(Event::Visual {
                    view_id,
                    id,
                    result,
                }));
            });
        }
    }

    pub(super) fn reactions_event(&mut self, event: Event) {
        match event {
            Event::Catalog { view_id, result } if view_id == self.view_id => {
                self.reaction_state.loading = false;
                match result {
                    Ok(catalog) => self.reaction_state.catalog = Some(catalog),
                    Err(error) => self.set_error(error),
                }
            }
            Event::Pack {
                view_id,
                index,
                result,
            } if view_id == self.view_id => {
                if let Some(pack) = self
                    .reaction_state
                    .catalog
                    .as_mut()
                    .and_then(|c| c.packs.get_mut(index))
                {
                    pack.loading = false;
                    match result {
                        Ok(choices) => pack.choices = Some(choices),
                        Err(error) => self.set_error(error),
                    }
                }
            }
            Event::Visual {
                view_id,
                id,
                result,
            } if view_id == self.view_id => match result {
                Ok((image, alt)) => {
                    self.reaction_state
                        .visuals
                        .insert(id, Visual::Ready(image, alt));
                }
                Err(error) => {
                    self.reaction_state.visuals.insert(id, Visual::Failed);
                    self.log_error(error);
                }
            },
            Event::Sent {
                view_id,
                message_id,
                selected,
                result,
            } if view_id == self.view_id => {
                self.reaction_state.busy.remove(&message_id);
                match result {
                    Ok(fresh) => {
                        if let Some(index) = self.message_index_by_id(message_id) {
                            if let Some(message) = fresh {
                                self.messages[index] = *message;
                            } else {
                                apply_selection(&mut self.messages[index], &selected);
                            }
                        }
                        self.status = if selected.is_empty() {
                            "Reaction removed"
                        } else {
                            "Reactions updated"
                        }
                        .to_owned();
                        self.status_error = false;
                    }
                    Err(error) => self.reaction_error(error),
                }
            }
            _ => {}
        }
    }
}

pub(super) fn quick_choices(app: &App) -> Vec<Key> {
    app.reaction_state
        .catalog
        .as_ref()
        .map(|c| c.choices.iter().take(6).cloned().collect())
        .unwrap_or_else(|| {
            ["👍", "❤", "🔥", "👏", "😁", "🤯"]
                .into_iter()
                .map(|s| Key::Emoji(s.to_owned()))
                .collect()
        })
}

fn draw_key(
    window: &mut Window,
    app: &App,
    key: &Key,
    position: (i32, i32),
    asset_id: i32,
    placements: &mut Vec<ImagePlacement>,
) {
    let (row, col) = position;
    let symbol = match key {
        Key::Emoji(emoji) => emoji.as_str(),
        Key::Custom(id) => match app.reaction_state.visuals.get(id) {
            Some(Visual::Ready(_, alt)) => alt.as_str(),
            _ => "?",
        },
    };
    let has_image = app.tgp_supported
        && matches!(key,Key::Custom(id) if matches!(app.reaction_state.visuals.get(id),Some(Visual::Ready(Some(_),_))));
    put(
        window,
        row,
        col,
        if has_image { "  " } else { symbol },
        CellStyle::foreground(app.theme.palette.text),
        2,
    );
    if app.tgp_supported
        && let Key::Custom(id) = key
        && let Some(Visual::Ready(Some(image), _)) = app.reaction_state.visuals.get(id)
    {
        placements.push(ImagePlacement {
            message_id: asset_id,
            asset: ImageAsset::Animated(image.clone()),
            row: (window.y + 1 + row).max(0) as u16,
            column: (window.x + 1 + col).max(0) as u16,
            rows: 1,
            columns: 2,
            crop: false,
        });
    }
}

pub(super) fn draw_quick(
    window: &mut Window,
    app: &App,
    origin: (i32, i32),
    message_id: i32,
    hitboxes: &mut Vec<HitBox>,
    placements: &mut Vec<ImagePlacement>,
) {
    for (slot, key) in quick_choices(app).into_iter().enumerate() {
        let x = origin.1 + slot as i32 * 4;
        draw_key(
            window,
            app,
            &key,
            (origin.0, x),
            -710_000_000 - slot as i32,
            placements,
        );
        hitboxes.push(HitBox {
            rect: Rect {
                x: window.x + 1 + x,
                y: window.y + 1 + origin.0,
                width: 2,
                height: 1,
            },
            target: HitTarget::Reaction {
                message_id,
                reaction: key,
            },
        });
    }
}

pub(super) fn draw_chips(
    window: &mut Window,
    app: &App,
    message: &Message,
    origin: (i32, i32),
    width: i32,
    hitboxes: &mut Vec<HitBox>,
    placements: &mut Vec<ImagePlacement>,
) {
    let mut x = origin.1;
    for (slot, reaction) in display_counts(app, message).iter().enumerate() {
        let tl::enums::ReactionCount::Count(reaction) = reaction;
        let count = reaction.count.to_string();
        let cells = 3 + count.len() as i32 + i32::from(reaction.chosen_order.is_some());
        if x + cells > origin.1 + width {
            put(
                window,
                origin.0,
                (origin.1 + width - 2).max(origin.1),
                "+",
                CellStyle::foreground(app.theme.palette.accent),
                1,
            );
            hitboxes.push(HitBox {
                rect: Rect {
                    x: window.x + 1 + (origin.1 + width - 2).max(origin.1),
                    y: window.y + 1 + origin.0,
                    width: 2,
                    height: 1,
                },
                target: HitTarget::ReactionPicker(message.id()),
            });
            break;
        }
        let start = x;
        if reaction.chosen_order.is_some() {
            put(
                window,
                origin.0,
                x,
                "·",
                CellStyle::foreground(app.theme.palette.accent),
                1,
            );
            x += 1;
        }
        if let Some(key) = Key::from_raw(&reaction.reaction) {
            draw_key(
                window,
                app,
                &key,
                (origin.0, x),
                -600_000_000
                    - ((message.id().unsigned_abs() % 1_000_000) * 64 + slot as u32) as i32,
                placements,
            );
            hitboxes.push(HitBox {
                rect: Rect {
                    x: window.x + 1 + start,
                    y: window.y + 1 + origin.0,
                    width: cells,
                    height: 1,
                },
                target: HitTarget::Reaction {
                    message_id: message.id(),
                    reaction: key,
                },
            });
        } else if matches!(reaction.reaction, tl::enums::Reaction::Paid) {
            put(
                window,
                origin.0,
                x,
                "⭐",
                CellStyle::foreground(app.theme.palette.accent),
                2,
            );
        }
        x += 3;
        put(
            window,
            origin.0,
            x,
            &count,
            CellStyle::foreground(app.theme.palette.accent),
            count.len() as i32,
        );
        x += count.len() as i32 + 2;
    }
}

pub(super) fn display_counts(app: &App, message: &Message) -> Vec<tl::enums::ReactionCount> {
    compat::counts(&app.compatible_reactions, message, counts(message).to_vec())
}

pub(super) fn draw_picker(
    window: &mut Window,
    app: &App,
    hitboxes: &mut Vec<HitBox>,
    placements: &mut Vec<ImagePlacement>,
) {
    let Some(picker) = &app.reaction_state.picker else {
        return;
    };
    let width = (window.width - 6).clamp(12, 54);
    let height = (window.height - 8).clamp(5, 19);
    let x = (window.width - width - 2).max(0) / 2;
    let y = 3;
    let rect = Rect {
        x: window.x + 1 + x,
        y: window.y + 1 + y,
        width,
        height,
    };
    placements.retain(|p| {
        i32::from(p.column) >= rect.x + width
            || i32::from(p.column) + p.columns as i32 <= rect.x
            || i32::from(p.row) >= rect.y + height
            || i32::from(p.row) + p.rows as i32 <= rect.y
    });
    for row in y..y + height {
        for col in x..x + width {
            window.posstyled(row, col, ' ', window.border_style);
        }
    }
    let accent = CellStyle::foreground(app.theme.palette.accent);
    let catalog = app.reaction_state.catalog.as_ref();
    let name = picker
        .pack
        .and_then(|i| catalog?.packs.get(i))
        .map_or("All reactions", |p| p.name.as_str());
    put(window, y, x + 1, "<", accent, 1);
    put(
        window,
        y,
        x + 3,
        if picker.pack.is_some() {
            name
        } else {
            app.settings.ui(name)
        },
        accent,
        width - 10,
    );
    put(window, y, x + width - 6, ">", accent, 1);
    put(window, y, x + width - 2, "×", accent, 2);
    hitboxes.push(HitBox {
        rect: Rect {
            x: rect.x + width - 3,
            y: rect.y,
            width: 3,
            height: 1,
        },
        target: HitTarget::CloseReactions,
    });
    let packs = catalog.map_or(0, |c| c.packs.len());
    let previous = picker.pack.and_then(|i| i.checked_sub(1));
    let next = if packs == 0 {
        None
    } else {
        match picker.pack {
            None => Some(0),
            Some(i) if i + 1 < packs => Some(i + 1),
            _ => None,
        }
    };
    hitboxes.push(HitBox {
        rect: Rect {
            x: rect.x,
            y: rect.y,
            width: 2,
            height: 1,
        },
        target: HitTarget::ReactionPack(previous),
    });
    hitboxes.push(HitBox {
        rect: Rect {
            x: rect.x + width - 6,
            y: rect.y,
            width: 2,
            height: 1,
        },
        target: HitTarget::ReactionPack(next),
    });
    let mut choices = if let Some(index) = picker.pack {
        catalog
            .and_then(|c| c.packs.get(index))
            .and_then(|p| p.choices.clone())
            .unwrap_or_default()
    } else {
        catalog.map_or_else(Vec::new, |c| c.choices.clone())
    };
    if picker.pack.is_none()
        && let Some(message) = app.messages.iter().find(|m| m.id() == picker.message_id)
    {
        for key in counts(message).iter().filter_map(|r| {
            let tl::enums::ReactionCount::Count(r) = r;
            Key::from_raw(&r.reaction)
        }) {
            if !choices.contains(&key) {
                choices.push(key);
            }
        }
    }
    let columns = ((width - 2) / 4).max(1) as usize;
    for (slot, key) in choices
        .into_iter()
        .skip(picker.scroll)
        .take(((height - 3) / 2).max(1) as usize * columns)
        .enumerate()
    {
        let row = y + 2 + slot as i32 / columns as i32 * 2;
        let col = x + 1 + slot as i32 % columns as i32 * 4;
        draw_key(
            window,
            app,
            &key,
            (row, col),
            -700_000_000 - slot as i32,
            placements,
        );
        hitboxes.push(HitBox {
            rect: Rect {
                x: window.x + 1 + col,
                y: window.y + 1 + row,
                width: 3,
                height: 1,
            },
            target: HitTarget::Reaction {
                message_id: picker.message_id,
                reaction: key,
            },
        });
    }
    if catalog.is_none() {
        put(
            window,
            y + 2,
            x + 1,
            app.settings
                .label("", "Loading reactions...", "Загрузка реакций..."),
            accent,
            width - 2,
        );
    }
    put(
        window,
        y + height - 1,
        x + 1,
        app.settings.ui("Remove my reactions"),
        accent,
        width - 2,
    );
    hitboxes.push(HitBox {
        rect: Rect {
            x: rect.x,
            y: rect.y + height - 1,
            width,
            height: 1,
        },
        target: HitTarget::RemoveReaction(picker.message_id),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn toggles_custom_reactions_and_keeps_newest_within_server_limit() {
        let a = Key::Emoji("👍".to_owned());
        let b = Key::Custom(42);
        let c = Key::Custom(43);
        // A custom choice remains a native document-ID reaction, even when
        // the account's maximum is the non-Premium default of one.
        let native = toggled(Vec::new(), Some(c.clone()), 1);
        assert!(
            matches!(native[0].raw(), tl::enums::Reaction::CustomEmoji(r) if r.document_id == 43)
        );
        assert_eq!(
            toggled(vec![a.clone(), b.clone()], Some(b.clone()), 3),
            vec![a.clone()]
        );
        assert_eq!(
            toggled(vec![a.clone(), b.clone()], Some(c.clone()), 2),
            vec![b, c.clone()]
        );
        assert!(toggled(vec![a], None, 1).is_empty());
        assert!(!allows(
            &tl::types::ChatReactionsAll {
                allow_custom: false
            }
            .into(),
            &c
        ));
        assert!(allows(
            &tl::types::ChatReactionsSome {
                reactions: vec![c.raw()]
            }
            .into(),
            &c
        ));
    }
}
