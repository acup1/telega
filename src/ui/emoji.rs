use std::cell::Cell;

use grammers_client::{Client, message::InputMessage};

use super::*;

// No private-use font glyphs in this list: ordinary emoji remain usable without Nerd Fonts/TGP.
const EMOJI: &str = "😀 😃 😄 😁 😆 😅 😂 🤣 😊 😇 🙂 🙃 😉 😌 😍 🥰 😘 😋 😛 😜 🤪 🤨 🧐 🤓 😎 🤩 🥳 😏 😒 😔 😞 😟 😕 🙁 😣 😖 😫 😩 🥺 😢 😭 😤 😠 😡 🤬 🤯 😳 🥵 🥶 😱 😨 😰 😥 😓 🤗 🤔 🫡 🤭 🫢 🫣 🤫 🤥 😶 😐 😑 🙄 😬 😮 😲 🥱 😴 🤤 😪 😵 🤐 🥴 🤢 🤮 🤧 😷 🤒 🤕 🤑 🤠 😈 👿 👻 💀 👽 🤖 🎃 😺 😸 😹 😻 😼 😽 🙀 😿 😾 ❤️ 🧡 💛 💚 💙 💜 🖤 🤍 🤎 💔 ❤️‍🔥 💕 💞 💓 💗 💖 💘 💝 💟 👍 👎 👊 ✊ 🤛 🤜 👏 🙌 👐 🤲 🤝 🙏 ✍️ 💪 🦾 🖕 👌 🤌 🤏 ✌️ 🤞 🤟 🤘 🤙 👈 👉 👆 👇 ☝️ ✋ 🤚 🖐️ 🖖 👋 🤙 🫶 💅 👀 🧠 🫀 🐶 🐱 🐭 🐹 🐰 🦊 🐻 🐼 🐨 🐯 🦁 🐮 🐷 🐸 🐵 🐔 🐧 🐦 🦆 🦉 🦋 🐝 🐞 🌸 🌹 🌺 🌻 🌼 🌷 🌱 🌲 🌴 🌵 🍀 🍁 🍂 🌍 🌞 🌝 🌚 ⭐ 🌟 ✨ ⚡ 🔥 🌈 ☀️ 🌤️ ☁️ 🌧️ ❄️ ☃️ 💧 🌊 🍏 🍎 🍐 🍊 🍋 🍌 🍉 🍇 🍓 🫐 🍒 🍑 🥭 🍍 🥥 🥝 🍅 🥑 🥦 🥕 🌽 🥔 🍞 🥐 🧀 🍔 🍟 🍕 🌮 🍣 🍜 🍰 🎂 🍫 🍿 ☕ 🍵 🍺 🍻 🥂 🍷 ⚽ 🏀 🎾 🎮 🎯 🎲 🎸 🎹 🎧 🎤 🎬 🎨 🚗 🚕 🚌 🚎 🚲 🏍️ ✈️ 🚀 🛸 🏠 🏢 🏔️ 🏖️ 🏝️ 🎉 🎊 🎈 🎁 🎀 🏆 🥇 💯 ✅ ❌ ❗ ❓ 💬 💡 🔑 🔒 🔔 📎 📌 📝 📚 💻 📱 ⌚ 📷 📹 💸 💰 💎 🛠️ ⚙️ 🧲 🧪 🧬 🩺 💊 🧸 🪄 🪩 🪅 🪷 🪻 🪼 🪿 🫎 🫏 🫚 🫛 🫧 🫠 🫤 🫨 🫥 🫦 🫰 🫵 🫱 🫲 🫳 🫴 🫷 🫸";

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum Tab {
    #[default]
    Emoji,
    Stickers,
    Custom,
}

impl Tab {
    fn next(self) -> Self {
        match self {
            Self::Emoji => Self::Stickers,
            Self::Stickers => Self::Custom,
            Self::Custom => Self::Emoji,
        }
    }
}

#[derive(Default)]
pub(super) struct State {
    view_id: u64,
    picker: Option<Picker>,
    catalog: Option<Catalog>,
    loading: bool,
    visuals: HashMap<i64, Visual>,
    external_pack: Option<Pack>,
    pack_request: u64,
}

impl State {
    pub(super) fn is_open(&self) -> bool {
        self.picker.is_some()
    }
    pub(super) fn visual(&self, id: i64) -> Option<&media::StickerFrames> {
        match self.visuals.get(&id) {
            Some(Visual::Ready(frames)) => Some(frames),
            _ => None,
        }
    }
    pub fn reset(&mut self, view_id: u64) {
        if self.view_id != view_id {
            *self = Self {
                view_id,
                ..Self::default()
            };
        }
    }
    pub fn premium(&self) -> bool {
        self.catalog.as_ref().is_some_and(|c| c.premium)
    }
}

