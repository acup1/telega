use super::*;

#[derive(Clone)]
pub(super) struct Entry {
    pub id: i64,
    pub title: String,
    pub detail: String,
    pub document: Option<tl::enums::Document>,
}

pub(super) enum Visual {
    Loading,
    Ready(media::StickerFrames),
    Failed,
}

#[derive(Default)]
pub(super) struct State {
    pub popup: bool,
    pub peer: Option<PeerRef>,
    pub entries: Vec<Entry>,
    pub next_offset: Option<String>,
    pub loading: bool,
    pub selected: usize,
    pub visuals: HashMap<i64, Visual>,
}

pub(super) enum Event {
    Loaded {
        peer: PeerRef,
        result: std::result::Result<(Vec<Entry>, Option<String>), String>,
    },
    Visual {
        peer: PeerRef,
        id: i64,
        result: std::result::Result<media::StickerFrames, String>,
    },
}

impl App {
    pub(super) fn close_profile_gifts(&mut self) {
        self.gifts = State::default();
    }

    pub(super) fn load_gift_page(&mut self) {
        let Some(peer) = self.gifts.peer else { return };
        if self.gifts.loading
            || (!self.gifts.entries.is_empty() && self.gifts.next_offset.is_none())
        {
            return;
        }
        self.gifts.loading = true;
        let offset = self.gifts.next_offset.clone().unwrap_or_default();
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        tokio::spawn(async move {
            let result = async {
                let response: tl::types::payments::SavedStarGifts = client
                    .invoke(&tl::functions::payments::GetSavedStarGifts {
                        exclude_unsaved: false,
                        exclude_saved: false,
                        exclude_unlimited: false,
                        exclude_unique: false,
                        sort_by_value: false,
                        exclude_upgradable: false,
                        exclude_unupgradable: false,
                        peer_color_available: false,
                        exclude_hosted: false,
                        peer: peer.into(),
                        collection_id: None,
                        offset,
                        limit: 30,
                    })
                    .await?
                    .into();
                let entries = response.gifts.into_iter().map(gift_entry).collect();
                Ok((entries, response.next_offset))
            }
            .await
            .map_err(|error: grammers_client::InvocationError| error.to_string());
            let _ = tx.send(UiEvent::Gifts(Event::Loaded { peer, result }));
        });
    }

    pub(super) fn gift_event(&mut self, event: Event) {
        match event {
            Event::Loaded { peer, result } if self.gifts.peer == Some(peer) => {
                self.gifts.loading = false;
                match result {
                    Ok((entries, next_offset)) => {
                        let known: HashSet<_> =
                            self.gifts.entries.iter().map(|gift| gift.id).collect();
                        self.gifts
                            .entries
                            .extend(entries.into_iter().filter(|gift| !known.contains(&gift.id)));
                        self.gifts.next_offset = next_offset;
                    }
                    Err(error) => self.set_error(error),
                }
            }
            Event::Visual { peer, id, result } if self.gifts.peer == Some(peer) => {
                if let Err(error) = &result {
                    self.log_error(error);
                }
                self.gifts.visuals.insert(
                    id,
                    match result {
                        Ok(image) => Visual::Ready(image),
                        Err(_) => Visual::Failed,
                    },
                );
            }
            _ => {}
        }
    }

