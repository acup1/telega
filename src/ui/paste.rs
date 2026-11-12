use super::*;
use crate::clipboard::{self, Content, PendingAttachments};
use crate::telegram::AttachmentKind;

pub(super) enum PasteEvent {
    Clipboard {
        view_id: u64,
        request: u64,
        field: bool,
        result: std::result::Result<Content, String>,
    },
    Sent {
        view_id: u64,
        messages: Vec<Message>,
        remaining: Option<PendingAttachments>,
        error: Option<String>,
    },
}

impl App {
    pub(super) fn request_clipboard(&mut self) {
        if self.open_chat.is_none() && !self.theme_input {
            self.status = "Open a chat before attaching files".to_owned();
            return;
        }
        self.clipboard_request = self.clipboard_request.wrapping_add(1);
        let request = self.clipboard_request;
        let view_id = self.view_id;
        let field = self.theme_input || self.saving.is_some() || self.attaching || self.searching;
        let cache = self.view_cache();
        let tx = self.events_tx.clone();
        self.status = "Reading clipboard...".to_owned();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || clipboard::read(&cache))
                .await
                .map_err(|e| e.to_string())
                .and_then(|result| result.map_err(|e| e.to_string()));
            let _ = tx.send(UiEvent::Paste(PasteEvent::Clipboard {
                view_id,
                request,
                field,
                result,
            }));
        });
    }

    pub(super) fn receive_pasted_text(&mut self, text: String) {
        if self.theme_preview.is_some() {
            return;
        }
        if self.searching {
            self.search_query.push_str(text.trim());
            self.search_changed();
            return;
        }
        if self.attaching {
            self.input = text.trim().to_owned();
            return;
        }
        if self.theme_input || self.saving.is_some() {
            self.input.push_str(text.trim());
            return;
        }
        if self.open_chat.is_none() {
            self.status = "Open a chat before pasting".to_owned();
            return;
        }
        if !self.require_write() {
            return;
        }
        if let Some(files) = clipboard::pasted_paths(&text) {
            // Generate the thumbnail off the UI thread. The same request epoch
            // prevents a late preview from attaching to another conversation.
            self.clipboard_request = self.clipboard_request.wrapping_add(1);
            let request = self.clipboard_request;
            let view_id = self.view_id;
            let tx = self.events_tx.clone();
            tokio::spawn(async move {
                let result = tokio::task::spawn_blocking(move || {
                    let mut files = files;
                    files.prepare_preview();
                    Content::Files(files)
                })
                .await
                .map_err(|e| e.to_string());
                let _ = tx.send(UiEvent::Paste(PasteEvent::Clipboard {
                    view_id,
                    request,
                    field: false,
                    result,
                }));
            });
        } else {
            self.composing = true;
            self.insert_draft_text(
                &text
                    .replace("\r\n", "\n")
                    .replace('\r', "\n")
                    .chars()
                    .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
                    .collect::<String>(),
            );
        }
    }

    fn stage_attachments(&mut self, files: PendingAttachments) {
        if !self.require_write() {
            return;
        }
        self.close_details();
        self.attaching = false;
        self.attachment_menu = false;
        self.composing = false;
        if let Some(pending) = &mut self.pending_attachments {
            pending.append(files);
        } else {
            self.pending_attachments = Some(files);
        }
        self.status = "Choose photo compression, then Send · Esc cancel".to_owned();
        self.status_error = false;
    }

    pub(super) fn set_paste_kind(&mut self, kind: AttachmentKind) {
        if let Some(pending) = &mut self.pending_attachments {
            if kind == AttachmentKind::Media && !pending.can_compress() {
                self.status = "Other file types must be sent without compression".to_owned();
            } else {
                pending.kind = kind;
            }
        }
    }

    pub(super) fn send_pending_attachments(&mut self) {
        if self.sending_attachments {
            return;
        }
        if !self.require_write() {
            return;
        }
        let Some(peer) = self.current_peer() else {
            return;
        };
        let Some(mut pending) = self.pending_attachments.take() else {
            return;
        };
        let client = self.telegram.client.clone();
        let reply_to = self.reply_root();
        let view_id = self.view_id;
        let tx = self.events_tx.clone();
        self.sending_attachments = true;
        self.status = "Uploading attachments...".to_owned();
        tokio::spawn(async move {
            let mut messages = Vec::new();
            let mut error = None;
            while let Some(path) = pending.paths.first() {
                match Telegram::send_attachment_with(&client, peer, reply_to, path, pending.kind)
                    .await
                {
                    Ok(message) => {
                        messages.push(message);
                        pending.paths.remove(0);
                    }
                    Err(failure) => {
                        error = Some(failure.to_string());
                        break;
                    }
                }
            }
            let remaining = (!pending.paths.is_empty()).then_some(pending);
            let _ = tx.send(UiEvent::Paste(PasteEvent::Sent {
                view_id,
                messages,
                remaining,
                error,
            }));
        });
    }

    pub(super) fn paste_event(&mut self, event: PasteEvent) {
        match event {
            PasteEvent::Clipboard {
                view_id,
                request,
                field,
                result,
            } if view_id == self.view_id && request == self.clipboard_request => match result {
                Ok(Content::Text(text)) => self.receive_pasted_text(text),
                Ok(Content::Files(files)) if field => {
                    if let Some(path) = files.paths.first() {
                        if self.attaching {
                            self.input = path.to_string_lossy().into_owned();
                        } else {
                            self.input.push_str(&path.to_string_lossy());
                        }
                    }
                }
                Ok(Content::Files(files)) => self.stage_attachments(files),
                Err(error) => self.set_error(error),
            },
            PasteEvent::Sent {
                view_id,
                messages,
                remaining,
                error,
            } => {
                self.sending_attachments = false;
                if view_id == self.view_id {
                    // The live update may arrive before the upload result.
                    // Retain that copy, which may already contain newer edits.
                    let mut known: HashSet<_> = self.messages.iter().map(Message::id).collect();
                    self.messages.extend(
                        messages
                            .into_iter()
                            .filter(|message| known.insert(message.id())),
                    );
                    self.messages.sort_by_key(Message::id);
                    self.message_index = self.messages.len().saturating_sub(1);
                    self.scroll_from_bottom = 0;
                    self.reply_to = None;
                    if let Some(index) = self.open_chat {
                        self.queue_previews(index);
                    }
                    if let Some(remaining) = remaining {
                        self.stage_attachments(remaining);
                    } else {
                        self.status = "Attachments sent".to_owned();
                        self.status_error = false;
                    }
                    if let Some(error) = error {
                        self.set_error(error);
                    }
                } else if let Some(error) = error {
                    self.log_error(error);
                }
            }
            _ => {}
        }
    }
}