#[derive(Default)]
struct Picker {
    tab: Tab,
    pack: usize,
    selected: usize,
    scroll: usize,
    columns: Cell<usize>,
    capacity: Cell<usize>,
    external: bool,
}

pub(super) struct Catalog {
    premium: bool,
    packs: Vec<Pack>,
}
struct Pack {
    id: i64,
    hash: i64,
    title: String,
    tab: Tab,
    documents: Option<Vec<tl::enums::Document>>,
    loading: bool,
}
enum Visual {
    Loading,
    Ready(media::StickerFrames),
    Failed,
}

pub(super) enum Event {
    OpenedPack {
        view_id: u64,
        request: u64,
        result: std::result::Result<tl::types::messages::StickerSet, String>,
    },
    Catalog {
        view_id: u64,
        result: std::result::Result<Catalog, String>,
    },
    Pack {
        view_id: u64,
        index: usize,
        result: std::result::Result<Vec<tl::enums::Document>, String>,
    },
    Visual {
        view_id: u64,
        id: i64,
        result: std::result::Result<media::StickerFrames, String>,
    },
    Sent {
        view_id: u64,
        result: std::result::Result<Box<Message>, String>,
    },
}

fn metadata(document: &tl::enums::Document) -> Option<(i64, &str, bool)> {
    let tl::enums::Document::Document(document) = document else {
        return None;
    };
    document
        .attributes
        .iter()
        .find_map(|attribute| match attribute {
            tl::enums::DocumentAttribute::Sticker(sticker) => {
                Some((document.id, sticker.alt.as_str(), false))
            }
            tl::enums::DocumentAttribute::CustomEmoji(emoji) => {
                Some((document.id, emoji.alt.as_str(), emoji.free))
            }
            _ => None,
        })
}

async fn catalog(client: &Client) -> Result<Catalog> {
    let (me, stickers, custom, recent, featured) = tokio::join!(
        client.get_me(),
        client.invoke(&tl::functions::messages::GetAllStickers { hash: 0 }),
        client.invoke(&tl::functions::messages::GetEmojiStickers { hash: 0 }),
        client.invoke(&tl::functions::messages::GetRecentStickers {
            attached: false,
            hash: 0
        }),
        client.invoke(&tl::functions::messages::GetFeaturedEmojiStickers { hash: 0 }),
    );
    let premium = matches!(me?.raw, tl::enums::User::User(user) if user.premium);
    let mut packs = Vec::new();
    if let Ok(tl::enums::messages::RecentStickers::Stickers(recent)) = recent {
        packs.push(Pack {
            id: 0,
            hash: 0,
            title: "Recent".into(),
            tab: Tab::Stickers,
            documents: Some(recent.stickers),
            loading: false,
        });
    }
    for (tab, result) in [(Tab::Stickers, stickers), (Tab::Custom, custom)] {
        // Keep ordinary emoji and the other catalog usable if a single RPC fails.
        if let Ok(tl::enums::messages::AllStickers::Stickers(sets)) = result {
            for set in sets.sets {
                let tl::enums::StickerSet::Set(set) = set;
                packs.push(Pack {
                    id: set.id,
                    hash: set.access_hash,
                    title: set.title,
                    tab,
                    documents: None,
                    loading: false,
                });
            }
        }
    }
    if let Ok(tl::enums::messages::FeaturedStickers::Stickers(featured)) = featured {
        for covered in featured.sets {
            let set = covered.set();
            let tl::enums::StickerSet::Set(set) = set;
            if !packs.iter().any(|pack| pack.id == set.id) {
                packs.push(Pack {
                    id: set.id,
                    hash: set.access_hash,
                    title: set.title,
                    tab: Tab::Custom,
                    documents: None,
                    loading: false,
                });
            }
        }
    }
    Ok(Catalog { premium, packs })
}

fn pack_indices(state: &State, tab: Tab) -> Vec<usize> {
    state
        .catalog
        .as_ref()
        .map(|c| {
            c.packs
                .iter()
                .enumerate()
                .filter_map(|(i, p)| (p.tab == tab).then_some(i))
                .collect()
        })
        .unwrap_or_default()
}

fn active_pack(state: &State) -> Option<usize> {
    let picker = state.picker.as_ref()?;
    pack_indices(state, picker.tab).get(picker.pack).copied()
}