    pub(super) fn queue_gift_visuals(&mut self, hits: &[HitBox]) {
        if !(self.gifts.popup
            || self
                .details
                .as_ref()
                .is_some_and(|panel| panel.section == details::ProfileTab::Gifts))
            || !self.tgp_supported
        {
            return;
        }
        let Some(peer) = self.gifts.peer else { return };
        let visible: Vec<_> = hits
            .iter()
            .filter_map(|hit| {
                if let HitTarget::Gift(index) = hit.target {
                    self.gifts.entries.get(index).cloned()
                } else {
                    None
                }
            })
            .collect();
        for gift in visible {
            let Some(document) = gift.document else {
                continue;
            };
            if self.gifts.visuals.contains_key(&gift.id) {
                continue;
            }
            self.gifts.visuals.insert(gift.id, Visual::Loading);
            let client = self.telegram.client.clone();
            let cache = self.view_cache().join("gifts");
            let workers = self.sticker_workers.clone();
            let tx = self.events_tx.clone();
            tokio::spawn(async move {
                let result = match workers.acquire_owned().await {
                    Ok(_permit) => tokio::time::timeout(
                        Duration::from_secs(30),
                        media::prepare_picker_sticker(client, document, cache),
                    )
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("gift image timed out")))
                    .map_err(|error| error.to_string()),
                    Err(_) => Err("gift worker stopped".to_owned()),
                };
                let _ = tx.send(UiEvent::Gifts(Event::Visual {
                    peer,
                    id: gift.id,
                    result,
                }));
            });
        }
    }
}

fn gift_entry(saved: tl::enums::SavedStarGift) -> Entry {
    let tl::enums::SavedStarGift::Gift(saved) = saved;
    let entry_id = saved
        .saved_id
        .or_else(|| saved.msg_id.map(i64::from))
        .unwrap_or_else(|| saved.gift.id());
    match saved.gift {
        tl::enums::StarGift::Gift(gift) => Entry {
            id: entry_id,
            title: gift.title.unwrap_or_else(|| "Gift".to_owned()),
            detail: format!("{} Stars", gift.stars),
            document: Some(gift.sticker),
        },
        tl::enums::StarGift::Unique(gift) => Entry {
            id: entry_id,
            title: format!("{} #{}", gift.title, gift.num),
            detail: "Collectible gift".to_owned(),
            document: gift.attributes.into_iter().find_map(|attribute| {
                if let tl::enums::StarGiftAttribute::Model(model) = attribute {
                    Some(model.document)
                } else {
                    None
                }
            }),
        },
    }
}

pub(super) fn draw_embedded(
    window: &mut Window,
    app: &App,
    hits: &mut Vec<HitBox>,
    images: &mut Vec<ImagePlacement>,
    top: i32,
) {
    let width = window.width.saturating_sub(4);
    let accent = CellStyle::foreground(app.theme.palette.accent);
    let text = CellStyle::foreground(app.theme.palette.text);
    let muted = CellStyle::foreground(app.theme.palette.muted);
    let visible = ((window.height - top - 5) / 3).max(0) as usize;
    let start = app.gifts.selected.saturating_sub(visible.saturating_sub(1));
    if app.gifts.entries.is_empty() {
        put(
            window,
            top,
            1,
            if app.gifts.loading {
                app.settings
                    .label("", "Loading gifts…", "Загрузка подарков…")
            } else {
                app.settings.ui("No visible gifts")
            },
            muted,
            width,
        );
    }
    for (row, index) in (start..app.gifts.entries.len()).take(visible).enumerate() {
        let gift = &app.gifts.entries[index];
        let y = top + row as i32 * 3;
        if let Some(Visual::Ready(frames)) = app.gifts.visuals.get(&gift.id) {
            images.push(ImagePlacement {
                message_id: -1_760_000_000 - index as i32,
                asset: ImageAsset::Animated(frames.clone()),
                row: (window.y + 1 + y).max(0) as u16,
                column: (window.x + 2).max(0) as u16,
                rows: 2,
                columns: 4,
                crop: false,
            });
        } else if !app.tgp_supported {
            put(
                window,
                y,
                1,
                if app.nerd_fonts { "\u{f06b}" } else { "!" },
                accent,
                2,
            );
        }
        put(
            window,
            y,
            7,
            &gift.title,
            if index == app.gifts.selected {
                accent
            } else {
                text
            },
            width - 7,
        );
        put(window, y + 1, 7, &gift.detail, muted, width - 7);
        hits.push(HitBox {
            rect: Rect {
                x: window.x + 1,
                y: window.y + 1 + y,
                width,
                height: 2,
            },
            target: HitTarget::Gift(index),
        });
    }
    if app.gifts.next_offset.is_some() {
        let y = window.height - 3;
        put(
            window,
            y,
            1,
            if app.gifts.loading {
                app.settings
                    .label("lng_profile_loading", "Loading…", "Загрузка…")
            } else {
                app.settings.ui("Load more")
            },
            accent,
            width,
        );
        hits.push(HitBox {
            rect: Rect {
                x: window.x + 1,
                y: window.y + 1 + y,
                width,
                height: 1,
            },
            target: HitTarget::GiftMore,
        });
    }
}

