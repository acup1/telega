use super::*;
use grammers_session::types::PeerKind;

#[derive(Default)]
pub(super) struct Clicks(Option<(u64, i32, Instant)>);
impl Clicks {
    fn double(&mut self, view: u64, id: i32, now: Instant) -> bool {
        let double = self.0.is_some_and(|(old_view, old_id, at)| {
            old_view == view && old_id == id && now.duration_since(at) <= Duration::from_millis(450)
        });
        self.0 = if double { None } else { Some((view, id, now)) };
        double
    }
}
pub(super) struct Delete {
    id: i32,
    peer: PeerRef,
    view: u64,
}
pub(super) struct Deleted {
    view: u64,
    id: i32,
    result: std::result::Result<(), String>,
}

impl App {
    pub(super) fn reply_message(&mut self, id: i32) {
        if !self.require_write() {
            return;
        }
        self.message_menu = None;
        self.reply_to = Some(id);
        self.composing = true;
    }
    pub(super) fn handle_message_mouse(&mut self, key: i32, terminal: &Terminal) -> bool {
        if self.searching
            || self.attaching
            || self.theme_input
            || self.saving.is_some()
            || self.theme_preview.is_some()
        {
            return false;
        }
        if key != MOUSE_EVENT {
            return false;
        }
        let mouse = terminal.getmouse();
        if self.composing
            && mouse.event == MouseEvent::LEFT
            && !matches!(
                self.hit_test(mouse.x, mouse.y),
                Some(HitTarget::Composer | HitTarget::Send)
            )
        {
            self.composing = false;
        }
        if mouse.event == MouseEvent::LEFT
            && let Some(HitTarget::StickerPack(index)) = self.hit_test(mouse.x, mouse.y)
            && let Some(message) = self.messages.get(index).cloned()
        {
            self.select_message(index);
            self.open_sticker_pack(&message);
            return true;
        }
        if mouse.event == MouseEvent::RIGHT {
            let index = self.hitboxes.iter().rev().find_map(|h| match h.target {
                HitTarget::Message(index)
                    if h.rect.contains(i32::from(mouse.x), i32::from(mouse.y)) =>
                {
                    Some(index)
                }
                _ => None,
            });
            if let Some(index) = index {
                self.select_message(index);
                self.open_message_menu(index, mouse.x, mouse.y);
            } else {
                self.message_menu = None;
            }
            return true;
        }
        if mouse.event == MouseEvent::LEFT
            && let Some(HitTarget::Message(index)) = self.hit_test(mouse.x, mouse.y)
        {
            self.message_menu = None;
            self.select_message(index);
            if let Some(id) = self.messages.get(index).map(Message::id)
                && self.clicks.double(self.view_id, id, Instant::now())
            {
                self.reply_message(id);
            }
            return true;
        }
        false
    }
    pub(super) fn request_delete(&mut self, index: usize) {
        let (Some(message), Some(peer)) = (self.messages.get(index), self.current_peer()) else {
            return;
        };
        self.deleting = Some(Delete {
            id: message.id(),
            peer,
            view: self.view_id,
        });
        self.message_menu = None;
    }
    pub(super) fn handle_delete_input(&mut self, key: i32, terminal: &Terminal) -> bool {
        if self.deleting.is_none() {
            return false;
        }
        match key {
            27 => self.deleting = None,
            MOUSE_EVENT => {
                let mouse = terminal.getmouse();
                if mouse.event == MouseEvent::LEFT {
                    match self.hit_test(mouse.x, mouse.y) {
                        Some(HitTarget::DeleteConfirm(everyone)) => self.confirm_delete(everyone),
                        Some(HitTarget::DeletePanel) => {}
                        _ => self.deleting = None,
                    }
                }
            }
            _ => {}
        }
        true
    }
    fn confirm_delete(&mut self, everyone: bool) {
        let Some(request) = self.deleting.take() else {
            return;
        };
        let channel = request.peer.id.kind() == PeerKind::Channel;
        if channel && !everyone {
            self.deleting = Some(request);
            return;
        }
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        tokio::spawn(async move {
            let result = async {
                if channel {
                    client
                        .invoke(&tl::functions::channels::DeleteMessages {
                            channel: request.peer.into(),
                            id: vec![request.id],
                        })
                        .await?;
                } else {
                    client
                        .invoke(&tl::functions::messages::DeleteMessages {
                            revoke: everyone,
                            id: vec![request.id],
                        })
                        .await?;
                }
                Ok::<_, anyhow::Error>(())
            }
            .await
            .map_err(|e| e.to_string());
            let _ = tx.send(UiEvent::Deleted(Deleted {
                view: request.view,
                id: request.id,
                result,
            }));
        });
    }
    pub(super) fn deleted_event(&mut self, event: Deleted) {
        if event.view != self.view_id {
            return;
        }
        match event.result {
            Ok(()) => {
                if let Some(index) = self.message_index_by_id(event.id) {
                    self.remove_message(index);
                }
            }
            Err(error) => self.set_error(error),
        }
    }
}

pub(super) fn draw_delete(window: &mut Window, app: &App, hits: &mut Vec<HitBox>) {
    let Some(request) = &app.deleting else {
        return;
    };
    let normal = window.border_style;
    let danger = CellStyle::new(Some(app.theme.palette.error), normal.background);
    put(
        window,
        1,
        2,
        app.settings
            .label("", "Delete message?", "Удалить сообщение?"),
        danger,
        window.width - 6,
    );
    hits.push(HitBox {
        rect: Rect {
            x: window.x,
            y: window.y,
            width: window.width,
            height: window.height,
        },
        target: HitTarget::DeletePanel,
    });
    let mut actions = Vec::new();
    if request.peer.id.kind() != PeerKind::Channel {
        actions.push((
            app.settings
                .label("", "Delete only for me", "Удалить только у меня"),
            HitTarget::DeleteConfirm(false),
        ));
    }
    actions.push((
        app.settings.ui("Delete for everyone"),
        HitTarget::DeleteConfirm(true),
    ));
    actions.push((app.settings.ui("Cancel"), HitTarget::DeleteCancel));
    for (slot, (label, target)) in actions.into_iter().enumerate() {
        let row = 3 + slot as i32 * 2;
        put(
            window,
            row,
            2,
            label,
            if matches!(target, HitTarget::DeleteCancel) {
                normal
            } else {
                danger
            },
            window.width - 6,
        );
        hits.push(HitBox {
            rect: Rect {
                x: window.x + 2,
                y: window.y + 1 + row,
                width: window.width - 4,
                height: 1,
            },
            target,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reply_requires_two_clicks_on_the_same_message_and_view() {
        let mut clicks = Clicks::default();
        let now = Instant::now();
        assert!(!clicks.double(1, 10, now));
        assert!(clicks.double(1, 10, now + Duration::from_millis(120)));
        assert!(!clicks.double(1, 10, now + Duration::from_millis(130)));
        assert!(!clicks.double(2, 10, now + Duration::from_millis(140)));
        assert!(!clicks.double(2, 11, now + Duration::from_millis(150)));
        assert!(!clicks.double(2, 11, now + Duration::from_secs(1)));
    }
}