fn documents(state: &State) -> &[tl::enums::Document] {
    selected_pack(state)
        .and_then(|pack| pack.documents.as_deref())
        .unwrap_or_default()
}

fn selected_pack(state: &State) -> Option<&Pack> {
    if state.picker.as_ref()?.external {
        state.external_pack.as_ref()
    } else {
        active_pack(state).and_then(|i| state.catalog.as_ref()?.packs.get(i))
    }
}

fn choice_count(state: &State) -> usize {
    if state.picker.as_ref().is_some_and(|p| p.tab == Tab::Emoji) {
        EMOJI.split_whitespace().count()
    } else {
        documents(state).len()
    }
}

pub(super) async fn send_text(
    client: &Client,
    peer: PeerRef,
    text: &str,
    reply: Option<i32>,
    draft: &[crate::custom_emoji::Draft],
    premium: bool,
) -> Result<Message> {
    // TextUrl is accepted as ordinary formatted text, not a forged Premium entity.
    // Compatible clients render its document ID; other clients retain a readable emoji.
    Ok(client
        .send_message(
            peer,
            crate::message_format::message(text, draft, premium).reply_to(reply),
        )
        .await?)
}

impl App {
    pub(super) fn open_sticker_pack(&mut self, message: &Message) {
        let Some(set) = media::sticker_set(message) else {
            self.status = "This sticker has no available pack".into();
            return;
        };
        self.emoji_state.reset(self.view_id);
        self.emoji_state.pack_request = self.emoji_state.pack_request.wrapping_add(1);
        let request = self.emoji_state.pack_request;
        self.emoji_state.external_pack = Some(Pack {
            id: 0,
            hash: 0,
            title: "Loading sticker pack…".into(),
            tab: Tab::Stickers,
            documents: None,
            loading: true,
        });
        self.emoji_state.picker = Some(Picker {
            tab: Tab::Stickers,
            external: true,
            ..Picker::default()
        });
        self.message_menu = None;
        self.attachment_menu = false;
        self.reaction_state.picker = None;
        self.request_emoji_catalog();
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        let view_id = self.view_id;
        tokio::spawn(async move {
            let result = client
                .invoke(&tl::functions::messages::GetStickerSet {
                    stickerset: set,
                    hash: 0,
                })
                .await
                .map_err(|error| error.to_string())
                .and_then(|set| match set {
                    tl::enums::messages::StickerSet::Set(set) => Ok(set),
                    _ => Err("Sticker pack is unavailable".into()),
                });
            let _ = tx.send(UiEvent::Emoji(Event::OpenedPack {
                view_id,
                request,
                result,
            }));
        });
    }

    pub(super) fn close_emoji_picker(&mut self) {
        self.emoji_state.picker = None;
    }

    pub(super) fn toggle_emoji_picker(&mut self) {
        if self.open_chat.is_none() {
            return;
        }
        self.emoji_state.reset(self.view_id);
        if self.emoji_state.picker.take().is_some() {
            return;
        }
        self.emoji_state.picker = Some(Picker::default());
        self.attachment_menu = false;
        self.message_menu = None;
        self.reaction_state.picker = None;
        self.request_emoji_catalog();
    }

    fn request_emoji_catalog(&mut self) {
        if self.emoji_state.catalog.is_none() && !self.emoji_state.loading {
            self.emoji_state.loading = true;
            let client = self.telegram.client.clone();
            let tx = self.events_tx.clone();
            let view_id = self.view_id;
            tokio::spawn(async move {
                let result = catalog(&client).await.map_err(|e| e.to_string());
                let _ = tx.send(UiEvent::Emoji(Event::Catalog { view_id, result }));
            });
        }
    }

    fn emoji_tab(&mut self, tab: Tab) {
        if let Some(picker) = &mut self.emoji_state.picker {
            *picker = Picker {
                tab,
                ..Picker::default()
            };
        }
        self.request_emoji_pack();
    }