pub(super) fn draw_popup(
    window: &mut Window,
    app: &App,
    hits: &mut Vec<HitBox>,
    images: &mut Vec<ImagePlacement>,
) {
    if !app.gifts.popup {
        return;
    }
    let width = window.width.saturating_sub(4);
    let accent = CellStyle::new(
        Some(app.theme.palette.accent),
        Some(app.theme.palette.window),
    );
    let text = CellStyle::new(Some(app.theme.palette.text), Some(app.theme.palette.window));
    let muted = CellStyle::new(
        Some(app.theme.palette.muted),
        Some(app.theme.palette.window),
    );
    put(window, 0, 2, app.settings.ui("Gifts"), accent, width);
    put(window, 0, window.width - 4, "×", accent, 2);
    hits.push(HitBox {
        rect: Rect {
            x: window.x,
            y: window.y,
            width: window.width,
            height: window.height,
        },
        target: HitTarget::GiftPanel,
    });
    hits.push(HitBox {
        rect: Rect {
            x: window.x + window.width - 4,
            y: window.y + 1,
            width: 3,
            height: 1,
        },
        target: HitTarget::GiftClose,
    });
    if app.gifts.entries.is_empty() {
        put(
            window,
            2,
            2,
            if app.gifts.loading {
                app.settings
                    .label("", "Loading gifts…", "Загрузка подарков…")
            } else {
                app.settings.ui("No visible gifts")
            },
            muted,
            width,
        );
    }
    let visible = (window.height.saturating_sub(5) / 3).max(0) as usize;
    let start = app.gifts.selected.saturating_sub(visible.saturating_sub(1));
    for (row, index) in (start..app.gifts.entries.len()).take(visible).enumerate() {
        let gift = &app.gifts.entries[index];
        let y = 2 + row as i32 * 3;
        if let Some(Visual::Ready(frames)) = app.gifts.visuals.get(&gift.id) {
            images.push(ImagePlacement {
                message_id: -1_760_000_000 - index as i32,
                asset: ImageAsset::Animated(frames.clone()),
                row: (window.y + 1 + y).max(0) as u16,
                column: (window.x + 3).max(0) as u16,
                rows: 2,
                columns: 4,
                crop: false,
            });
        } else if !app.tgp_supported {
            put(
                window,
                y,
                2,
                if app.nerd_fonts { "\u{f06b}" } else { "!" },
                accent,
                2,
            );
        }
        put(
            window,
            y,
            8,
            &gift.title,
            if index == app.gifts.selected {
                accent
            } else {
                text
            },
            width - 8,
        );
        put(window, y + 1, 8, &gift.detail, muted, width - 8);
        hits.push(HitBox {
            rect: Rect {
                x: window.x + 2,
                y: window.y + 1 + y,
                width,
                height: 2,
            },
            target: HitTarget::Gift(index),
        });
    }
    if app.gifts.next_offset.is_some() {
        let y = window.height - 3;
        put(
            window,
            y,
            2,
            if app.gifts.loading {
                app.settings
                    .label("lng_profile_loading", "Loading…", "Загрузка…")
            } else {
                app.settings.ui("Load more")
            },
            accent,
            width,
        );
        hits.push(HitBox {
            rect: Rect {
                x: window.x + 2,
                y: window.y + 1 + y,
                width,
                height: 1,
            },
            target: HitTarget::GiftMore,
        });
    }
}