pub(super) fn draw_pending(
    window: &mut Window,
    app: &App,
    hitboxes: &mut Vec<HitBox>,
    placements: &mut Vec<ImagePlacement>,
) {
    let Some(pending) = &app.pending_attachments else {
        return;
    };
    let width = (window.width - 6).clamp(12, 58);
    let height = (window.height - 8).clamp(5, 13);
    let x = (window.width - width - 2).max(0) / 2;
    let y = (window.height - height - 2).max(0) / 2;
    let rect = Rect {
        x: window.x + 1 + x,
        y: window.y + 1 + y,
        width,
        height,
    };
    placements.retain(|p| {
        i32::from(p.column) >= rect.x + rect.width
            || i32::from(p.column) + p.columns as i32 <= rect.x
            || i32::from(p.row) >= rect.y + rect.height
            || i32::from(p.row) + p.rows as i32 <= rect.y
    });
    for row in y..y + height {
        for col in x..x + width {
            window.posstyled(row, col, ' ', window.border_style);
        }
    }
    let accent = CellStyle::foreground(app.theme.palette.accent);
    let muted = CellStyle::foreground(app.theme.palette.muted);
    put(
        window,
        y,
        x + 1,
        app.settings
            .label("", "Send attachments", "Отправить вложения"),
        accent,
        width - 2,
    );
    let label = pending
        .paths
        .iter()
        .take(2)
        .map(|path| path.file_name().unwrap_or_default().to_string_lossy())
        .collect::<Vec<_>>()
        .join(", ");
    put(
        window,
        y + 1,
        x + 1,
        &label,
        CellStyle::foreground(app.theme.palette.text),
        width - 2,
    );
    if height >= 9 {
        let bounds = Rect {
            x: x + 1,
            y: y + 3,
            width: width - 2,
            height: height - 7,
        };
        if let Some(image) = &pending.preview {
            let (pixel_width, pixel_height) = image.dimensions();
            let (columns, rows) = media::fit_dimensions(
                pixel_width,
                pixel_height,
                bounds.width.max(1) as u32,
                bounds.height.max(1) as u32,
            );
            if app.tgp_supported {
                placements.push(ImagePlacement {
                    message_id: -91_000_000,
                    asset: ImageAsset::Memory(image.clone()),
                    row: (window.y + 1 + bounds.y).max(0) as u16,
                    column: (window.x + 1 + bounds.x + (bounds.width - columns as i32) / 2).max(0)
                        as u16,
                    rows,
                    columns,
                    crop: false,
                });
            } else {
                details::placeholder(window, app, bounds);
            }
        }
    }
    let choice_y = y + height - 3;
    for (row, label, kind) in [
        (
            choice_y,
            "[c] Photo compressed / video",
            AttachmentKind::Media,
        ),
        (
            choice_y + 1,
            "[f] File · no compression",
            AttachmentKind::File,
        ),
    ] {
        let selected = pending.kind == kind;
        put(
            window,
            row,
            x + 1,
            &format!("{} {label}", if selected { "●" } else { "○" }),
            if selected { accent } else { muted },
            width - 2,
        );
        hitboxes.push(HitBox {
            rect: Rect {
                x: rect.x,
                y: window.y + 1 + row,
                width,
                height: 1,
            },
            target: HitTarget::PasteKind(kind),
        });
    }
    let send_y = y + height - 1;
    put(
        window,
        send_y,
        x + 1,
        "[ Send ]     [ Cancel ]",
        accent,
        width - 2,
    );
    hitboxes.push(HitBox {
        rect: Rect {
            x: rect.x,
            y: window.y + 1 + send_y,
            width: 10.min(width),
            height: 1,
        },
        target: HitTarget::PasteSend,
    });
    hitboxes.push(HitBox {
        rect: Rect {
            x: rect.x + 13,
            y: window.y + 1 + send_y,
            width: (width - 13).clamp(0, 10),
            height: 1,
        },
        target: HitTarget::PasteCancel,
    });
}