    fn request_emoji_pack(&mut self) {
        if self.emoji_state.picker.as_ref().is_some_and(|p| p.external) {
            return;
        }
        let Some(index) = active_pack(&self.emoji_state) else {
            return;
        };
        let pack = &mut self.emoji_state.catalog.as_mut().unwrap().packs[index];
        if pack.documents.is_some() || pack.loading {
            return;
        }
        pack.loading = true;
        let set = tl::types::InputStickerSetId {
            id: pack.id,
            access_hash: pack.hash,
        };
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        let view_id = self.view_id;
        tokio::spawn(async move {
            let result = async {
                let result = client
                    .invoke(&tl::functions::messages::GetStickerSet {
                        stickerset: set.into(),
                        hash: 0,
                    })
                    .await?;
                match result {
                    tl::enums::messages::StickerSet::Set(set) => {
                        Ok::<_, anyhow::Error>(set.documents)
                    }
                    _ => anyhow::bail!("sticker set not returned"),
                }
            }
            .await
            .map_err(|e| e.to_string());
            let _ = tx.send(UiEvent::Emoji(Event::Pack {
                view_id,
                index,
                result,
            }));
        });
    }

    fn move_emoji_pack(&mut self, direction: i32) {
        let Some(picker) = &self.emoji_state.picker else {
            return;
        };
        let count = pack_indices(&self.emoji_state, picker.tab).len();
        if count == 0 {
            return;
        }
        let picker = self.emoji_state.picker.as_mut().unwrap();
        picker.external = false;
        picker.pack = (picker.pack as i32 + direction).rem_euclid(count as i32) as usize;
        picker.scroll = 0;
        picker.selected = 0;
        self.request_emoji_pack();
    }

    fn pick_emoji(&mut self, index: usize) {
        if !self.require_write() {
            return;
        }
        let Some(picker) = &self.emoji_state.picker else {
            return;
        };
        let tab = picker.tab;
        if tab == Tab::Emoji {
            if let Some(emoji) = EMOJI.split_whitespace().nth(index) {
                self.insert_draft_text(emoji);
                self.composing = true;
            }
            return;
        }
        let Some(raw) = documents(&self.emoji_state).get(index).cloned() else {
            return;
        };
        let Some((id, alt, free)) = metadata(&raw) else {
            return;
        };
        if tab == Tab::Custom {
            let alt = if alt.is_empty() { "🙂" } else { alt };
            let offset = self.input[..self.composer.byte(&self.input)]
                .encode_utf16()
                .count() as i32;
            self.insert_draft_text(alt);
            self.draft_custom.push(crate::custom_emoji::Draft {
                offset,
                length: alt.encode_utf16().count() as i32,
                id,
                alt: alt.to_owned(),
                free,
            });
            self.composing = true;
        } else {
            let Some(peer) = self.current_peer() else {
                return;
            };
            let client = self.telegram.client.clone();
            let tx = self.events_tx.clone();
            let view_id = self.view_id;
            let reply = self.reply_root();
            self.reply_to = None;
            self.status = "Sending sticker…".into();
            tokio::spawn(async move {
                let document = media::sticker_document(raw);
                let result = client
                    .send_message(
                        peer,
                        InputMessage::new()
                            .media(document.to_raw_input_media())
                            .reply_to(reply),
                    )
                    .await
                    .map(Box::new)
                    .map_err(|e| e.to_string());
                let _ = tx.send(UiEvent::Emoji(Event::Sent { view_id, result }));
            });
        }
    }

    pub(super) fn handle_emoji_input(&mut self, key: i32, terminal: &Terminal) -> bool {
        if self.emoji_state.picker.is_none() {
            if (key == 5
                && !self.theme_input
                && !self.attaching
                && !self.searching
                && self.saving.is_none())
                || (key == MOUSE_EVENT && {
                    let mouse = terminal.getmouse();
                    mouse.event == MouseEvent::LEFT
                        && matches!(
                            self.hit_test(mouse.x, mouse.y),
                            Some(HitTarget::EmojiToggle)
                        )
                })
            {
                self.toggle_emoji_picker();
                return true;
            }
            return false;
        }
        if key == 27
            || key == 6
            || matches!(key, 101 | 105)
            || (self.composing && key != MOUSE_EVENT && key != 5)
        {
            return false;
        }
        if key == MOUSE_EVENT
            && !matches!(
                self.hit_test(terminal.getmouse().x, terminal.getmouse().y),
                Some(
                    HitTarget::EmojiPanel
                        | HitTarget::EmojiTab(_)
                        | HitTarget::EmojiPack(_)
                        | HitTarget::EmojiPick(_)
                        | HitTarget::EmojiClose
                )
            )
        {
            return false;
        }
        match key {
            5 => {}
            9 => {
                let tab = self.emoji_state.picker.as_ref().unwrap().tab.next();
                self.emoji_tab(tab);
            }
            10 | 13 => self.pick_emoji(self.emoji_state.picker.as_ref().unwrap().selected),
            KEY_LEFT | KEY_RIGHT | KEY_UP | KEY_DOWN => {
                let count = choice_count(&self.emoji_state);
                let picker = self.emoji_state.picker.as_mut().unwrap();
                let columns = picker.columns.get().max(1);
                let step = if matches!(key, KEY_UP | KEY_DOWN) {
                    columns
                } else {
                    1
                };
                picker.selected = if matches!(key, KEY_LEFT | KEY_UP) {
                    picker.selected.saturating_sub(step)
                } else {
                    (picker.selected + step).min(count.saturating_sub(1))
                };
                reveal(picker);
            }
            MOUSE_EVENT => {
                let mouse = terminal.getmouse();
                if mouse.event == MouseEvent::LEFT {
                    match self.hit_test(mouse.x, mouse.y) {
                        Some(HitTarget::EmojiTab(tab)) => self.emoji_tab(tab),
                        Some(HitTarget::EmojiPack(direction)) => self.move_emoji_pack(direction),
                        Some(HitTarget::EmojiPick(index)) => self.pick_emoji(index),
                        Some(HitTarget::EmojiPanel) => {}
                        Some(HitTarget::EmojiClose) => self.close_emoji_picker(),
                        _ => {}
                    }
                } else if matches!(mouse.event, MouseEvent::WHEELUP | MouseEvent::WHEELDOWN) {
                    let count = choice_count(&self.emoji_state);
                    let picker = self.emoji_state.picker.as_mut().unwrap();
                    let columns = picker.columns.get().max(1);
                    let last_row = count.saturating_sub(1) / columns * columns;
                    picker.scroll = if mouse.event == MouseEvent::WHEELUP {
                        picker.scroll.saturating_sub(columns)
                    } else {
                        (picker.scroll + columns).min(last_row)
                    };
                    picker.selected = picker.scroll;
                }
            }
            _ => return false,
        }
        true
    }

    pub(super) fn queue_draft_visuals(&mut self) {
        if !self.tgp_supported {
            return;
        }
        for id in self
            .draft_custom
            .iter()
            .map(|e| e.id)
            .collect::<HashSet<_>>()
        {
            if self.emoji_state.visuals.contains_key(&id) {
                continue;
            }
            self.emoji_state.visuals.insert(id, Visual::Loading);
            let client = self.telegram.client.clone();
            let cache = self.view_cache();
            let tx = self.events_tx.clone();
            let view_id = self.view_id;
            let workers = self.preview_workers.clone();
            tokio::spawn(async move {
                let result = match workers.acquire_owned().await {
                    Ok(_permit) => media::prepare_reaction_emoji(client, id, cache, true)
                        .await
                        .and_then(|(frames, _)| frames.context("custom emoji has no image"))
                        .map_err(|e| e.to_string()),
                    Err(_) => Err("preview worker stopped".into()),
                };
                let _ = tx.send(UiEvent::Emoji(Event::Visual {
                    view_id,
                    id,
                    result,
                }));
            });
        }
    }

    pub(super) fn queue_emoji_visuals(&mut self, hitboxes: &[HitBox]) {
        if !self.tgp_supported
            || self
                .emoji_state
                .picker
                .as_ref()
                .is_none_or(|p| p.tab == Tab::Emoji)
        {
            return;
        }
        let visible: Vec<_> = hitboxes
            .iter()
            .filter_map(|hit| match hit.target {
                HitTarget::EmojiPick(index) => documents(&self.emoji_state).get(index).cloned(),
                _ => None,
            })
            .collect();
        for raw in visible {
            let Some((id, _, _)) = metadata(&raw) else {
                continue;
            };
            if self.emoji_state.visuals.contains_key(&id) {
                continue;
            }
            // A bounded, per-chat RAM cache; never keep full packs on disk.
            if self.emoji_state.visuals.len() >= 96 {
                self.emoji_state
                    .visuals
                    .retain(|_, v| matches!(v, Visual::Loading));
            }
            self.emoji_state.visuals.insert(id, Visual::Loading);
            let client = self.telegram.client.clone();
            let tx = self.events_tx.clone();
            let cache = self.view_cache();
            let view_id = self.view_id;
            let workers = self.preview_workers.clone();
            tokio::spawn(async move {
                let result = match workers.acquire_owned().await {
                    Ok(_permit) => media::prepare_picker_sticker_progressive(client, raw, cache, {
                        let tx = tx.clone();
                        move |image| {
                            let _ = tx.send(UiEvent::Emoji(Event::Visual {
                                view_id,
                                id,
                                result: Ok(media::StickerFrames::still(image)),
                            }));
                        }
                    })
                    .await
                    .map_err(|e| e.to_string()),
                    Err(_) => Err("sticker worker stopped".into()),
                };
                let _ = tx.send(UiEvent::Emoji(Event::Visual {
                    view_id,
                    id,
                    result,
                }));
            });
        }
    }

    pub(super) fn emoji_event(&mut self, event: Event) {
        match event {
            Event::OpenedPack {
                view_id,
                request,
                result,
            } if view_id == self.view_id && request == self.emoji_state.pack_request => {
                match result {
                    Ok(set) => {
                        let tl::enums::StickerSet::Set(meta) = set.set;
                        let tab = if meta.emojis {
                            Tab::Custom
                        } else {
                            Tab::Stickers
                        };
                        self.emoji_state.external_pack = Some(Pack {
                            id: meta.id,
                            hash: meta.access_hash,
                            title: meta.title,
                            tab,
                            documents: Some(set.documents),
                            loading: false,
                        });
                        if let Some(picker) = &mut self.emoji_state.picker
                            && picker.external
                        {
                            picker.tab = tab;
                        }
                    }
                    Err(error) => {
                        if let Some(pack) = &mut self.emoji_state.external_pack {
                            pack.loading = false;
                            pack.title = "Sticker pack unavailable".into();
                        }
                        self.set_error(error);
                    }
                }
            }
            Event::Catalog { view_id, result } if view_id == self.view_id => {
                self.emoji_state.loading = false;
                match result {
                    Ok(catalog) => {
                        self.emoji_state.catalog = Some(catalog);
                        self.request_emoji_pack();
                    }
                    Err(error) => self.set_error(error),
                }
            }
            Event::Pack {
                view_id,
                index,
                result,
            } if view_id == self.view_id => {
                if let Some(pack) = self
                    .emoji_state
                    .catalog
                    .as_mut()
                    .and_then(|c| c.packs.get_mut(index))
                {
                    pack.loading = false;
                    match result {
                        Ok(documents) => pack.documents = Some(documents),
                        Err(error) => self.set_error(error),
                    }
                }
            }
            Event::Visual {
                view_id,
                id,
                result,
            } if view_id == self.view_id => match result {
                Ok(frames) => {
                    self.emoji_state.visuals.insert(id, Visual::Ready(frames));
                }
                Err(error) => {
                    if !matches!(self.emoji_state.visuals.get(&id), Some(Visual::Ready(_))) {
                        self.emoji_state.visuals.insert(id, Visual::Failed);
                    }
                    self.log_error(error);
                }
            },
            Event::Sent { view_id, result } if view_id == self.view_id => match result {
                Ok(message) => {
                    self.messages.push(*message);
                    self.message_index = self.messages.len().saturating_sub(1);
                    self.scroll_from_bottom = 0;
                    self.status = "Sticker sent".into();
                    self.status_error = false;
                    if let Some(index) = self.open_chat {
                        self.queue_previews(index);
                    }
                }
                Err(error) => self.set_error(error),
            },
            _ => {}
        }
    }
}

fn reveal(picker: &mut Picker) {
    let columns = picker.columns.get().max(1);
    let capacity = picker.capacity.get().max(columns);
    if picker.selected < picker.scroll {
        picker.scroll = picker.selected / columns * columns;
    }
    if picker.selected >= picker.scroll + capacity {
        picker.scroll = (picker.selected / columns + 1) * columns - capacity;
    }
}

pub(super) fn draw_picker(
    window: &mut Window,
    app: &App,
    hitboxes: &mut Vec<HitBox>,
    placements: &mut Vec<ImagePlacement>,
) {
    draw_panel(
        window,
        PanelContext {
            theme: &app.theme,
            emoji_state: &app.emoji_state,
            settings: &app.settings,
        },
        hitboxes,
        placements,
    );
}

struct PanelContext<'a> {
    theme: &'a theme::Theme,
    emoji_state: &'a State,
    settings: &'a settings::State,
}

fn draw_panel(
    window: &mut Window,
    app: PanelContext<'_>,
    hitboxes: &mut Vec<HitBox>,
    placements: &mut Vec<ImagePlacement>,
) {
    let Some(picker) = &app.emoji_state.picker else {
        return;
    };
    if window.width < 12 || window.height < 14 {
        return;
    }
    let width = (window.width - 2).max(8);
    let height = (window.height - 2).max(5);
    let (x, y) = (0, 0);
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
    let background = CellStyle::new(Some(app.theme.palette.text), Some(app.theme.palette.window));
    for row in y..y + height {
        for col in x..x + width {
            window.posstyled(row, col, ' ', background);
        }
    }
    hitboxes.push(HitBox {
        rect,
        target: HitTarget::EmojiPanel,
    });
    let accent = CellStyle::new(
        Some(app.theme.palette.accent),
        Some(app.theme.palette.window),
    );
    let muted = CellStyle::new(
        Some(app.theme.palette.muted),
        Some(app.theme.palette.window),
    );
    let tab_width = ((width - 3) / 3).max(1);
    for (i, (tab, name)) in [
        (Tab::Emoji, "Emoji"),
        (Tab::Stickers, "Stickers"),
        (Tab::Custom, "Custom"),
    ]
    .into_iter()
    .enumerate()
    {
        let col = x + i as i32 * tab_width;
        put(
            window,
            y,
            col,
            app.settings.ui(name),
            if picker.tab == tab { accent } else { muted },
            tab_width,
        );
        hitboxes.push(HitBox {
            rect: Rect {
                x: rect.x + i as i32 * tab_width,
                y: rect.y,
                width: tab_width,
                height: 1,
            },
            target: HitTarget::EmojiTab(tab),
        });
    }
    put(window, y, x + width - 2, "×", accent, 2);
    hitboxes.push(HitBox {
        rect: Rect {
            x: rect.x + width - 2,
            y: rect.y,
            width: 2,
            height: 1,
        },
        target: HitTarget::EmojiClose,
    });
    let pack = selected_pack(app.emoji_state);
    let title = if picker.tab == Tab::Emoji {
        app.settings.label(
            "",
            "Emoji · Tab switches category",
            "Эмодзи · Tab меняет категорию",
        )
    } else {
        pack.map_or(
            if app.emoji_state.loading {
                app.settings
                    .label("", "Loading packs…", "Загрузка наборов…")
            } else {
                app.settings
                    .label("", "No installed packs", "Нет установленных наборов")
            },
            |p| p.title.as_str(),
        )
    };
    put(window, y + 2, x + 2, title, muted, width - 6);
    if picker.tab != Tab::Emoji {
        for (col, label, direction) in [(0, "‹", -1), (width - 2, "›", 1)] {
            put(window, y + 2, x + col, label, accent, 2);
            hitboxes.push(HitBox {
                rect: Rect {
                    x: rect.x + col,
                    y: rect.y + 2,
                    width: 2,
                    height: 1,
                },
                target: HitTarget::EmojiPack(direction),
            });
        }
    }
    let cell_width = if picker.tab == Tab::Emoji { 4 } else { 7 };
    let columns = ((width - 2) / cell_width).max(1) as usize;
    let capacity = ((height - 4) / 3).max(1) as usize * columns;
    picker.columns.set(columns);
    picker.capacity.set(capacity);
    let emojis: Vec<_> = EMOJI.split_whitespace().collect();
    let docs = documents(app.emoji_state);
    let count = choice_count(app.emoji_state);
    for (slot, index) in (picker.scroll..count).take(capacity).enumerate() {
        let row = y + 4 + (slot / columns) as i32 * 3;
        let col = x + 1 + (slot % columns) as i32 * cell_width;
        let selected = index == picker.selected;
        let symbol = if picker.tab == Tab::Emoji {
            emojis[index]
        } else {
            metadata(&docs[index]).map_or("!", |(_, alt, _)| if alt.is_empty() { "!" } else { alt })
        };
        put(
            window,
            row,
            col,
            if selected { "›" } else { " " },
            accent,
            1,
        );
        let frames = docs
            .get(index)
            .filter(|_| picker.tab != Tab::Emoji)
            .and_then(metadata)
            .and_then(|(id, _, _)| match app.emoji_state.visuals.get(&id) {
                Some(Visual::Ready(frames)) => Some(frames),
                _ => None,
            });
        if let Some(frames) = frames {
            let (pixel_width, pixel_height) = frames.current().dimensions();
            let (cols, rows) =
                media::fit_dimensions(pixel_width, pixel_height, (cell_width - 1) as u32, 2);
            placements.push(ImagePlacement {
                message_id: -1_600_000_000 - slot as i32,
                asset: ImageAsset::Animated(frames.clone()),
                row: (window.y + 1 + row).max(0) as u16,
                column: (window.x + 1 + col + 1).max(0) as u16,
                rows,
                columns: cols,
                crop: false,
            });
        } else {
            put(window, row, col + 1, symbol, background, cell_width - 1);
        }
        hitboxes.push(HitBox {
            rect: Rect {
                x: window.x + 1 + col,
                y: window.y + 1 + row,
                width: cell_width,
                height: 2,
            },
            target: HitTarget::EmojiPick(index),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn keyboard_selection_scrolls_by_rows_and_tabs_cycle() {
        let mut picker = Picker {
            selected: 29,
            columns: Cell::new(7),
            capacity: Cell::new(21),
            ..Picker::default()
        };
        reveal(&mut picker);
        assert_eq!(picker.scroll, 14);
        picker.selected = 2;
        reveal(&mut picker);
        assert_eq!(picker.scroll, 0);
        assert_eq!(Tab::Emoji.next().next().next(), Tab::Emoji);
        assert!(EMOJI.split_whitespace().count() > 300);
    }
    #[test]
    fn changing_chat_releases_picker_media_and_catalog() {
        let mut state = State {
            picker: Some(Picker::default()),
            loading: true,
            ..State::default()
        };
        state.reset(1);
        assert!(
            state.picker.is_none()
                && state.catalog.is_none()
                && state.visuals.is_empty()
                && !state.loading
        );
        assert!(!state.premium());
    }

    #[test]
    fn picker_grid_has_distinct_mouse_targets_and_opaque_cells() {
        let state = State {
            picker: Some(Picker::default()),
            ..State::default()
        };
        let theme = theme::Theme::default();
        let mut window = Window::new(0);
        window.x = 40;
        window.y = 2;
        window.width = 62;
        window.height = 30;
        let mut hits = Vec::new();
        let mut images = Vec::new();
        let settings = settings::State::default();
        draw_panel(
            &mut window,
            PanelContext {
                theme: &theme,
                emoji_state: &state,
                settings: &settings,
            },
            &mut hits,
            &mut images,
        );
        let panel = hits
            .iter()
            .find(|h| matches!(h.target, HitTarget::EmojiPanel))
            .unwrap()
            .rect;
        assert_eq!(
            hits.iter()
                .filter(|h| matches!(h.target, HitTarget::EmojiTab(_)))
                .count(),
            3
        );
        let choices: Vec<_> = hits
            .iter()
            .filter(|h| matches!(h.target, HitTarget::EmojiPick(_)))
            .collect();
        assert!(choices.len() >= 20);
        for (i, choice) in choices.iter().enumerate() {
            assert!(panel.contains(choice.rect.x, choice.rect.y));
            assert!(panel.contains(
                choice.rect.x + choice.rect.width - 1,
                choice.rect.y + choice.rect.height - 1
            ));
            assert!(
                choices
                    .iter()
                    .skip(i + 1)
                    .all(|other| !choice.rect.contains(other.rect.x, other.rect.y))
            );
        }
        assert!(
            window
                .cells()
                .all(|cell| cell.style.background == Some(theme.palette.window))
        );
        assert!(images.is_empty());
        assert_eq!(state.picker.as_ref().unwrap().capacity.get(), choices.len());
    }

    #[test]
    fn custom_packs_are_not_filtered_by_premium_and_preserve_document_metadata() {
        let document: tl::enums::Document = tl::types::Document {
            id: 123,
            access_hash: 456,
            file_reference: vec![1, 2],
            date: 0,
            mime_type: "application/x-tgsticker".into(),
            size: 42,
            thumbs: None,
            video_thumbs: None,
            dc_id: 2,
            attributes: vec![
                tl::types::DocumentAttributeCustomEmoji {
                    free: false,
                    text_color: false,
                    alt: "🎉".into(),
                    stickerset: tl::enums::InputStickerSet::Empty,
                }
                .into(),
            ],
        }
        .into();
        let state = State {
            picker: Some(Picker {
                tab: Tab::Custom,
                ..Picker::default()
            }),
            catalog: Some(Catalog {
                premium: false,
                packs: vec![Pack {
                    id: 99,
                    hash: 0,
                    title: "Custom pack".into(),
                    tab: Tab::Custom,
                    documents: Some(vec![document]),
                    loading: false,
                }],
            }),
            ..State::default()
        };
        assert_eq!(choice_count(&state), 1);
        assert_eq!(metadata(&documents(&state)[0]), Some((123, "🎉", false)));
        assert_eq!(
            media::sticker_document(documents(&state)[0].clone())
                .to_raw_input_media()
                .id,
            tl::types::InputDocument {
                id: 123,
                access_hash: 456,
                file_reference: vec![1, 2]
            }
            .into()
        );
    }
}
