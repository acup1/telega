use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use grammers_client::message::Message;
use grammers_client::peer::Peer;
use grammers_client::tl;
use grammers_session::types::{PeerId, PeerRef};
use terminalib::terminal::Terminal;
use terminalib::tgp::{ImageData, ImageGroup, TerminalImage, query_support};
use terminalib::{
    CellStyle, KEY_DOWN, KEY_LEFT, KEY_PASTE, KEY_RIGHT, KEY_SHIFT_ENTER, KEY_UP, MOUSE_EVENT,
    MouseEvent, Window, WindowId, WindowManager,
};
use tokio::sync::{Semaphore, mpsc};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::telegram::{Chat, Discussion, Folder, Telegram};
use crate::{media, theme};

mod chat_search;
mod code;
mod compat;
mod composer;
mod details;
mod editing;
mod emoji;
mod forwarded;
mod gifts;
mod global_search;
mod inline_emoji;
mod interaction;
mod latex;
mod live;
mod paste;
mod permissions;
mod reactions;
mod reads;
mod replies;
mod settings;
mod stories;
use details::{Details, DetailsEvent, Pins, ProfileTab, SharedTab};

const PREVIEW_ROWS: i32 = 7;
const HISTORY_PAGE: usize = 50;

pub async fn run(
    telegram: Telegram,
    cache: PathBuf,
    avatar_cache: PathBuf,
    log_path: PathBuf,
) -> Result<()> {
    let _stderr_redirect = StderrRedirect::new(log_path.clone()).ok();
    let mut terminal = Terminal::wannajustwork()?;
    terminal.enable_bracketed_paste()?;
    terminal.enable_enhanced_keyboard()?;
    let tgp_supported =
        query_support(&mut terminal, std::time::Duration::from_millis(500)).unwrap_or(false);
    let mut windows = WindowManager::new();
    let panels = Panels {
        sidebar: windows.new_window(0),
        folders: windows.new_window(0),
        content: windows.new_window(0),
        composer: windows.new_window(1),
        emoji: windows.new_window(2),
        chat_search: windows.new_window(2),
        details: windows.new_window(3),
        stories: windows.new_window(3),
        settings: windows.new_window(4),
    };
    let (events_tx, events_rx) = mpsc::unbounded_channel();

    let settings_root = log_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."))
        .to_path_buf();
    let theme = theme::Theme::restore(&settings_root);
    let nerd_fonts = std::env::var("TELEGA_NERD_FONTS")
        .ok()
        .or_else(|| fs::read_to_string(settings_root.join("nerd-fonts")).ok())
        .is_none_or(|value| !matches!(value.trim(), "0" | "false" | "off"));
    let settings = settings::State::restore(&settings_root);
    let mut app = App {
        telegram,
        cache,
        avatar_cache,
        log_path,
        chats: Vec::new(),
        stories: stories::State::default(),
        stories_rect: None,
        gifts: gifts::State::default(),
        gifts_rect: None,
        folders: Vec::new(),
        active_folder: 0,
        sidebar_collapsed: false,
        chat_index: 0,
        messages: Vec::new(),
        message_index: 0,
        scroll_from_bottom: 0,
        open_chat: None,
        discussion: None,
        view_id: 0,
        read_outbox_max: 0,
        read_poll: Instant::now(),
        read_poll_pending: false,
        composing: false,
        composer: composer::State::default(),
        composer_width: 40,
        editing: None,
        edit_previous_pending: false,
        searching: false,
        global_search_due: None,
        global_search_generation: 0,
        global_search_loading: false,
        remote_results: HashSet::new(),
        mention_request: 0,
        chat_search: None,
        search_request: 0,
        attaching: false,
        saving: None,
        attachment_menu: false,
        pending_attachments: None,
        clipboard_request: 0,
        sending_attachments: false,
        attachment_kind: crate::telegram::AttachmentKind::Media,
        attachment_directory: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        attachment_entries: Vec::new(),
        attachment_scroll: 0,
        menu_open: false,
        theme,
        theme_input: false,
        theme_preview: None,
        theme_request: 0,
        details: None,
        details_generation: 0,
        details_rect: None,
        pins: Pins::default(),
        has_newer_messages: false,
        loading_newer: false,
        settings_root,
        nerd_fonts,
        message_menu: None,
        reaction_state: reactions::State::default(),
        forwarding: None,
        reply_to: None,
        replies: replies::State::default(),
        forwards: forwarded::State::default(),
        code_cache: std::cell::RefCell::default(),
        menu_overlay_cache: std::cell::RefCell::default(),
        emoji_state: emoji::State::default(),
        draft_custom: Vec::new(),
        live_state: live::State::default(),
        inbox_reads: reads::State::default(),
        settings,
        clicks: interaction::Clicks::default(),
        deleting: None,
        input: String::new(),
        search_query: String::new(),
        status: "Loading chats…".to_owned(),
        status_error: false,
        messages_loading: false,
        loading_older: false,
        has_more_messages: true,
        hitboxes: Vec::new(),
        previews: HashMap::new(),
        preview_failures: HashMap::new(),
        custom_emoji: HashMap::new(),
        sidebar_emoji: HashMap::new(),
        compatible_reactions: Vec::new(),
        avatars: HashMap::new(),
        author_avatars: HashMap::new(),
        audio: None,
        inline_video: None,
        gifs: HashMap::new(),
        gif_failures: HashSet::new(),
        inline_images: InlineImages::default(),
        events_tx,
        events_rx,
        preview_workers: Arc::new(Semaphore::new(4)),
        preview_tasks: tokio::task::JoinSet::new(),
        sticker_workers: Arc::new(Semaphore::new(2)),
        avatar_workers: Arc::new(Semaphore::new(2)),
        tgp_supported,
    };
    (app.chats, app.folders) = app.telegram.chats(500).await?;
    app.queue_avatars();
    if app.settings.language_code != "en" {
        let language = app.settings.language_code.clone();
        app.request_language(language);
    }
    app.start_chat_refresh();
    let (stop_updates, updates_task) = app.start_updates().await?;
    app.status = "Ready".to_owned();
    if let Ok(source) = std::env::var("TELEGA_THEME") {
        app.request_theme(source);
    }

    let mut dirty = true;
    let mut last_preview_retry = Instant::now();
    loop {
        app.draft_custom.retain(|emoji| {
            crate::custom_emoji::slice_utf16(&app.input, emoji.offset, emoji.length)
                == Some(emoji.alt.as_str())
        });
        dirty |= app.process_events();
        app.poll_global_search();
        app.poll_stories();
        app.poll_read_receipts();
        dirty |= app.tick_audio();
        dirty |= app.tick_inline_playback();
        dirty |= app.inline_images.animation_due();
        dirty |= app.inline_images.pending_upload;
        if last_preview_retry.elapsed() >= Duration::from_secs(1) {
            last_preview_retry = Instant::now();
            if let Some(index) = app.open_chat {
                app.queue_previews_near(index);
            }
        }
        if dirty {
            draw(&mut terminal, &mut windows, &panels, &mut app)?;
            dirty = app.observe_visible_reads();
        }
        app.poll_inbox_reads();
        let mut key = terminal.get_key(32, false)?;
        if key == 0 {
            continue;
        }
        dirty = true;
        if key == 27 {
            app.clipboard_request = app.clipboard_request.wrapping_add(1);
        }
        if app.handle_delete_input(key, &terminal) {
            continue;
        }
        if app.handle_settings_input(key, &terminal) {
            continue;
        }
        if app.gifts.popup {
            match key {
                27 | 113 => app.close_profile_gifts(),
                KEY_UP => app.gifts.selected = app.gifts.selected.saturating_sub(1),
                KEY_DOWN => {
                    if app.gifts.selected + 1 < app.gifts.entries.len() {
                        app.gifts.selected += 1;
                    } else {
                        app.load_gift_page();
                    }
                }
                MOUSE_EVENT => {
                    let mouse = terminal.getmouse();
                    match mouse.event {
                        MouseEvent::WHEELUP => {
                            app.gifts.selected = app.gifts.selected.saturating_sub(1)
                        }
                        MouseEvent::WHEELDOWN => {
                            if app.gifts.selected + 1 < app.gifts.entries.len() {
                                app.gifts.selected += 1;
                            } else {
                                app.load_gift_page();
                            }
                        }
                        MouseEvent::LEFT => {
                            if !app.gifts_rect.is_some_and(|rect| {
                                rect.contains(i32::from(mouse.x), i32::from(mouse.y))
                            }) {
                                app.close_profile_gifts();
                            } else {
                                match app.hit_test(mouse.x, mouse.y) {
                                    Some(HitTarget::GiftClose) => app.close_profile_gifts(),
                                    Some(HitTarget::GiftMore) => app.load_gift_page(),
                                    Some(HitTarget::Gift(index)) => app.gifts.selected = index,
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
            continue;
        }
        if app.stories.popup {
            match key {
                27 | 113 => app.stories.popup = false,
                KEY_UP => app.stories.selected = app.stories.selected.saturating_sub(1),
                KEY_DOWN => {
                    app.stories.selected =
                        (app.stories.selected + 1).min(app.stories.entries.len().saturating_sub(1))
                }
                10 | 13 => {
                    if let Some(peer) = app
                        .stories
                        .entries
                        .get(app.stories.selected)
                        .map(|entry| entry.peer)
                    {
                        app.stories.popup = false;
                        open_stories_peer(&mut app, peer, &mut terminal, &mut windows).await;
                    }
                }
                MOUSE_EVENT => {
                    let mouse = terminal.getmouse();
                    if mouse.event == MouseEvent::LEFT {
                        if !app
                            .stories_rect
                            .is_some_and(|r| r.contains(i32::from(mouse.x), i32::from(mouse.y)))
                        {
                            app.stories.popup = false;
                            continue;
                        }
                        match app.hit_test(mouse.x, mouse.y) {
                            Some(HitTarget::Story(index)) => {
                                if let Some(peer) =
                                    app.stories.entries.get(index).map(|entry| entry.peer)
                                {
                                    app.stories.popup = false;
                                    open_stories_peer(&mut app, peer, &mut terminal, &mut windows)
                                        .await;
                                }
                            }
                            Some(HitTarget::StoryClose) | None => app.stories.popup = false,
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if app.handle_chat_search_input(key, &mut terminal) {
            continue;
        }

        if key == KEY_PASTE {
            if let Some(text) = terminal.take_paste() {
                app.receive_pasted_text(text);
            }
            continue;
        }
        if key == 22 {
            app.request_clipboard();
            continue;
        }
        if app.pending_attachments.is_some() {
            match key {
                27 => app.pending_attachments = None,
                10 | 13 => app.send_pending_attachments(),
                value if value == i32::from(b'c') => {
                    app.set_paste_kind(crate::telegram::AttachmentKind::Media)
                }
                value if value == i32::from(b'f') => {
                    app.set_paste_kind(crate::telegram::AttachmentKind::File)
                }
                MOUSE_EVENT => {
                    let mouse = terminal.getmouse();
                    if mouse.event == MouseEvent::LEFT {
                        match app.hit_test(mouse.x, mouse.y) {
                            Some(HitTarget::PasteSend) => app.send_pending_attachments(),
                            Some(HitTarget::PasteCancel) => app.pending_attachments = None,
                            Some(HitTarget::PasteKind(kind)) => app.set_paste_kind(kind),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if app.handle_reaction_picker_input(key, &terminal) {
            continue;
        }
        if app.handle_emoji_input(key, &terminal) {
            continue;
        }
        if app.handle_message_mouse(key, &terminal) {
            continue;
        }
        if key == MOUSE_EVENT && terminal.getmouse().event == MouseEvent::LEFT {
            match app.hit_test(terminal.getmouse().x, terminal.getmouse().y) {
                Some(HitTarget::Send) if app.require_write() => {
                    app.composing = true;
                    app.message_menu = None;
                    app.attachment_menu = false;
                    app.forwarding = None;
                    key = 13;
                }
                Some(HitTarget::CancelComposeAction) => {
                    app.cancel_edit();
                    app.reply_to = None;
                    continue;
                }
                _ => {}
            }
        }

        if app.saving.is_some() {
            match key {
                27 => {
                    app.saving = None;
                    app.input.clear();
                }
                8 | 127 => pop_grapheme(&mut app.input),
                10 | 13 => app.save_shared_file(),
                _ => {
                    if let Some(text) = input_text(&terminal)
                        && !text.chars().any(char::is_control)
                    {
                        app.input.push_str(&text);
                    }
                }
            }
            continue;
        }

        if app.theme_preview.is_some() {
            match key {
                27 => app.theme_preview = None,
                10 | 13 => app.apply_theme_preview(),
                MOUSE_EVENT => {
                    let mouse = terminal.getmouse();
                    if mouse.event == MouseEvent::LEFT {
                        match app.hit_test(mouse.x, mouse.y) {
                            Some(HitTarget::ApplyTheme) => app.apply_theme_preview(),
                            Some(HitTarget::CancelTheme) => app.theme_preview = None,
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            continue;
        }

        if app.theme_input {
            match key {
                27 => {
                    app.theme_input = false;
                    app.input.clear();
                }
                8 | 127 => pop_grapheme(&mut app.input),
                10 | 13 => {
                    let source = std::mem::take(&mut app.input);
                    app.theme_input = false;
                    if !source.trim().is_empty() {
                        app.request_theme(source.trim().to_owned());
                    }
                }
                _ => {
                    if let Some(text) = input_text(&terminal)
                        && !text.chars().any(char::is_control)
                    {
                        app.input.push_str(&text);
                    }
                }
            }
            continue;
        }

        if key == 27 && (app.message_menu.is_some() || app.forwarding.is_some()) {
            app.message_menu = None;
            app.forwarding = None;
            app.status = "Ready".to_owned();
            continue;
        }

        if app.searching {
            match key {
                27 => {
                    app.searching = false;
                    app.search_query.clear();
                    app.search_changed();
                }
                8 | 127 => {
                    pop_grapheme(&mut app.search_query);
                    app.search_changed();
                }
                KEY_UP => app.select_chat_previous(),
                KEY_DOWN => app.select_chat_next(),
                10 | 13 => {
                    app.searching = false;
                    let visible = app.visible_chat_indices();
                    if let Some(index) = visible
                        .contains(&app.chat_index)
                        .then_some(app.chat_index)
                        .or_else(|| visible.first().copied())
                    {
                        app.open_chat(index);
                    }
                }
                MOUSE_EVENT => {
                    let mouse = terminal.getmouse();
                    match mouse.event {
                        MouseEvent::WHEELUP => app.select_chat_previous(),
                        MouseEvent::WHEELDOWN => app.select_chat_next(),
                        MouseEvent::LEFT => match app.hit_test(mouse.x, mouse.y) {
                            Some(HitTarget::Chat(index)) => {
                                app.searching = false;
                                app.open_chat(index);
                            }
                            Some(HitTarget::Composer) => {
                                app.searching = false;
                                app.activate_composer_at(mouse.x, mouse.y);
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                }
                _ => {
                    if let Some(text) = input_text(&terminal)
                        && !text.chars().any(char::is_control)
                    {
                        app.search_query.push_str(&text);
                        app.search_changed();
                        if let Some(index) = app.visible_chat_indices().first().copied() {
                            app.chat_index = index;
                        }
                    }
                }
            }
            continue;
        }

        if app.attaching {
            if key == MOUSE_EVENT {
                let mouse = terminal.getmouse();
                if mouse.event == MouseEvent::WHEELUP {
                    app.attachment_scroll = app.attachment_scroll.saturating_sub(3);
                    continue;
                }
                if mouse.event == MouseEvent::WHEELDOWN {
                    app.attachment_scroll = (app.attachment_scroll + 3)
                        .min(app.attachment_entries.len().saturating_sub(1));
                    continue;
                }
                if mouse.event == MouseEvent::LEFT {
                    match app.hit_test(mouse.x, mouse.y) {
                        Some(HitTarget::AttachmentPath(path)) => app.browse_attachment(path),
                        Some(HitTarget::AttachmentSend) => {
                            // Run the same explicit send path as Enter below.
                        }
                        _ => continue,
                    }
                    if !matches!(
                        app.hit_test(mouse.x, mouse.y),
                        Some(HitTarget::AttachmentSend)
                    ) {
                        continue;
                    }
                } else {
                    continue;
                }
            }
            match key {
                27 => {
                    app.attaching = false;
                    app.input.clear();
                }
                8 | 127 => {
                    pop_grapheme(&mut app.input);
                }
                10 | 13 | MOUSE_EVENT => {
                    if !app.input.trim().is_empty() {
                        if !app.require_write() {
                            continue;
                        }
                        let peer = app.current_peer().context("no chat selected")?;
                        let reply_root = app.reply_root();
                        let path = PathBuf::from(app.input.trim());
                        if path.is_dir() {
                            app.browse_attachment(path);
                            continue;
                        }
                        app.input.clear();
                        app.attaching = false;
                        app.status = format!("Uploading {}…", path.display());
                        let result = app
                            .telegram
                            .send_attachment(peer, reply_root, &path, app.attachment_kind)
                            .await;
                        match result {
                            Ok(message) => {
                                app.messages.push(message);
                                app.message_index = app.messages.len().saturating_sub(1);
                                app.scroll_from_bottom = 0;
                                app.status = "Attachment sent".to_owned();
                                if let Some(chat_index) = app.open_chat {
                                    app.queue_previews(chat_index);
                                }
                            }
                            Err(error) => app.set_error(error),
                        }
                    }
                }
                _ => {
                    if let Some(text) = input_text(&terminal)
                        && !text.chars().any(char::is_control)
                    {
                        app.input.push_str(&text);
                    }
                }
            }
            continue;
        }

        // A first click must focus and position the cursor just like a click
        // in an already active editor, and dismiss the message action menu.
        if key == MOUSE_EVENT {
            let mouse = terminal.getmouse();
            if mouse.event == MouseEvent::LEFT
                && matches!(app.hit_test(mouse.x, mouse.y), Some(HitTarget::Composer))
            {
                app.activate_composer_at(mouse.x, mouse.y);
                continue;
            }
        }
        // Navigation buttons must remain usable without discarding a draft.
        if app.composing && key == MOUSE_EVENT {
            let mouse = terminal.getmouse();
            if mouse.event == MouseEvent::LEFT {
                let target = app.hit_test(mouse.x, mouse.y);
                if matches!(target, Some(HitTarget::Send)) {
                    key = 13;
                } else {
                    app.composing = false;
                }
                match target {
                    Some(HitTarget::Latest) => {
                        app.jump_to_latest();
                        continue;
                    }
                    Some(HitTarget::ToggleSidebar) => {
                        app.toggle_sidebar();
                        continue;
                    }
                    Some(HitTarget::Menu) => {
                        app.menu_open = true;
                        continue;
                    }
                    Some(HitTarget::CopyCode(source)) => {
                        app.copy_code(&mut terminal, &source);
                        continue;
                    }
                    _ => {}
                }
            }
        }
        if app.composing && app.message_menu.is_none() {
            match key {
                27 => {
                    app.cancel_edit();
                    app.composing = false;
                    app.status = "Input unfocused · e to resume".to_owned();
                }
                8 | 127 => {
                    app.composer
                        .backspace(&mut app.input, &mut app.draft_custom);
                }
                KEY_SHIFT_ENTER => app.insert_draft_text("\n"),
                KEY_LEFT | KEY_RIGHT | KEY_DOWN => {
                    app.composer
                        .move_cursor(key, &app.input, app.composer_width)
                }
                KEY_UP => {
                    if app.composer.on_first_line(&app.input, app.composer_width) {
                        app.edit_previous();
                    } else {
                        app.composer
                            .move_cursor(key, &app.input, app.composer_width);
                    }
                }
                10 | 13 => {
                    if !app.input.trim().is_empty() {
                        if app.editing.is_some() {
                            app.submit_edit().await;
                            continue;
                        }
                        if !app.require_write() {
                            continue;
                        }
                        let peer = app.current_peer().context("no chat selected")?;
                        let reply_root = app.reply_root();
                        app.reply_to = None;
                        let text = std::mem::take(&mut app.input);
                        app.composer = composer::State::default();
                        app.composing = true;
                        app.status = "Sending…".to_owned();
                        draw(&mut terminal, &mut windows, &panels, &mut app)?;
                        let draft = std::mem::take(&mut app.draft_custom);
                        let result = if draft.is_empty() {
                            if let Some(root) = reply_root {
                                app.telegram.send_reply(peer, root, &text).await
                            } else {
                                app.telegram.send(peer, &text).await
                            }
                        } else {
                            emoji::send_text(
                                &app.telegram.client,
                                peer,
                                &text,
                                reply_root,
                                &draft,
                                app.emoji_state.premium(),
                            )
                            .await
                        };
                        match result {
                            Ok(message) => {
                                app.messages.push(message);
                                app.message_index = app.messages.len().saturating_sub(1);
                                app.scroll_from_bottom = 0;
                                if let Some(chat) =
                                    app.open_chat.and_then(|index| app.chats.get_mut(index))
                                {
                                    chat.last_message = app.messages.last().cloned();
                                    chat.last = chat
                                        .last_message
                                        .as_ref()
                                        .map_or(text.replace('\n', " "), |message| {
                                            message.text().replace('\n', " ")
                                        });
                                }
                                app.status = "Sent".to_owned();
                                app.status_error = false;
                                if let Some(index) = app.open_chat {
                                    app.queue_previews(index);
                                }
                            }
                            Err(error) => {
                                app.input = text;
                                app.draft_custom = draft;
                                app.composer = composer::State::default();
                                app.composing = true;
                                app.reply_to = reply_root;
                                app.set_error(error);
                            }
                        }
                    }
                }
                _ => {
                    if let Some(text) = input_text(&terminal)
                        && !text.chars().any(char::is_control)
                    {
                        app.insert_draft_text(&text);
                    }
                }
            }
            continue;
        }

        if let Some(action) = back_action(
            key,
            app.active_folder,
            app.open_chat.is_some(),
            app.details.is_some(),
            app.menu_open
                || app.attachment_menu
                || app.message_menu.is_some()
                || app.forwarding.is_some(),
        ) {
            match action {
                BackAction::CloseOverlay => {
                    app.menu_open = false;
                    app.attachment_menu = false;
                    app.message_menu = None;
                    app.forwarding = None;
                    app.status = "Ready".to_owned();
                }
                BackAction::CloseDetails => app.close_details(),
                BackAction::CloseChat => app.close_chat(),
                BackAction::Home => app.switch_folder(0),
                BackAction::Quit => break,
            }
            continue;
        }

        match key {
            value if value == i32::from(b'm') => app.menu_open = !app.menu_open,
            value if value == i32::from(b't') => {
                app.theme_input = true;
                app.input.clear();
            }
            value if value == i32::from(b'/') && app.open_chat.is_none() => {
                app.start_search();
            }
            KEY_LEFT if app.open_chat.is_some() => app.close_chat(),
            value if value == i32::from(b'p') && app.open_chat.is_some() => {
                app.open_details(SharedTab::Pinned)
            }
            value if value == i32::from(b's') && app.open_chat.is_some() => {
                open_stories(&mut app, &mut terminal, &mut windows).await;
            }
            value if value == i32::from(b's') => app.stories.popup = true,
            KEY_UP => app.select_previous(),
            KEY_DOWN => app.select_next(),
            KEY_RIGHT | 10 | 13 if app.open_chat.is_none() => app.open_selected_chat(),
            10 | 13 if app.open_chat.is_some() => {
                let index = app.message_index;
                if app.messages.get(index).is_some_and(media::is_audio) {
                    app.toggle_audio(index);
                } else {
                    open_media_at(index, &mut app, &mut terminal, &mut windows).await?;
                }
            }
            value
                if matches!(value, 101 | 105) && app.open_chat.is_some() && app.require_write() =>
            {
                app.composing = true;
            }
            value if value == i32::from(b'a') && app.open_chat.is_some() => {
                app.attachment_menu = !app.attachment_menu;
            }
            value if value == i32::from(b'c') && app.open_chat.is_some() => {
                open_discussion_view(&mut app, false).await;
            }
            value if value == i32::from(b'g') && app.open_chat.is_some() => {
                open_discussion_view(&mut app, true).await;
            }
            value if value == i32::from(b'r') => {
                if app.discussion.is_some() {
                    let full_chat = app.discussion.as_ref().is_some_and(|view| view.full_chat);
                    open_discussion_view(&mut app, full_chat).await;
                } else if let Some(index) = app.open_chat {
                    app.request_messages(index, false);
                } else {
                    app.status = "Refreshing chats…".to_owned();
                    match app.telegram.chats(500).await {
                        Ok((chats, folders)) => {
                            app.chats = chats;
                            app.folders = folders;
                            app.avatars.clear();
                            app.queue_avatars();
                            app.status = "Updated".to_owned();
                            app.status_error = false;
                        }
                        Err(error) => app.set_error(error),
                    }
                }
            }
            MOUSE_EVENT => {
                let mouse = terminal.getmouse();
                match mouse.event {
                    MouseEvent::WHEELUP => {
                        if app.scroll_story_bar(mouse.x, mouse.y, false, terminal.getsize().1) {
                            continue;
                        }
                        if app
                            .details_rect
                            .is_some_and(|r| r.contains(i32::from(mouse.x), i32::from(mouse.y)))
                        {
                            app.scroll_details(false);
                        } else if i32::from(mouse.x)
                            < sidebar_width(terminal.getsize().1, app.sidebar_is_compact())
                        {
                            app.select_chat_previous();
                        } else {
                            app.scroll_messages_up();
                        }
                    }
                    MouseEvent::WHEELDOWN => {
                        if app.scroll_story_bar(mouse.x, mouse.y, true, terminal.getsize().1) {
                            continue;
                        }
                        if app
                            .details_rect
                            .is_some_and(|r| r.contains(i32::from(mouse.x), i32::from(mouse.y)))
                        {
                            app.scroll_details(true);
                        } else if i32::from(mouse.x)
                            < sidebar_width(terminal.getsize().1, app.sidebar_is_compact())
                        {
                            app.select_chat_next();
                        } else {
                            app.scroll_messages_down();
                        }
                    }
                    MouseEvent::LEFT => {
                        if let Some(target) = app.hit_test(mouse.x, mouse.y) {
                            match target {
                                HitTarget::Menu => app.menu_open = !app.menu_open,
                                HitTarget::DeleteConfirm(_)
                                | HitTarget::DeleteCancel
                                | HitTarget::DeletePanel => {}
                                HitTarget::SettingsPanel
                                | HitTarget::SettingsTab(_)
                                | HitTarget::SettingsToggle(_)
                                | HitTarget::Language(_)
                                | HitTarget::LanguageInput
                                | HitTarget::CloseSettings => {}
                                HitTarget::ToggleSidebar => app.toggle_sidebar(),
                                HitTarget::EmojiToggle => app.toggle_emoji_picker(),
                                HitTarget::EmojiPanel
                                | HitTarget::EmojiTab(_)
                                | HitTarget::EmojiPack(_)
                                | HitTarget::EmojiPick(_)
                                | HitTarget::EmojiClose => {}
                                HitTarget::CopyCode(source) => {
                                    app.copy_code(&mut terminal, &source)
                                }
                                HitTarget::CopyUsername(username) => {
                                    match copy_to_clipboard(&mut terminal, &username) {
                                        Ok(()) => {
                                            app.status = "Username copied".into();
                                            app.status_error = false;
                                        }
                                        Err(error) => app.set_error(error),
                                    }
                                }
                                HitTarget::Clipboard => app.request_clipboard(),
                                HitTarget::PasteSend
                                | HitTarget::PasteCancel
                                | HitTarget::PasteKind(_) => {}
                                HitTarget::Theme(source) => app.request_theme(source),
                                HitTarget::PreviewTheme(source) => {
                                    app.request_theme_preview(source)
                                }
                                HitTarget::Mention(username) => app.resolve_mention(username),
                                HitTarget::ApplyTheme | HitTarget::CancelTheme => {}
                                HitTarget::Profile => app.open_details(SharedTab::Media),
                                HitTarget::ProfileTab(tab) => app.change_profile_tab(tab),
                                HitTarget::ProfilePost(id) => {
                                    if let Some(peer) = app.current_peer() {
                                        open_stories_source(
                                            &mut app,
                                            peer,
                                            &mut terminal,
                                            &mut windows,
                                            media::StorySource::Pinned,
                                            Some(id),
                                        )
                                        .await;
                                    }
                                }
                                HitTarget::ProfilePostsMore => app.load_posts_page(),
                                HitTarget::Stories => {
                                    open_stories(&mut app, &mut terminal, &mut windows).await
                                }
                                HitTarget::Story(index) => {
                                    if let Some(peer) =
                                        app.stories.entries.get(index).map(|entry| entry.peer)
                                    {
                                        open_stories_peer(
                                            &mut app,
                                            peer,
                                            &mut terminal,
                                            &mut windows,
                                        )
                                        .await;
                                    }
                                }
                                HitTarget::StoryMore => app.stories.popup = true,
                                HitTarget::StoryPanel
                                | HitTarget::StoryClose
                                | HitTarget::GiftPanel
                                | HitTarget::GiftClose => {}
                                HitTarget::GiftMore => app.load_gift_page(),
                                HitTarget::Gift(index) => app.gifts.selected = index,
                                HitTarget::AvatarGallery => {
                                    if let Some(peer) = app.current_peer() {
                                        match media::open_avatar_gallery(
                                            &app.telegram.client,
                                            peer,
                                            &app.avatar_cache,
                                            app.details
                                                .as_ref()
                                                .and_then(|panel| panel.avatar.clone()),
                                            media::ViewerContext {
                                                terminal: &mut terminal,
                                                windows: &mut windows,
                                                tgp_supported: app.tgp_supported,
                                                nerd_fonts: app.nerd_fonts,
                                            },
                                        )
                                        .await
                                        {
                                            Ok(()) => {}
                                            Err(error) => app.set_error(error),
                                        }
                                    }
                                }
                                HitTarget::CloseDetails => app.close_details(),
                                HitTarget::SharedTab(tab) => app.change_shared_tab(tab),
                                HitTarget::SharedMore => app.load_shared_page(),
                                HitTarget::SharedAudio {
                                    index,
                                    timeline_x,
                                    timeline_width,
                                } => {
                                    if let Some(message) = app
                                        .details
                                        .as_ref()
                                        .and_then(|p| p.messages.get(index))
                                        .cloned()
                                    {
                                        if i32::from(mouse.x) >= timeline_x {
                                            app.seek_audio_message(
                                                &message,
                                                f64::from(i32::from(mouse.x) - timeline_x)
                                                    / f64::from(timeline_width.max(1)),
                                            );
                                        } else {
                                            app.toggle_audio_message(&message);
                                        }
                                    }
                                }
                                HitTarget::SharedMessage(index) => {
                                    details::open_shared(
                                        index,
                                        &mut app,
                                        &mut terminal,
                                        &mut windows,
                                    )
                                    .await?
                                }
                                HitTarget::PinnedNext => app.next_pin(),
                                HitTarget::Pins => app.open_details(SharedTab::Pinned),
                                HitTarget::ImportTheme => {
                                    app.theme_input = true;
                                    app.input.clear();
                                    app.menu_open = false;
                                }
                                HitTarget::ThemeBackground => {
                                    app.theme.transparent = !app.theme.transparent;
                                    if let Err(error) = app.theme.save(&app.settings_root) {
                                        app.set_error(error);
                                    }
                                }
                                HitTarget::NerdFonts => {
                                    app.nerd_fonts = !app.nerd_fonts;
                                    if let Err(error) = fs::write(
                                        app.settings_root.join("nerd-fonts"),
                                        if app.nerd_fonts { "1" } else { "0" },
                                    ) {
                                        app.set_error(error);
                                    }
                                }
                                HitTarget::Search => {
                                    app.start_search();
                                }
                                HitTarget::ChatSearchResult(_)
                                | HitTarget::ChatSearchMore
                                | HitTarget::ChatSearchClose => {}
                                HitTarget::Folder(folder) => app.switch_folder(folder),
                                HitTarget::Chat(index) => {
                                    if let Some(message) = app.forwarding.clone() {
                                        let destination = app.chats.get(index).map(|chat| {
                                            (chat.peer, permissions::can_write(&chat.peer_object))
                                        });
                                        if let Some((destination, writable)) = destination {
                                            if !writable {
                                                app.status = "Cannot forward to this chat".into();
                                                continue;
                                            }
                                            app.forwarding = None;
                                            app.status = "Forwarding…".to_owned();
                                            match message.forward_to(destination).await {
                                                Ok(_) => {
                                                    app.status = "Forwarded".to_owned();
                                                    app.status_error = false;
                                                }
                                                Err(error) => app.set_error(error),
                                            }
                                        }
                                    } else {
                                        app.open_chat(index);
                                    }
                                }
                                HitTarget::Message(index) => {
                                    app.select_message(index);
                                    app.open_message_menu(index, mouse.x, mouse.y);
                                }
                                HitTarget::MessageAuthor(peer) => app.open_message_author(peer),
                                HitTarget::ForwardSource(peer) => app.open_forward_source(peer),
                                HitTarget::Reply(target) => app.open_reply(target),
                                HitTarget::Latest => app.jump_to_latest(),
                                HitTarget::Media(index) | HitTarget::StickerPack(index) => {
                                    app.select_message(index);
                                    open_media_at(index, &mut app, &mut terminal, &mut windows)
                                        .await?;
                                }
                                HitTarget::Audio {
                                    index,
                                    timeline_x,
                                    timeline_width,
                                } => {
                                    app.select_message(index);
                                    let x = i32::from(mouse.x);
                                    if x >= timeline_x {
                                        let ratio = f64::from(x - timeline_x)
                                            / f64::from(timeline_width.max(1));
                                        app.seek_audio(index, ratio);
                                    } else {
                                        app.toggle_audio(index);
                                    }
                                }
                                HitTarget::Reaction {
                                    message_id,
                                    reaction,
                                } => app.choose_reaction(message_id, Some(reaction)),
                                HitTarget::ReactionPicker(message_id) => {
                                    app.open_reaction_picker(message_id)
                                }
                                HitTarget::RemoveReaction(message_id) => {
                                    app.choose_reaction(message_id, None)
                                }
                                HitTarget::ReactionPack(_) | HitTarget::CloseReactions => {}
                                HitTarget::MessageAction { message_id, action } => {
                                    if let Some(index) = app.message_index_by_id(message_id) {
                                        match action {
                                            MessageAction::Reply => {
                                                app.reply_message(message_id);
                                                app.status = "Replying to message".to_owned();
                                            }
                                            MessageAction::Edit => {
                                                app.begin_edit(app.messages[index].clone())
                                            }
                                            MessageAction::Copy => {
                                                app.message_menu = None;
                                                let text = app.messages[index].text().to_owned();
                                                if text.is_empty() {
                                                    app.status = "Message has no text".to_owned();
                                                } else if let Err(error) =
                                                    copy_to_clipboard(&mut terminal, &text)
                                                {
                                                    app.set_error(error);
                                                } else {
                                                    app.status = "Copied".to_owned();
                                                    app.status_error = false;
                                                }
                                            }
                                            MessageAction::Forward => {
                                                app.message_menu = None;
                                                app.forwarding = Some(app.messages[index].clone());
                                                app.status = "Choose a destination chat".to_owned();
                                            }
                                            MessageAction::Delete => {
                                                app.request_delete(index);
                                            }
                                            MessageAction::Pin => app.toggle_pin(index),
                                            MessageAction::MoreReactions => {
                                                app.open_reaction_picker(message_id)
                                            }
                                            MessageAction::RemoveReaction => {
                                                app.choose_reaction(message_id, None)
                                            }
                                        }
                                    }
                                }
                                HitTarget::Comments(index) => {
                                    app.select_message(index);
                                    open_discussion_view(&mut app, false).await;
                                }
                                HitTarget::Composer => app.activate_composer_at(mouse.x, mouse.y),
                                HitTarget::Send | HitTarget::CancelComposeAction => {}
                                HitTarget::Attachment => {
                                    if app.require_write() {
                                        app.attachment_menu = !app.attachment_menu;
                                    }
                                }
                                HitTarget::AttachmentKind(kind) => {
                                    app.attachment_kind = kind;
                                    app.attachment_menu = false;
                                    app.attaching = true;
                                    app.browse_attachment(app.attachment_directory.clone());
                                }
                                HitTarget::AttachmentPath(_) | HitTarget::AttachmentSend => {}
                            }
                        } else {
                            app.message_menu = None;
                        }
                    }
                    _ => {}
                }
            }
            _ => {
                if let Some(text) = input_text(&terminal) {
                    match text.as_str() {
                        "j" => app.select_next(),
                        "k" => app.select_previous(),
                        _ => {}
                    }
                }
            }
        }
    }

    app.stop_audio();
    app.gifs.clear();
    let _ = stop_updates.send(());
    let _ = tokio::time::timeout(Duration::from_secs(2), updates_task).await;
    let graphics_result = app.inline_images.clear(&mut terminal);
    let restore_result = terminal.restore();
    graphics_result?;
    restore_result?;
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
enum BackAction {
    CloseOverlay,
    CloseDetails,
    CloseChat,
    Home,
    Quit,
}

// Called only after text inputs and modal viewers have handled their keys.
fn back_action(
    key: i32,
    folder: i32,
    chat_open: bool,
    details_open: bool,
    overlay_open: bool,
) -> Option<BackAction> {
    if key != 27 && key != i32::from(b'q') {
        return None;
    }
    Some(if overlay_open {
        BackAction::CloseOverlay
    } else if details_open {
        BackAction::CloseDetails
    } else if chat_open {
        BackAction::CloseChat
    } else if folder != 0 {
        BackAction::Home
    } else {
        BackAction::Quit
    })
}

struct StderrRedirect {
    saved: RawFd,
    _log: File,
}

impl StderrRedirect {
    fn new(path: PathBuf) -> io::Result<Self> {
        let log = OpenOptions::new().create(true).append(true).open(path)?;
        let saved = unsafe { libc::dup(libc::STDERR_FILENO) };
        if saved < 0 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { libc::dup2(log.as_raw_fd(), libc::STDERR_FILENO) } < 0 {
            let error = io::Error::last_os_error();
            unsafe { libc::close(saved) };
            return Err(error);
        }
        Ok(Self { saved, _log: log })
    }
}

impl Drop for StderrRedirect {
    fn drop(&mut self) {
        unsafe {
            libc::dup2(self.saved, libc::STDERR_FILENO);
            libc::close(self.saved);
        }
    }
}

async fn open_stories(app: &mut App, terminal: &mut Terminal, windows: &mut WindowManager) {
    let Some(peer) = app.current_peer() else {
        return;
    };
    open_stories_peer(app, peer, terminal, windows).await;
}

async fn open_stories_peer(
    app: &mut App,
    peer: PeerRef,
    terminal: &mut Terminal,
    windows: &mut WindowManager,
) {
    open_stories_source(
        app,
        peer,
        terminal,
        windows,
        media::StorySource::Active,
        None,
    )
    .await;
}

async fn open_stories_source(
    app: &mut App,
    peer: PeerRef,
    terminal: &mut Terminal,
    windows: &mut WindowManager,
    source: media::StorySource,
    start_id: Option<i32>,
) {
    app.stop_audio();
    let mut backdrop = if app.tgp_supported {
        media::viewer_backdrop(terminal).ok()
    } else {
        None
    };
    let result = media::open_stories(
        &app.telegram.client,
        peer,
        &app.view_cache(),
        media::ViewerContext {
            terminal,
            windows,
            tgp_supported: app.tgp_supported,
            nerd_fonts: app.nerd_fonts,
        },
        source,
        start_id,
    )
    .await;
    if let Some(mut image) = backdrop.take() {
        let _ = image.delete(&mut terminal.stdout);
    }
    windows.invalidate();
    match result {
        Ok(outcome) if outcome.count == 0 => {
            app.status = if matches!(source, media::StorySource::Pinned) {
                "No profile posts"
            } else {
                "No available stories"
            }
            .into()
        }
        Ok(outcome) => {
            if !outcome.viewed.is_empty() {
                if let Some(entry) = app
                    .stories
                    .entries
                    .iter_mut()
                    .find(|entry| entry.peer.id == peer.id)
                {
                    entry.unread = false;
                }
                let client = app.telegram.client.clone();
                let tx = app.events_tx.clone();
                tokio::spawn(async move {
                    let max_id = outcome.viewed.iter().copied().max().unwrap_or_default();
                    let result = async {
                        client
                            .invoke(&tl::functions::stories::IncrementStoryViews {
                                peer: peer.into(),
                                id: outcome.viewed,
                            })
                            .await?;
                        client
                            .invoke(&tl::functions::stories::ReadStories {
                                peer: peer.into(),
                                max_id,
                            })
                            .await?;
                        Ok(())
                    }
                    .await
                    .map_err(|error: grammers_client::InvocationError| error.to_string());
                    let _ = tx.send(UiEvent::StoryRead(result));
                });
            }
            app.status = "Stories closed".into();
        }
        Err(error) => app.set_error(error),
    }
}

async fn open_media_at(
    index: usize,
    app: &mut App,
    terminal: &mut Terminal,
    windows: &mut WindowManager,
) -> Result<()> {
    let Some(message) = app.messages.get(index).cloned() else {
        return Ok(());
    };
    if message.media().is_none() {
        return Ok(());
    }
    if media::is_sticker(&message) {
        app.open_sticker_pack(&message);
        return Ok(());
    }
    if media::is_circle(&message) {
        app.toggle_audio(index);
        return Ok(());
    }
    if media::is_gif(&message) {
        return Ok(());
    }
    app.stop_audio();
    let mut backdrop = if app.tgp_supported {
        Some(media::viewer_backdrop(terminal)?)
    } else {
        None
    };
    let preview = app
        .previews
        .get(&message.id())
        .and_then(|state| match state {
            PreviewState::Ready(ImageAsset::Memory(image)) => Some(image.clone()),
            _ => None,
        });
    let view_cache = app.view_cache();
    let result = if media::opens_in_gallery(&message) {
        let messages = app.messages.clone();
        let peer = app.current_peer().context("no chat selected")?;
        media::open_gallery(
            &app.telegram.client,
            peer,
            &messages,
            index,
            &view_cache,
            preview,
            media::ViewerContext {
                terminal,
                windows,
                tgp_supported: app.tgp_supported,
                nerd_fonts: app.nerd_fonts,
            },
        )
        .await
        .map(|selected| {
            if let Some(index) = app
                .messages
                .iter()
                .position(|message| message.id() == selected)
            {
                app.message_index = index;
            }
            "Media viewer closed".to_owned()
        })
    } else {
        media::open(
            &app.telegram.client,
            &message,
            &view_cache,
            preview,
            media::ViewerContext {
                terminal,
                windows,
                tgp_supported: app.tgp_supported,
                nerd_fonts: app.nerd_fonts,
            },
        )
        .await
    };
    if let Some(backdrop) = &mut backdrop {
        backdrop.delete(&mut terminal.stdout)?;
        terminal.refresh()?;
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

async fn open_discussion_view(app: &mut App, full_chat: bool) {
    let (channel, post_id) = if let Some(view) = &app.discussion {
        (view.channel_peer, view.post_id)
    } else {
        let Some(channel) = app.current_chat().map(|chat| chat.peer) else {
            return;
        };
        let Some(post_id) = app.messages.get(app.message_index).map(Message::id) else {
            return;
        };
        (channel, post_id)
    };
    app.status = if full_chat {
        "Opening linked chat…"
    } else {
        "Loading comments…"
    }
    .to_owned();
    app.status_error = false;
    let result = if full_chat {
        Telegram::linked_discussion(&app.telegram.client, channel, post_id, HISTORY_PAGE).await
    } else {
        Telegram::discussion(&app.telegram.client, channel, post_id, HISTORY_PAGE, 0).await
    };
    match result {
        Ok(discussion) => app.enter_discussion(discussion, full_chat),
        Err(error) => app.set_error(error),
    }
}

struct App {
    telegram: Telegram,
    cache: PathBuf,
    avatar_cache: PathBuf,
    log_path: PathBuf,
    chats: Vec<Chat>,
    stories: stories::State,
    stories_rect: Option<Rect>,
    gifts: gifts::State,
    gifts_rect: Option<Rect>,
    folders: Vec<Folder>,
    active_folder: i32,
    sidebar_collapsed: bool,
    chat_index: usize,
    messages: Vec<Message>,
    message_index: usize,
    scroll_from_bottom: usize,
    open_chat: Option<usize>,
    discussion: Option<DiscussionView>,
    view_id: u64,
    read_outbox_max: i32,
    read_poll: Instant,
    read_poll_pending: bool,
    composing: bool,
    composer: composer::State,
    composer_width: usize,
    editing: Option<editing::State>,
    edit_previous_pending: bool,
    searching: bool,
    global_search_due: Option<Instant>,
    global_search_generation: u64,
    global_search_loading: bool,
    remote_results: HashSet<PeerId>,
    mention_request: u64,
    chat_search: Option<chat_search::State>,
    search_request: u64,
    attaching: bool,
    saving: Option<Message>,
    attachment_menu: bool,
    pending_attachments: Option<crate::clipboard::PendingAttachments>,
    clipboard_request: u64,
    sending_attachments: bool,
    attachment_kind: crate::telegram::AttachmentKind,
    attachment_directory: PathBuf,
    attachment_entries: Vec<PathBuf>,
    attachment_scroll: usize,
    menu_open: bool,
    theme: theme::Theme,
    theme_input: bool,
    theme_preview: Option<theme::Theme>,
    theme_request: u64,
    details: Option<Details>,
    details_generation: u64,
    details_rect: Option<Rect>,
    pins: Pins,
    has_newer_messages: bool,
    loading_newer: bool,
    settings_root: PathBuf,
    nerd_fonts: bool,
    message_menu: Option<MessageMenu>,
    reaction_state: reactions::State,
    forwarding: Option<Message>,
    reply_to: Option<i32>,
    replies: replies::State,
    forwards: forwarded::State,
    code_cache: std::cell::RefCell<code::Cache>,
    menu_overlay_cache: std::cell::RefCell<Option<(i32, media::MemoryImage)>>,
    emoji_state: emoji::State,
    draft_custom: Vec<crate::custom_emoji::Draft>,
    live_state: live::State,
    inbox_reads: reads::State,
    settings: settings::State,
    clicks: interaction::Clicks,
    deleting: Option<interaction::Delete>,
    input: String,
    search_query: String,
    status: String,
    status_error: bool,
    messages_loading: bool,
    loading_older: bool,
    has_more_messages: bool,
    hitboxes: Vec<HitBox>,
    previews: HashMap<i32, PreviewState>,
    preview_failures: HashMap<i32, (u32, Instant)>,
    custom_emoji: HashMap<i32, CustomEmojiState>,
    sidebar_emoji: HashMap<PeerId, SidebarEmoji>,
    compatible_reactions: Vec<Message>,
    avatars: HashMap<usize, AvatarState>,
    author_avatars: HashMap<PeerId, AvatarState>,
    audio: Option<AudioPlayback>,
    inline_video: Option<(i32, media::InlinePlayback)>,
    gifs: HashMap<i32, media::InlinePlayback>,
    gif_failures: HashSet<i32>,
    inline_images: InlineImages,
    events_tx: mpsc::UnboundedSender<UiEvent>,
    events_rx: mpsc::UnboundedReceiver<UiEvent>,
    preview_workers: Arc<Semaphore>,
    preview_tasks: tokio::task::JoinSet<()>,
    sticker_workers: Arc<Semaphore>,
    avatar_workers: Arc<Semaphore>,
    tgp_supported: bool,
}

struct DiscussionView {
    channel_peer: PeerRef,
    post_id: i32,
    root_id: i32,
    peer: PeerRef,
    peer_object: Peer,
    name: String,
    full_chat: bool,
    origin_messages: Vec<Message>,
    origin_previews: HashMap<i32, PreviewState>,
    origin_preview_failures: HashMap<i32, (u32, Instant)>,
    origin_custom_emoji: HashMap<i32, CustomEmojiState>,
    origin_message_index: usize,
    origin_scroll_from_bottom: usize,
    origin_has_more: bool,
    origin_has_newer: bool,
}

impl App {
    fn copy_code(&mut self, terminal: &mut Terminal, source: &str) {
        match copy_to_clipboard(terminal, source) {
            Ok(()) => {
                self.status = "Code copied".to_owned();
                self.status_error = false;
            }
            Err(error) => self.set_error(error),
        }
    }
    fn sidebar_is_compact(&self) -> bool {
        self.sidebar_collapsed
    }

    fn toggle_sidebar(&mut self) {
        self.sidebar_collapsed = !self.sidebar_is_compact();
        self.menu_open = false;
    }

    fn start_search(&mut self) {
        self.sidebar_collapsed = false;
        self.menu_open = false;
        self.searching = true;
        self.search_query.clear();
        self.search_changed();
    }

    fn browse_attachment(&mut self, path: PathBuf) {
        if path.is_dir() {
            self.attachment_directory = path;
            let mut entries: Vec<_> = fs::read_dir(&self.attachment_directory)
                .into_iter()
                .flatten()
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .collect();
            entries.sort_by_key(|path| {
                (
                    !path.is_dir(),
                    path.file_name().map(|name| name.to_ascii_lowercase()),
                )
            });
            if let Some(parent) = self.attachment_directory.parent() {
                entries.insert(0, parent.to_path_buf());
            }
            self.attachment_entries = entries;
            self.attachment_scroll = 0;
            self.input = format!("{}/", self.attachment_directory.display());
        } else {
            self.input = path.to_string_lossy().into_owned();
        }
    }
    fn request_theme(&mut self, source: String) {
        self.load_theme(source, false);
    }

    fn request_theme_preview(&mut self, source: String) {
        self.load_theme(source, true);
    }

    fn load_theme(&mut self, source: String, preview: bool) {
        self.theme_request = self.theme_request.wrapping_add(1);
        let request = self.theme_request;
        self.status = "Loading theme...".to_owned();
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        tokio::spawn(async move {
            let result = theme::load(&client, &source)
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(UiEvent::ThemeLoaded {
                request,
                preview,
                result,
            });
        });
    }

    fn visible_chat_indices(&self) -> Vec<usize> {
        let query = self.search_query.trim_start_matches('@').to_lowercase();
        self.chats
            .iter()
            .enumerate()
            .filter(|(_, chat)| {
                if !query.is_empty() {
                    !chat.search_only || self.remote_results.contains(&chat.peer.id)
                } else if chat.search_only {
                    false
                } else {
                    match self.active_folder {
                        0 => !chat.archived,
                        -1 => chat.archived,
                        folder => chat.folders.contains(&folder),
                    }
                }
            })
            .filter(|(_, chat)| {
                query.is_empty()
                    || self.remote_results.contains(&chat.peer.id)
                    || chat.name.to_lowercase().contains(&query)
                    || chat.last.to_lowercase().contains(&query)
                    || chat
                        .peer_object
                        .username()
                        .is_some_and(|name| name.to_lowercase().contains(&query))
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn switch_folder(&mut self, folder: i32) {
        self.active_folder = folder;
        self.menu_open = false;
        if let Some(index) = self.visible_chat_indices().first().copied() {
            self.chat_index = index;
        }
    }

    fn select_chat_previous(&mut self) {
        let visible = self.visible_chat_indices();
        if let Some(position) = visible.iter().position(|index| *index == self.chat_index) {
            self.chat_index = visible[position.saturating_sub(1)];
        }
    }

    fn select_chat_next(&mut self) {
        let visible = self.visible_chat_indices();
        if let Some(position) = visible.iter().position(|index| *index == self.chat_index) {
            self.chat_index = visible[(position + 1).min(visible.len().saturating_sub(1))];
        } else if let Some(index) = visible.first() {
            self.chat_index = *index;
        }
    }

    fn current_chat(&self) -> Option<&Chat> {
        self.open_chat
            .and_then(|index| self.chats.get(index))
            .or_else(|| self.chats.get(self.chat_index))
    }

    fn current_peer(&self) -> Option<PeerRef> {
        self.discussion
            .as_ref()
            .map(|view| view.peer)
            .or_else(|| self.current_chat().map(|chat| chat.peer))
    }

    fn reply_root(&self) -> Option<i32> {
        self.reply_to.or_else(|| {
            self.discussion
                .as_ref()
                .filter(|view| !view.full_chat)
                .map(|view| view.root_id)
        })
    }

    fn view_name(&self) -> &str {
        self.discussion
            .as_ref()
            .map(|view| view.name.as_str())
            .or_else(|| self.current_chat().map(|chat| chat.name.as_str()))
            .unwrap_or("Telegram")
    }

    fn view_presence(&self) -> &str {
        if let Some(view) = &self.discussion {
            if view.full_chat {
                "linked chat · read without joining"
            } else {
                "comments · g: linked chat"
            }
        } else {
            self.current_chat()
                .map(|chat| chat.presence.as_str())
                .unwrap_or("")
        }
    }

    fn open_selected_chat(&mut self) {
        if !self.chats.is_empty() {
            self.open_chat(self.chat_index);
        }
    }

    fn open_chat(&mut self, index: usize) {
        if index >= self.chats.len() {
            return;
        }
        self.chat_index = index;
        if self.open_chat == Some(index) && self.discussion.is_none() {
            return;
        }
        self.draft_custom.clear();
        self.input.clear();
        self.composing = false;
        self.editing = None;
        self.edit_previous_pending = false;
        self.chat_search = None;
        self.compatible_reactions.clear();
        self.close_profile_gifts();
        self.stop_audio();
        self.clear_view_cache();
        self.open_chat = Some(index);
        self.read_outbox_max = self.chats[index].read_outbox_max_id;
        self.read_poll = Instant::now() - Duration::from_secs(4);
        self.read_poll_pending = false;
        self.reaction_state = reactions::State::default();
        self.pending_attachments = None;
        self.gifs.clear();
        self.gif_failures.clear();
        self.close_details();
        self.pins = Pins::default();
        self.has_newer_messages = false;
        self.loading_newer = false;
        self.discussion = None;
        self.view_id = self.view_id.wrapping_add(1);
        self.messages.clear();
        self.previews.clear();
        self.preview_failures.clear();
        self.custom_emoji.clear();
        self.message_menu = None;
        self.forwarding = None;
        self.reply_to = None;
        self.message_index = 0;
        self.scroll_from_bottom = 0;
        self.has_more_messages = true;
        self.loading_older = false;
        self.request_messages(index, true);
        self.load_pins(true);
        self.request_reaction_catalog();
    }

    fn close_chat(&mut self) {
        if self.leave_discussion() {
            return;
        }
        self.draft_custom.clear();
        self.input.clear();
        self.composing = false;
        self.editing = None;
        self.edit_previous_pending = false;
        self.chat_search = None;
        self.compatible_reactions.clear();
        self.close_profile_gifts();
        self.stop_audio();
        self.clear_view_cache();
        self.view_id = self.view_id.wrapping_add(1);
        self.open_chat = None;
        self.reaction_state = reactions::State::default();
        self.pending_attachments = None;
        self.gifs.clear();
        self.gif_failures.clear();
        self.close_details();
        self.pins = Pins::default();
        self.messages.clear();
        self.previews.clear();
        self.preview_failures.clear();
        self.custom_emoji.clear();
        self.message_menu = None;
        self.forwarding = None;
        self.reply_to = None;
        self.messages_loading = false;
        self.status = "Ready".to_owned();
    }

    fn enter_discussion(&mut self, discussion: Discussion, full_chat: bool) {
        let origin_has_newer = self.has_newer_messages;
        self.draft_custom.clear();
        self.input.clear();
        self.composing = false;
        self.chat_search = None;
        self.editing = None;
        self.edit_previous_pending = false;
        self.reply_to = None;
        self.message_menu = None;
        self.forwarding = None;
        self.compatible_reactions.clear();
        self.read_outbox_max = 0;
        self.read_poll_pending = false;
        self.read_poll = Instant::now() - Duration::from_secs(4);
        self.reaction_state = reactions::State::default();
        self.pending_attachments = None;
        self.gifs.clear();
        self.gif_failures.clear();
        self.close_details();
        self.pins = Pins::default();
        self.has_newer_messages = false;
        self.loading_newer = false;
        let chat_index = self.open_chat;
        if let Some(view) = &mut self.discussion {
            view.root_id = discussion.root_id;
            view.peer = discussion.peer;
            view.peer_object = discussion.peer_object;
            view.name = discussion.name;
            view.full_chat = full_chat;
        } else {
            let Some(channel_peer) = self.current_chat().map(|chat| chat.peer) else {
                return;
            };
            self.discussion = Some(DiscussionView {
                channel_peer,
                post_id: discussion.post_id,
                root_id: discussion.root_id,
                peer: discussion.peer,
                peer_object: discussion.peer_object,
                name: discussion.name,
                full_chat,
                origin_messages: std::mem::take(&mut self.messages),
                origin_previews: std::mem::take(&mut self.previews),
                origin_preview_failures: std::mem::take(&mut self.preview_failures),
                origin_custom_emoji: std::mem::take(&mut self.custom_emoji),
                origin_message_index: self.message_index,
                origin_scroll_from_bottom: self.scroll_from_bottom,
                origin_has_more: self.has_more_messages,
                origin_has_newer,
            });
        }
        self.stop_audio();
        self.clear_view_cache();
        self.messages = discussion.messages;
        self.previews.clear();
        self.preview_failures.clear();
        self.custom_emoji.clear();
        self.message_index = self.messages.len().saturating_sub(1);
        self.scroll_from_bottom = 0;
        self.has_more_messages = discussion.has_more;
        self.loading_older = false;
        self.messages_loading = false;
        self.view_id = self.view_id.wrapping_add(1);
        self.status = "Ready".to_owned();
        self.status_error = false;
        if let Some(chat_index) = chat_index {
            self.queue_previews(chat_index);
        }
        self.load_pins(true);
        self.request_reaction_catalog();
    }

    fn leave_discussion(&mut self) -> bool {
        let Some(view) = self.discussion.take() else {
            return false;
        };
        self.draft_custom.clear();
        self.input.clear();
        self.composing = false;
        self.chat_search = None;
        self.editing = None;
        self.edit_previous_pending = false;
        self.reply_to = None;
        self.message_menu = None;
        self.forwarding = None;
        self.compatible_reactions.clear();
        self.pending_attachments = None;
        self.gifs.clear();
        self.gif_failures.clear();
        self.stop_audio();
        self.clear_view_cache();
        self.close_details();
        self.reaction_state = reactions::State::default();
        self.read_outbox_max = self.current_chat().map_or(0, |c| c.read_outbox_max_id);
        self.read_poll_pending = false;
        self.read_poll = Instant::now() - Duration::from_secs(4);
        self.pins = Pins::default();
        self.has_newer_messages = view.origin_has_newer;
        self.loading_newer = false;
        self.messages = view.origin_messages;
        self.previews = view.origin_previews;
        self.preview_failures = view.origin_preview_failures;
        self.custom_emoji = view.origin_custom_emoji;
        // Requests from the old view were cancelled and their results cannot
        // complete these Loading entries after returning from comments.
        self.previews
            .retain(|_, state| !matches!(state, PreviewState::Loading));
        self.custom_emoji
            .retain(|_, state| !matches!(state, CustomEmojiState::Loading));
        self.message_index = view.origin_message_index;
        self.scroll_from_bottom = view.origin_scroll_from_bottom;
        self.has_more_messages = view.origin_has_more;
        self.loading_older = false;
        self.messages_loading = false;
        self.view_id = self.view_id.wrapping_add(1);
        self.status = "Ready".to_owned();
        self.status_error = false;
        self.load_pins(true);
        self.request_reaction_catalog();
        if let Some(index) = self.open_chat {
            self.queue_previews(index);
        }
        true
    }

    fn request_messages(&mut self, chat_index: usize, clear: bool) {
        let Some(chat) = self.chats.get(chat_index) else {
            return;
        };
        if clear {
            self.messages.clear();
        }
        self.messages_loading = true;
        self.status = "Loading messages…".to_owned();
        self.status_error = false;
        let client = self.telegram.client.clone();
        let peer = chat.peer;
        let tx = self.events_tx.clone();
        let view_id = self.view_id;
        tokio::spawn(async move {
            let result = Telegram::messages_with(&client, peer, HISTORY_PAGE)
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(UiEvent::MessagesLoaded {
                chat_index,
                view_id,
                result,
            });
        });
    }

    fn poll_read_receipts(&mut self) {
        if self.open_chat.is_none()
            || self.read_poll_pending
            || self.read_poll.elapsed() < Duration::from_secs(3)
        {
            return;
        }
        let Some(peer) = self.current_peer() else {
            return;
        };
        self.read_poll_pending = true;
        self.read_poll = Instant::now();
        let view_id = self.view_id;
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        tokio::spawn(async move {
            let result = async {
                let dialogs = client
                    .invoke(&tl::functions::messages::GetPeerDialogs {
                        peers: vec![tl::types::InputDialogPeer { peer: peer.into() }.into()],
                    })
                    .await?;
                let tl::enums::messages::PeerDialogs::Dialogs(dialogs) = dialogs;
                Ok::<_, anyhow::Error>(
                    dialogs
                        .dialogs
                        .iter()
                        .find_map(|dialog| match dialog {
                            tl::enums::Dialog::Dialog(dialog) => Some(reads::DialogReadState {
                                outbox: dialog.read_outbox_max_id,
                                inbox: dialog.read_inbox_max_id,
                                unread: dialog.unread_count,
                            }),
                            _ => None,
                        })
                        .unwrap_or_default(),
                )
            }
            .await
            .map_err(|e| e.to_string());
            let _ = tx.send(UiEvent::ReadReceipt { view_id, result });
        });
    }

    fn process_events(&mut self) -> bool {
        let mut changed = false;
        while let Ok(event) = self.events_rx.try_recv() {
            changed = true;
            match event {
                UiEvent::InboxReadSent(event) => self.inbox_read_sent(event),
                UiEvent::ForwardSource(event) => self.forward_source_event(event),
                UiEvent::Language(event) => self.language_event(event),
                UiEvent::Gifts(event) => self.gift_event(event),
                UiEvent::GlobalSearchReady { generation, result } => {
                    self.global_search_event(generation, result);
                }
                UiEvent::MentionResolved { generation, result }
                    if generation == self.mention_request =>
                {
                    match *result {
                        Ok(Some(chat)) => self.open_remote_peer(chat),
                        Ok(None) => self.status = "Username not found".to_owned(),
                        Err(error) => self.set_error(error),
                    }
                }
                UiEvent::Live(event) => self.live_event(event),
                UiEvent::Deleted(event) => self.deleted_event(event),
                UiEvent::CompatibleReaction(event) => self.compatible_reaction_event(event),
                UiEvent::ReadReceipt { view_id, result } if view_id == self.view_id => {
                    self.read_poll_pending = false;
                    match result {
                        Ok(state) => {
                            self.read_outbox_max = self.read_outbox_max.max(state.outbox);
                            if let Some(peer) = self.current_peer() {
                                self.apply_inbox_update(peer.id, state.inbox, state.unread);
                            }
                        }
                        Err(error) => self.log_error(error),
                    }
                }
                UiEvent::Reactions(event) => self.reactions_event(event),
                UiEvent::Paste(event) => self.paste_event(event),
                UiEvent::Details(event) => self.details_event(event),
                UiEvent::Replies(event) => self.replies_event(event),
                UiEvent::Emoji(event) => self.emoji_event(event),
                UiEvent::ThemeLoaded {
                    request,
                    preview,
                    result,
                } if request == self.theme_request => match result {
                    Ok(theme) if preview => {
                        self.theme_preview = Some(theme);
                        self.message_menu = None;
                        self.status = "Theme preview · Enter apply · Esc cancel".to_owned();
                        self.status_error = false;
                    }
                    Ok(theme) => {
                        self.theme = theme;
                        if let Err(error) = self.theme.save(&self.settings_root) {
                            self.set_error(error);
                        } else {
                            self.status = format!("Theme: {}", self.theme.name);
                            self.status_error = false;
                        }
                    }
                    Err(error) => self.set_error(error),
                },
                UiEvent::MessagesLoaded {
                    chat_index,
                    view_id,
                    result,
                } if self.open_chat == Some(chat_index) && self.view_id == view_id => {
                    self.messages_loading = false;
                    match result {
                        Ok(messages) => {
                            self.messages = messages;
                            self.has_more_messages = self.messages.len() == HISTORY_PAGE;
                            self.message_index = self.messages.len().saturating_sub(1);
                            self.scroll_from_bottom = 0;
                            self.status = "Ready".to_owned();
                            self.status_error = false;
                            self.queue_previews(chat_index);
                        }
                        Err(error) => self.set_error(error),
                    }
                    self.apply_pending_live();
                }
                UiEvent::PreviewReady {
                    chat_index,
                    view_id,
                    message_id,
                    result,
                } if self.open_chat == Some(chat_index) && self.view_id == view_id => {
                    let failed = result.is_err();
                    if let Err(error) = &result {
                        self.log_error(error);
                        // A progressively decoded sticker already has a usable
                        // frame. Do not make it disappear if later frames fail.
                        if matches!(self.previews.get(&message_id), Some(PreviewState::Ready(_))) {
                            self.queue_previews_near(chat_index);
                            continue;
                        }
                    }
                    self.previews.insert(
                        message_id,
                        match result {
                            Ok(asset) => PreviewState::Ready(asset),
                            Err(_) => PreviewState::Failed,
                        },
                    );
                    if failed {
                        let failure = self
                            .preview_failures
                            .entry(message_id)
                            .or_insert((0, Instant::now()));
                        failure.0 = failure.0.saturating_add(1);
                        failure.1 = Instant::now() + preview_retry_delay(failure.0);
                    } else {
                        self.preview_failures.remove(&message_id);
                    }
                    self.queue_previews_near(chat_index);
                }
                UiEvent::CustomEmojiReady {
                    chat_index,
                    view_id,
                    message_id,
                    result,
                } if self.open_chat == Some(chat_index) && self.view_id == view_id => {
                    if let Err(error) = &result {
                        self.log_error(error);
                    }
                    self.custom_emoji.insert(
                        message_id,
                        match result {
                            Ok(images) => CustomEmojiState::Ready(images),
                            Err(_) => CustomEmojiState::Failed,
                        },
                    );
                    self.queue_previews_near(chat_index);
                }
                UiEvent::AudioReady {
                    chat_index,
                    view_id,
                    message_id,
                    result,
                } if self.open_chat == Some(chat_index) && self.view_id == view_id => {
                    if self
                        .audio
                        .as_ref()
                        .is_some_and(|audio| audio.message_id == message_id)
                    {
                        match result {
                            Ok(clip) => {
                                let requested_ratio = self
                                    .audio
                                    .as_ref()
                                    .map(|audio| audio.requested_ratio)
                                    .unwrap_or_default();
                                let position = clip.duration * requested_ratio.clamp(0.0, 1.0);
                                match media::spawn_audio_at(&clip.path, position) {
                                    Ok(child) => {
                                        self.audio = Some(AudioPlayback {
                                            message_id,
                                            child: Some(child),
                                            path: Some(clip.path),
                                            duration: clip.duration,
                                            position,
                                            started: Instant::now(),
                                            paused: false,
                                            loading: false,
                                            requested_ratio,
                                            last_tick: Instant::now(),
                                        });
                                    }
                                    Err(error) => {
                                        self.audio = None;
                                        self.set_error(error);
                                    }
                                }
                            }
                            Err(error) => {
                                self.audio = None;
                                self.set_error(error);
                            }
                        }
                    }
                }
                UiEvent::OlderMessages {
                    chat_index,
                    view_id,
                    result,
                } if self.open_chat == Some(chat_index) && self.view_id == view_id => {
                    self.loading_older = false;
                    match result {
                        Ok(mut page) => {
                            self.has_more_messages = page.has_more;
                            let mut known: HashSet<_> =
                                self.messages.iter().map(Message::id).collect();
                            page.messages.retain(|message| known.insert(message.id()));
                            let added = page.messages.len();
                            page.messages.append(&mut self.messages);
                            self.messages = page.messages;
                            self.message_index = self.message_index.saturating_add(added);
                            self.status = "Ready".to_owned();
                            self.queue_previews(chat_index);
                        }
                        Err(error) => self.set_error(error),
                    }
                }
                UiEvent::EditPrevious { view_id, result } if view_id == self.view_id => {
                    self.edit_previous_pending = false;
                    match result {
                        Ok(Some(message)) if self.composing && self.editing.is_none() => {
                            self.begin_edit(*message)
                        }
                        Ok(None) => self.status = "No previous editable message".into(),
                        Err(error) => self.log_error(&error),
                        _ => {}
                    }
                }
                UiEvent::ChatSearchResults {
                    view_id,
                    request,
                    append,
                    result,
                } => {
                    self.chat_search_event(view_id, request, append, result);
                }
                UiEvent::StoriesLoaded(result) => self.stories_event(result),
                UiEvent::StoryRead(Err(error)) => self.log_error(error),
                UiEvent::AvatarReady { chat_index, result } => {
                    if let Err(error) = &result {
                        self.log_error(error);
                    }
                    self.avatars.insert(
                        chat_index,
                        match result {
                            Ok(Some(avatar)) => AvatarState::Ready(avatar),
                            Ok(None) => AvatarState::Missing,
                            Err(_) => AvatarState::Failed,
                        },
                    );
                }
                UiEvent::AuthorAvatarReady { peer, result } => {
                    if let Err(error) = &result {
                        self.log_error(error);
                    }
                    self.author_avatars.insert(
                        peer,
                        match result {
                            Ok(Some(avatar)) => AvatarState::Ready(avatar),
                            Ok(None) => AvatarState::Missing,
                            Err(_) => AvatarState::Failed,
                        },
                    );
                }
                UiEvent::AuthorResolved(result) => match *result {
                    Ok(Some(chat)) => self.open_remote_peer(chat),
                    Ok(None) => self.status = "User is unavailable".into(),
                    Err(error) => self.set_error(error),
                },
                UiEvent::SidebarEmojiReady {
                    peer,
                    message_id,
                    result,
                } => {
                    if self.chats.iter().any(|chat| {
                        chat.peer.id == peer
                            && chat
                                .last_message
                                .as_ref()
                                .is_some_and(|m| m.id() == message_id)
                    }) && self
                        .sidebar_emoji
                        .get(&peer)
                        .is_some_and(|entry| entry.message_id == message_id)
                    {
                        let state = match result {
                            Ok(images) => CustomEmojiState::Ready(images),
                            Err(error) => {
                                self.log_error(error);
                                CustomEmojiState::Failed
                            }
                        };
                        if let Some(entry) = self.sidebar_emoji.get_mut(&peer) {
                            entry.state = state;
                        }
                    }
                }
                UiEvent::ChatsRefreshed {
                    result: Ok((fresh_chats, folders)),
                } => {
                    for chat in &mut self.chats {
                        if let Some(fresh) = fresh_chats
                            .iter()
                            .find(|fresh| fresh.peer.id == chat.peer.id)
                        {
                            chat.unread = self.inbox_reads.refreshed_unread(chat, fresh);
                            if fresh.last_message.as_ref().map(Message::id)
                                >= chat.last_message.as_ref().map(Message::id)
                            {
                                chat.last.clone_from(&fresh.last);
                                chat.last_message.clone_from(&fresh.last_message);
                            }
                            chat.presence.clone_from(&fresh.presence);
                            chat.archived = fresh.archived;
                            chat.read_inbox_max_id =
                                chat.read_inbox_max_id.max(fresh.read_inbox_max_id);
                            chat.read_outbox_max_id = fresh.read_outbox_max_id;
                            chat.folders.clone_from(&fresh.folders);
                            chat.search_only = false;
                        }
                    }
                    self.folders = folders;
                    if !self.pins.loading {
                        self.load_pins(true);
                    }
                }
                UiEvent::AudioReady {
                    result: Ok(clip), ..
                } => {
                    let _ = fs::remove_file(clip.path);
                }
                _ => {}
            }
        }
        changed
    }

    fn start_chat_refresh(&self) {
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
            interval.tick().await;
            loop {
                interval.tick().await;
                let result = Telegram::chats_with(&client, 500)
                    .await
                    .map_err(|error| error.to_string());
                if tx.send(UiEvent::ChatsRefreshed { result }).is_err() {
                    break;
                }
            }
        });
    }

    fn queue_previews(&mut self, chat_index: usize) {
        self.queue_previews_inner(chat_index, true);
    }

    fn queue_previews_near(&mut self, chat_index: usize) {
        self.queue_previews_inner(chat_index, false);
    }

    fn queue_previews_inner(&mut self, chat_index: usize, scan_compatibility: bool) {
        while self.preview_tasks.try_join_next().is_some() {}
        if scan_compatibility {
            let compatible: Vec<_> = self
                .messages
                .iter()
                .filter(|message| compat::record(message).is_some())
                .cloned()
                .collect();
            for message in compatible {
                self.remember_compatible(message);
            }
        }
        if !self.tgp_supported {
            return;
        }
        // Keep work close to the viewport. Queuing an entire long history
        // made an image reached by scrolling wait behind hundreds of downloads.
        let nearby = preview_message_indices(
            self.messages.len(),
            self.scroll_from_bottom,
            self.message_index,
        );
        let jobs: Vec<_> = nearby
            .iter()
            .map(|index| &self.messages[*index])
            .filter(|message| media::has_inline_preview(message))
            .map(|message| (message.id(), message.clone()))
            .collect();
        for (message_id, message) in jobs {
            if matches!(
                self.previews.get(&message_id),
                Some(PreviewState::Loading | PreviewState::Ready(_))
            ) || self
                .preview_failures
                .get(&message_id)
                .is_some_and(|(_, retry_at)| *retry_at > Instant::now())
            {
                continue;
            }
            let workers = if media::is_sticker(&message) {
                self.sticker_workers.clone()
            } else {
                self.preview_workers.clone()
            };
            let Ok(permit) = workers.try_acquire_owned() else {
                continue;
            };
            self.previews.insert(message_id, PreviewState::Loading);
            let client = self.telegram.client.clone();
            let cache = self.view_cache();
            let tx = self.events_tx.clone();
            let view_id = self.view_id;
            self.preview_tasks.spawn(async move {
                let _permit = permit;
                let result = if media::is_sticker(&message) {
                    tokio::time::timeout(
                        Duration::from_secs(30),
                        media::prepare_sticker(client, message, cache, {
                            let tx = tx.clone();
                            move |image| {
                                let _ = tx.send(UiEvent::PreviewReady {
                                    chat_index,
                                    view_id,
                                    message_id,
                                    result: Ok(ImageAsset::Memory(image)),
                                });
                            }
                        }),
                    )
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("sticker preview timed out")))
                    .map(ImageAsset::Animated)
                } else {
                    tokio::time::timeout(
                        Duration::from_secs(30),
                        media::prepare_inline_preview(client, message, cache),
                    )
                    .await
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("image preview timed out")))
                    .map(ImageAsset::Memory)
                }
                .map_err(|error| error.to_string());
                drop(_permit);
                let _ = tx.send(UiEvent::PreviewReady {
                    chat_index,
                    view_id,
                    message_id,
                    result,
                });
            });
        }

        let emoji_jobs: Vec<_> = nearby
            .iter()
            .map(|index| &self.messages[*index])
            .filter(|message| media::has_custom_emoji(message))
            .map(|message| (message.id(), message.clone()))
            .collect();
        for (message_id, message) in emoji_jobs {
            if self.custom_emoji.contains_key(&message_id) {
                continue;
            }
            let Ok(permit) = self.sticker_workers.clone().try_acquire_owned() else {
                continue;
            };
            self.custom_emoji
                .insert(message_id, CustomEmojiState::Loading);
            let client = self.telegram.client.clone();
            let cache = self.view_cache();
            let tx = self.events_tx.clone();
            let view_id = self.view_id;
            self.preview_tasks.spawn(async move {
                let _permit = permit;
                let result = tokio::time::timeout(
                    Duration::from_secs(30),
                    media::prepare_custom_emoji(client, message, cache),
                )
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!("custom emoji preview timed out")))
                .map_err(|error| error.to_string());
                drop(_permit);
                let _ = tx.send(UiEvent::CustomEmojiReady {
                    chat_index,
                    view_id,
                    message_id,
                    result,
                });
            });
        }
    }

    fn queue_sidebar_emoji(&mut self, hitboxes: &[HitBox]) {
        if !self.tgp_supported {
            return;
        }
        let visible: HashSet<_> = hitboxes
            .iter()
            .filter_map(|hit| match hit.target {
                HitTarget::Chat(index) => self.chats.get(index).map(|chat| chat.peer.id),
                _ => None,
            })
            .collect();
        self.sidebar_emoji.retain(|peer, _| visible.contains(peer));
        let jobs: Vec<_> = self
            .chats
            .iter()
            .filter(|chat| visible.contains(&chat.peer.id))
            .filter_map(|chat| {
                chat.last_message
                    .as_ref()
                    .filter(|message| media::has_custom_emoji(message))
                    .map(|message| (chat.peer.id, message.clone()))
            })
            .collect();
        for (peer, message) in jobs {
            if self
                .sidebar_emoji
                .get(&peer)
                .is_some_and(|entry| entry.message_id == message.id())
            {
                continue;
            }
            let message_id = message.id();
            self.sidebar_emoji.insert(
                peer,
                SidebarEmoji {
                    message_id,
                    state: CustomEmojiState::Loading,
                },
            );
            let client = self.telegram.client.clone();
            let cache = self
                .cache
                .join("sidebar-emoji")
                .join(peer.bot_api_dialog_id().unwrap_or_default().to_string());
            let workers = self.sticker_workers.clone();
            let tx = self.events_tx.clone();
            tokio::spawn(async move {
                let result = match workers.acquire_owned().await {
                    Ok(_permit) => media::prepare_custom_emoji(client, message, cache)
                        .await
                        .map_err(|error| error.to_string()),
                    Err(_) => Err("sidebar emoji worker stopped".into()),
                };
                let _ = tx.send(UiEvent::SidebarEmojiReady {
                    peer,
                    message_id,
                    result,
                });
            });
        }
    }

    fn queue_avatars(&mut self) {
        if !self.tgp_supported {
            return;
        }
        for (chat_index, chat) in self.chats.iter().enumerate() {
            if self.avatars.contains_key(&chat_index) {
                continue;
            }
            self.avatars.insert(chat_index, AvatarState::Loading);
            let client = self.telegram.client.clone();
            let peer = chat.peer_object.clone();
            let cache = self.avatar_cache.clone();
            let tx = self.events_tx.clone();
            let workers = self.avatar_workers.clone();
            tokio::spawn(async move {
                let result = match workers.acquire_owned().await {
                    Ok(_permit) => media::prepare_avatar(client, peer, cache)
                        .await
                        .map_err(|error| error.to_string()),
                    Err(_) => Err("avatar worker stopped".to_owned()),
                };
                let _ = tx.send(UiEvent::AvatarReady { chat_index, result });
            });
        }
    }

    fn queue_author_avatars(&mut self, hits: &[HitBox]) {
        if !self.tgp_supported {
            return;
        }
        let authors: Vec<_> = hits
            .iter()
            .filter_map(|hit| {
                if let HitTarget::MessageAuthor(peer) = &hit.target {
                    Some((**peer).clone())
                } else {
                    None
                }
            })
            .collect();
        for peer in authors {
            let id = peer.id();
            if self.author_avatars.contains_key(&id) {
                continue;
            }
            self.author_avatars.insert(id, AvatarState::Loading);
            let client = self.telegram.client.clone();
            let cache = self.avatar_cache.clone();
            let tx = self.events_tx.clone();
            let workers = self.avatar_workers.clone();
            tokio::spawn(async move {
                let result = match workers.acquire_owned().await {
                    Ok(_permit) => media::prepare_avatar(client, peer, cache)
                        .await
                        .map_err(|error| error.to_string()),
                    Err(_) => Err("avatar worker stopped".to_owned()),
                };
                let _ = tx.send(UiEvent::AuthorAvatarReady { peer: id, result });
            });
        }
    }

    fn open_message_author(&mut self, peer: Box<Peer>) {
        let tx = self.events_tx.clone();
        tokio::spawn(async move {
            let result = peer
                .to_ref()
                .await
                .map(|reference| {
                    reference.map(|reference| Chat::from_search_peer(*peer, reference))
                })
                .map_err(|error| error.to_string());
            let _ = tx.send(UiEvent::AuthorResolved(Box::new(result)));
        });
    }

    fn select_previous(&mut self) {
        self.message_menu = None;
        if self.open_chat.is_some() {
            self.message_index = self.message_index.saturating_sub(1);
            self.keep_selected_visible();
            if let Some(index) = self.open_chat {
                self.queue_previews_near(index);
            }
            if self.message_index <= 8 {
                self.request_older_messages();
            }
        } else {
            self.select_chat_previous();
        }
    }

    fn select_next(&mut self) {
        self.message_menu = None;
        if self.open_chat.is_some() {
            self.message_index =
                (self.message_index + 1).min(self.messages.len().saturating_sub(1));
            self.keep_selected_visible();
            if let Some(index) = self.open_chat {
                self.queue_previews_near(index);
            }
            if self.message_index + 8 >= self.messages.len() {
                self.load_newer();
            }
        } else {
            self.select_chat_next();
        }
    }

    fn select_message(&mut self, index: usize) {
        if index < self.messages.len() {
            self.message_index = index;
        }
    }

    fn keep_selected_visible(&mut self) {
        self.scroll_from_bottom = self
            .messages
            .len()
            .saturating_sub(self.message_index.saturating_add(1));
    }

    fn scroll_messages_up(&mut self) {
        self.message_menu = None;
        if self.messages.len() > 1 {
            self.scroll_from_bottom =
                (self.scroll_from_bottom + 2).min(self.messages.len().saturating_sub(1));
        }
        if self.scroll_from_bottom + 6 >= self.messages.len() {
            self.request_older_messages();
        }
        if let Some(index) = self.open_chat {
            self.queue_previews_near(index);
        }
    }

    fn scroll_messages_down(&mut self) {
        self.message_menu = None;
        self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(2);
        if self.scroll_from_bottom <= 6 {
            self.load_newer();
        }
        if let Some(index) = self.open_chat {
            self.queue_previews_near(index);
        }
    }

    fn request_older_messages(&mut self) {
        if self.loading_older || !self.has_more_messages || self.messages.is_empty() {
            return;
        }
        let Some(chat_index) = self.open_chat else {
            return;
        };
        self.loading_older = true;
        self.status = "Loading older messages…".to_owned();
        let client = self.telegram.client.clone();
        let offset_id = self.messages[0].id();
        let tx = self.events_tx.clone();
        let view_id = self.view_id;
        let request = if let Some(view) = &self.discussion {
            if view.full_chat {
                OlderRequest::History { peer: view.peer }
            } else {
                OlderRequest::Comments {
                    channel: view.channel_peer,
                    post_id: view.post_id,
                }
            }
        } else {
            let Some(chat) = self.chats.get(chat_index) else {
                return;
            };
            OlderRequest::History { peer: chat.peer }
        };
        tokio::spawn(async move {
            let result = match request {
                OlderRequest::History { peer } => {
                    Telegram::messages_before(&client, peer, HISTORY_PAGE, offset_id)
                        .await
                        .map(|messages| OlderPage {
                            has_more: messages.len() == HISTORY_PAGE,
                            messages,
                        })
                }
                OlderRequest::Comments { channel, post_id } => {
                    Telegram::discussion(&client, channel, post_id, HISTORY_PAGE, offset_id)
                        .await
                        .map(|discussion| OlderPage {
                            messages: discussion.messages,
                            has_more: discussion.has_more,
                        })
                }
            }
            .map_err(|error| error.to_string());
            let _ = tx.send(UiEvent::OlderMessages {
                chat_index,
                view_id,
                result,
            });
        });
    }

    fn message_index_by_id(&self, message_id: i32) -> Option<usize> {
        self.messages
            .iter()
            .position(|message| message.id() == message_id)
    }

    fn open_message_menu(&mut self, index: usize, x: u16, y: u16) {
        let Some(message_id) = self.messages.get(index).map(Message::id) else {
            return;
        };
        self.message_menu = Some(MessageMenu {
            message_id,
            anchor_x: i32::from(x),
            anchor_y: i32::from(y),
        });
        self.menu_overlay_cache.borrow_mut().take();
        self.request_reaction_catalog();
    }

    fn remove_message(&mut self, index: usize) {
        if index >= self.messages.len() {
            return;
        }
        let message_id = self.messages[index].id();
        self.messages.remove(index);
        self.pins
            .messages
            .retain(|message| message.id() != message_id);
        self.pins.cursor = self
            .pins
            .cursor
            .min(self.pins.messages.len().saturating_sub(1));
        self.previews.remove(&message_id);
        self.custom_emoji.remove(&message_id);
        self.message_index = self
            .message_index
            .min(self.messages.len().saturating_sub(1));
        self.status = "Message deleted".to_owned();
        self.status_error = false;
    }

    fn start_audio_message(&mut self, message: Message, requested_ratio: f64) {
        let Some(chat_index) = self.open_chat else {
            return;
        };
        self.stop_audio();
        let message_id = message.id();
        self.audio = Some(AudioPlayback {
            message_id,
            child: None,
            path: None,
            duration: 0.0,
            position: 0.0,
            started: Instant::now(),
            paused: false,
            loading: true,
            requested_ratio: requested_ratio.clamp(0.0, 1.0),
            last_tick: Instant::now(),
        });
        let client = self.telegram.client.clone();
        let cache = self.view_cache();
        let tx = self.events_tx.clone();
        let view_id = self.view_id;
        tokio::spawn(async move {
            let result = media::prepare_audio(client, message, cache)
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(UiEvent::AudioReady {
                chat_index,
                view_id,
                message_id,
                result,
            });
        });
    }

    fn toggle_audio(&mut self, index: usize) {
        let Some(message) = self.messages.get(index).cloned() else {
            return;
        };
        self.toggle_audio_message(&message);
    }

    fn toggle_audio_message(&mut self, message: &Message) {
        if media::is_circle(message) && self.tgp_supported {
            self.toggle_circle(message);
            return;
        }
        let message_id = message.id();
        if self
            .audio
            .as_ref()
            .is_none_or(|audio| audio.message_id != message_id)
        {
            self.start_audio_message(message.clone(), 0.0);
            return;
        }
        let Some(audio) = self.audio.as_mut() else {
            return;
        };
        if audio.loading {
            return;
        }
        if !audio.paused {
            audio.position = audio.current_position();
        }
        audio.paused = !audio.paused;
        audio.started = Instant::now();
        if let Some(child) = &audio.child {
            media::set_audio_paused(child, audio.paused);
        }
    }

    fn seek_audio(&mut self, index: usize, ratio: f64) {
        let Some(message) = self.messages.get(index).cloned() else {
            return;
        };
        self.seek_audio_message(&message, ratio);
    }

    fn seek_audio_message(&mut self, message: &Message, ratio: f64) {
        if media::is_circle(message) && self.tgp_supported {
            if let Some((id, playback)) = &mut self.inline_video
                && *id == message.id()
            {
                if let Err(error) = playback.seek(ratio) {
                    self.set_error(error);
                }
            } else {
                self.start_circle(message, ratio);
            }
            return;
        }
        let message_id = message.id();
        let ratio = ratio.clamp(0.0, 1.0);
        if self
            .audio
            .as_ref()
            .is_none_or(|audio| audio.message_id != message_id)
        {
            self.start_audio_message(message.clone(), ratio);
            return;
        }
        let Some(audio) = self.audio.as_mut() else {
            return;
        };
        audio.requested_ratio = ratio;
        if audio.loading {
            return;
        }
        let Some(path) = audio.path.clone() else {
            return;
        };
        if let Some(child) = audio.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let position = audio.duration * ratio;
        match media::spawn_audio_at(&path, position) {
            Ok(child) => {
                audio.child = Some(child);
                audio.position = position;
                audio.started = Instant::now();
                audio.paused = false;
            }
            Err(error) => {
                self.stop_audio();
                self.set_error(error);
            }
        }
    }

    fn audio_snapshot(&self, message_id: i32) -> AudioSnapshot {
        if let Some((id, playback)) = &self.inline_video
            && *id == message_id
        {
            return AudioSnapshot {
                active: !playback.ended,
                loading: playback.frame.is_none(),
                paused: playback.paused,
                position: playback.position,
                duration: playback.duration,
            };
        }
        let Some(audio) = self
            .audio
            .as_ref()
            .filter(|audio| audio.message_id == message_id)
        else {
            return AudioSnapshot::default();
        };
        AudioSnapshot {
            active: true,
            loading: audio.loading,
            paused: audio.paused,
            position: audio.current_position(),
            duration: audio.duration,
        }
    }

    fn tick_audio(&mut self) -> bool {
        let Some(audio) = self.audio.as_mut() else {
            return false;
        };
        if audio.loading || audio.last_tick.elapsed() < Duration::from_millis(120) {
            return false;
        }
        audio.last_tick = Instant::now();
        let finished = audio
            .child
            .as_mut()
            .and_then(|child| child.try_wait().ok().flatten())
            .is_some();
        if finished {
            self.stop_audio();
        }
        true
    }

    fn stop_audio(&mut self) {
        self.inline_video = None;
        self.audio = None;
    }

    fn hit_test(&self, x: u16, y: u16) -> Option<HitTarget> {
        self.hitboxes
            .iter()
            .rev()
            .find(|hitbox| hitbox.rect.contains(i32::from(x), i32::from(y)))
            .map(|hitbox| hitbox.target.clone())
    }

    fn set_error(&mut self, error: impl std::fmt::Display) {
        self.log_error(error);
        self.status = "Operation failed · see telega.log".to_owned();
        self.status_error = true;
    }

    fn log_error(&self, error: impl std::fmt::Display) {
        if let Ok(mut log) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
        {
            let _ = writeln!(log, "{error}");
        }
    }

    fn view_cache(&self) -> PathBuf {
        self.cache.join(format!("view-{}", self.view_id))
    }

    fn clear_view_cache(&mut self) {
        // Drop aborts active downloads and releases their worker permits. Old
        // chats must not keep a queue of work ahead of the newly opened one.
        self.preview_tasks = tokio::task::JoinSet::new();
        let _ = fs::remove_dir_all(self.view_cache());
    }
}

enum UiEvent {
    InboxReadSent(reads::Event),
    ForwardSource(forwarded::Event),
    Language(settings::Event),
    Gifts(gifts::Event),
    GlobalSearchReady {
        generation: u64,
        result: std::result::Result<Vec<Chat>, String>,
    },
    MentionResolved {
        generation: u64,
        result: Box<std::result::Result<Option<Chat>, String>>,
    },
    CompatibleReaction(compat::Event),
    Deleted(interaction::Deleted),
    Live(live::Event),
    Emoji(emoji::Event),
    Replies(replies::Event),
    ReadReceipt {
        view_id: u64,
        result: std::result::Result<reads::DialogReadState, String>,
    },
    Reactions(reactions::Event),
    Paste(paste::PasteEvent),
    ThemeLoaded {
        request: u64,
        preview: bool,
        result: std::result::Result<theme::Theme, String>,
    },
    Details(DetailsEvent),
    MessagesLoaded {
        chat_index: usize,
        view_id: u64,
        result: std::result::Result<Vec<Message>, String>,
    },
    PreviewReady {
        chat_index: usize,
        view_id: u64,
        message_id: i32,
        result: std::result::Result<ImageAsset, String>,
    },
    CustomEmojiReady {
        chat_index: usize,
        view_id: u64,
        message_id: i32,
        result: std::result::Result<Vec<Option<media::StickerFrames>>, String>,
    },
    SidebarEmojiReady {
        peer: PeerId,
        message_id: i32,
        result: std::result::Result<Vec<Option<media::StickerFrames>>, String>,
    },
    AudioReady {
        chat_index: usize,
        view_id: u64,
        message_id: i32,
        result: std::result::Result<media::AudioClip, String>,
    },
    OlderMessages {
        chat_index: usize,
        view_id: u64,
        result: std::result::Result<OlderPage, String>,
    },
    AvatarReady {
        chat_index: usize,
        result: std::result::Result<Option<media::Avatar>, String>,
    },
    AuthorAvatarReady {
        peer: PeerId,
        result: std::result::Result<Option<media::Avatar>, String>,
    },
    AuthorResolved(Box<std::result::Result<Option<Chat>, String>>),
    ChatsRefreshed {
        result: std::result::Result<(Vec<Chat>, Vec<Folder>), String>,
    },
    EditPrevious {
        view_id: u64,
        result: std::result::Result<Option<Box<Message>>, String>,
    },
    ChatSearchResults {
        view_id: u64,
        request: u64,
        append: bool,
        result: std::result::Result<(Vec<Message>, bool), String>,
    },
    StoriesLoaded(std::result::Result<Vec<stories::Entry>, String>),
    StoryRead(std::result::Result<(), String>),
}

struct OlderPage {
    messages: Vec<Message>,
    has_more: bool,
}

enum OlderRequest {
    History { peer: PeerRef },
    Comments { channel: PeerRef, post_id: i32 },
}

enum PreviewState {
    Loading,
    Ready(ImageAsset),
    Failed,
}

fn preview_retry_delay(attempts: u32) -> Duration {
    Duration::from_secs((1u64 << attempts.min(5)).min(30))
}

fn preview_message_indices(count: usize, scroll_from_bottom: usize, selected: usize) -> Vec<usize> {
    if count == 0 {
        return Vec::new();
    }
    let visible = count.saturating_sub(scroll_from_bottom.saturating_add(1));
    let selected = selected.min(count - 1);
    let viewport = visible.saturating_sub(20)..visible.saturating_add(21).min(count);
    let selection = selected.saturating_sub(12)..selected.saturating_add(13).min(count);
    let mut indices: Vec<_> = viewport
        .clone()
        .chain(selection.filter(|i| !viewport.contains(i)))
        .collect();
    indices.sort_by_key(|index| index.abs_diff(visible));
    indices
}

enum CustomEmojiState {
    Loading,
    Ready(Vec<Option<media::StickerFrames>>),
    Failed,
}

struct SidebarEmoji {
    message_id: i32,
    state: CustomEmojiState,
}

enum AvatarState {
    Loading,
    Ready(media::Avatar),
    Missing,
    Failed,
}

struct AudioPlayback {
    message_id: i32,
    child: Option<Child>,
    path: Option<PathBuf>,
    duration: f64,
    position: f64,
    started: Instant,
    paused: bool,
    loading: bool,
    requested_ratio: f64,
    last_tick: Instant,
}

impl AudioPlayback {
    fn current_position(&self) -> f64 {
        let elapsed = if self.paused || self.loading {
            0.0
        } else {
            self.started.elapsed().as_secs_f64()
        };
        let position = self.position + elapsed;
        if self.duration > 0.0 {
            position.min(self.duration)
        } else {
            position
        }
    }
}

impl Drop for AudioPlayback {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(path) = &self.path {
            let _ = fs::remove_file(path);
        }
    }
}

#[derive(Default)]
struct AudioSnapshot {
    active: bool,
    loading: bool,
    paused: bool,
    position: f64,
    duration: f64,
}

struct MessageMenu {
    message_id: i32,
    anchor_x: i32,
    anchor_y: i32,
}

#[derive(Clone)]
enum MessageAction {
    Reply,
    Edit,
    Copy,
    Forward,
    Delete,
    Pin,
    MoreReactions,
    RemoveReaction,
}

#[derive(Clone)]
enum HitTarget {
    AvatarGallery,
    DeleteConfirm(bool),
    DeleteCancel,
    DeletePanel,
    SettingsPanel,
    SettingsTab(settings::Tab),
    SettingsToggle(settings::Toggle),
    Language(String),
    LanguageInput,
    CloseSettings,
    EmojiToggle,
    EmojiPanel,
    EmojiTab(emoji::Tab),
    EmojiPack(i32),
    EmojiPick(usize),
    EmojiClose,
    CopyCode(String),
    CopyUsername(String),
    Menu,
    ToggleSidebar,
    Clipboard,
    PasteKind(crate::telegram::AttachmentKind),
    PasteSend,
    PasteCancel,
    Theme(String),
    PreviewTheme(String),
    Mention(String),
    ApplyTheme,
    CancelTheme,
    Profile,
    ProfileTab(ProfileTab),
    ProfilePost(i32),
    ProfilePostsMore,
    Stories,
    Story(usize),
    StoryMore,
    StoryPanel,
    StoryClose,
    GiftPanel,
    GiftClose,
    GiftMore,
    Gift(usize),
    CloseDetails,
    SharedTab(SharedTab),
    SharedMore,
    SharedMessage(usize),
    SharedAudio {
        index: usize,
        timeline_x: i32,
        timeline_width: i32,
    },
    PinnedNext,
    Pins,
    ImportTheme,
    ThemeBackground,
    NerdFonts,
    Search,
    ChatSearchResult(usize),
    ChatSearchMore,
    ChatSearchClose,
    Folder(i32),
    Chat(usize),
    Message(usize),
    MessageAuthor(Box<Peer>),
    ForwardSource(PeerId),
    Reply(replies::Target),
    Latest,
    Media(usize),
    StickerPack(usize),
    Audio {
        index: usize,
        timeline_x: i32,
        timeline_width: i32,
    },
    Reaction {
        message_id: i32,
        reaction: reactions::Key,
    },
    ReactionPicker(i32),
    RemoveReaction(i32),
    ReactionPack(Option<usize>),
    CloseReactions,
    MessageAction {
        message_id: i32,
        action: MessageAction,
    },
    Comments(usize),
    Composer,
    Send,
    CancelComposeAction,
    Attachment,
    AttachmentKind(crate::telegram::AttachmentKind),
    AttachmentPath(PathBuf),
    AttachmentSend,
}

struct HitBox {
    rect: Rect,
    target: HitTarget,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Rect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl Rect {
    fn contains(self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }
}

struct ImagePlacement {
    message_id: i32,
    asset: ImageAsset,
    row: u16,
    column: u16,
    rows: u32,
    columns: u32,
    crop: bool,
}

#[derive(Clone)]
enum ImageAsset {
    Memory(media::MemoryImage),
    Video(media::MemoryImage, usize),
    Animated(media::StickerFrames),
    Persistent(PathBuf),
    Overlay(media::MemoryImage, i32),
}

impl ImageAsset {
    fn image_data(&self) -> Result<ImageData> {
        match self {
            Self::Animated(frames) => {
                Ok(ImageData::from_png_bytes(frames.current().png().to_vec())?)
            }
            Self::Memory(image) | Self::Video(image, _) | Self::Overlay(image, _) => {
                Ok(ImageData::from_png_bytes(image.png().to_vec())?)
            }
            Self::Persistent(path) => Ok(ImageData::from_png_file(path)?),
        }
    }

    fn dimensions(&self) -> Result<(u32, u32)> {
        match self {
            Self::Animated(frames) => Ok(frames.current().dimensions()),
            Self::Memory(image) | Self::Video(image, _) | Self::Overlay(image, _) => {
                Ok(image.dimensions())
            }
            Self::Persistent(path) => media::image_dimensions(path),
        }
    }

    fn frame_index(&self) -> usize {
        match self {
            Self::Memory(image) | Self::Overlay(image, _) => image.identity(),
            Self::Animated(frames) => frames.current().identity(),
            Self::Video(_, serial) => *serial,
            _ => 0,
        }
    }
}

#[derive(Default)]
struct InlineImages {
    images: HashMap<i32, InlineImage>,
    next_id: u32,
    pending_upload: bool,
}

struct InlineImage {
    image: TerminalImage,
    placement: Option<(u16, u16, u32, u32, bool)>,
    frame_index: usize,
    animated: bool,
    last_frame: Instant,
}

impl InlineImages {
    fn animation_due(&self) -> bool {
        self.images
            .values()
            .any(|image| image.animated && image.last_frame.elapsed() >= Duration::from_millis(100))
    }
    fn render(&mut self, writer: &mut impl Write, placements: &[ImagePlacement]) -> Result<()> {
        self.pending_upload = false;
        let visible: HashSet<_> = placements
            .iter()
            .map(|placement| placement.message_id)
            .collect();
        let stale: Vec<_> = self
            .images
            .keys()
            .copied()
            .filter(|message_id| !visible.contains(message_id))
            .collect();
        for message_id in stale {
            if let Some(mut entry) = self.images.remove(&message_id) {
                entry.image.delete(writer)?;
            }
        }

        let mut pending = Vec::new();
        for placement in placements {
            // Keep the selected animation frame stable throughout this swap.
            let animated_frame = match &placement.asset {
                ImageAsset::Animated(frames) => Some(frames.current()),
                _ => None,
            };
            let frame_index = animated_frame.map_or_else(
                || placement.asset.frame_index(),
                media::MemoryImage::identity,
            );
            let bounds = (
                placement.row,
                placement.column,
                placement.rows,
                placement.columns,
                placement.crop,
            );
            if let Some(entry) = self.images.get_mut(&placement.message_id)
                && entry.frame_index == frame_index
            {
                if entry.animated {
                    entry.last_frame = Instant::now();
                }
                if entry.placement == Some(bounds) {
                    continue;
                }
                write!(
                    writer,
                    "\x1b[{};{}H",
                    placement.row.saturating_add(1),
                    placement.column.saturating_add(1)
                )?;
                place_inline_image(&entry.image, writer, placement)?;
                entry.placement = Some(bounds);
                continue;
            }

            pending.push((placement, animated_frame, frame_index, bounds));
        }

        // First show newly visible images, then update the frames that have
        // waited longest. A fast animation at the top must not monopolize the
        // per-draw upload budget and starve everything below it.
        pending.sort_by_key(|(placement, _, _, _)| {
            self.images
                .get(&placement.message_id)
                .map(|entry| entry.last_frame)
        });
        self.pending_upload = pending.len() > 2;
        for (placement, animated_frame, frame_index, bounds) in pending.into_iter().take(2) {
            // Keep the previous frame visible until its replacement has been
            // uploaded and placed. This also works in terminals that ignore
            // synchronized output markers.
            self.next_id = self.next_id.wrapping_add(1).max(1);
            let image_id = 0x5448_0000u32.wrapping_add(self.next_id);
            let data = if let Some(frame) = animated_frame {
                ImageData::from_png_bytes(frame.png().to_vec())?
            } else {
                placement.asset.image_data()?
            };
            let mut image = TerminalImage::new(image_id, data)?;
            write!(
                writer,
                "\x1b[{};{}H",
                placement.row.saturating_add(1),
                placement.column.saturating_add(1)
            )?;
            image.upload(writer)?;
            place_inline_image(&image, writer, placement)?;
            let old = self.images.insert(
                placement.message_id,
                InlineImage {
                    image,
                    placement: Some(bounds),
                    frame_index,
                    animated: matches!(&placement.asset, ImageAsset::Animated(frames) if frames.frames.len() > 1),
                    last_frame: Instant::now(),
                },
            );
            if let Some(mut old) = old {
                old.image.delete(writer)?;
            }
        }
        Ok(())
    }

    fn clear(&mut self, terminal: &mut Terminal) -> Result<()> {
        let mut group = ImageGroup::begin(&mut terminal.stdout)?;
        for entry in self.images.values_mut() {
            entry.image.delete(&mut group)?;
        }
        self.images.clear();
        self.pending_upload = false;
        group.present()?;
        Ok(())
    }
}

fn place_inline_image(
    image: &TerminalImage,
    writer: &mut impl Write,
    placement: &ImagePlacement,
) -> Result<()> {
    if let ImageAsset::Overlay(_, z_index) = &placement.asset {
        image.place_at_z(writer, placement.columns, placement.rows, *z_index)?;
        return Ok(());
    }
    if !placement.crop {
        image.place(writer, placement.columns, placement.rows)?;
        return Ok(());
    }
    let (pixel_width, pixel_height) = placement.asset.dimensions()?;
    let target_ratio =
        placement.columns as f64 * media::terminal_cell_aspect() / placement.rows.max(1) as f64;
    let source_ratio = pixel_width as f64 / pixel_height.max(1) as f64;
    let (x, y, width, height) = if source_ratio > target_ratio {
        let width = (pixel_height as f64 * target_ratio)
            .round()
            .clamp(1.0, pixel_width as f64) as u32;
        ((pixel_width - width) / 2, 0, width, pixel_height)
    } else {
        let height = (pixel_width as f64 / target_ratio)
            .round()
            .clamp(1.0, pixel_height as f64) as u32;
        (0, (pixel_height - height) / 2, pixel_width, height)
    };
    write!(
        writer,
        "\x1b_Ga=p,i={},p=1,x={x},y={y},w={width},h={height},c={},r={},C=1,q=2\x1b\\",
        image.id(),
        placement.columns,
        placement.rows,
    )?;
    Ok(())
}

struct Panels {
    sidebar: WindowId,
    folders: WindowId,
    content: WindowId,
    composer: WindowId,
    emoji: WindowId,
    chat_search: WindowId,
    details: WindowId,
    stories: WindowId,
    settings: WindowId,
}

fn draw(
    terminal: &mut Terminal,
    windows: &mut WindowManager,
    panels: &Panels,
    app: &mut App,
) -> Result<()> {
    let (height, width) = terminal.getsize();
    app.code_cache.borrow_mut().reset(app.view_id);
    app.emoji_state.reset(app.view_id);
    let compact = app.sidebar_is_compact();
    let layout = pane_layout(width, compact);
    let sidebar_width = layout.chat_x;
    let available = i32::from(width).saturating_sub(sidebar_width);
    let emoji_width = if app.emoji_state.is_open() && available >= 45 {
        35.min(available / 2)
    } else {
        0
    };
    let content_width = available - emoji_width;
    let composer_width = content_width.saturating_sub(2);
    app.composer_width = composer::text_width(composer_width);
    let composer_height = composer::height(app, composer_width);
    let body_height = i32::from(height).saturating_sub(composer_height);
    app.details_rect = None;
    let mut hitboxes = Vec::new();
    let mut placements = Vec::new();
    let mut input_cursor = None;

    {
        let sidebar = windows.get_mut(panels.sidebar).unwrap();
        sidebar.x = layout.rail;
        sidebar.y = 0;
        sidebar.width = layout.chats;
        sidebar.height = i32::from(height);
        sidebar.filling = false;
        sidebar.border = 0;
        sidebar.border_style = app
            .theme
            .style(app.theme.palette.border, app.theme.palette.window);
        sidebar.clear();
        clear_area(sidebar);
        put(
            sidebar,
            0,
            1,
            "☰",
            CellStyle::foreground(app.theme.palette.accent),
            2,
        );
        hitboxes.push(sidebar_toggle_hitbox(sidebar));
        if app.forwarding.is_some() && !compact {
            put(
                sidebar,
                0,
                4,
                app.settings
                    .label("lng_forward_choose", "Forward to…", "Переслать в…"),
                CellStyle::foreground(app.theme.palette.accent),
                sidebar.width - 7,
            );
        }
        if layout.rail == 0 {
            let x = sidebar.width - 4;
            put(
                sidebar,
                0,
                x,
                "⋮",
                CellStyle::foreground(app.theme.palette.accent),
                2,
            );
            hitboxes.push(HitBox {
                rect: Rect {
                    x: sidebar.x + 1 + x,
                    y: sidebar.y + 1,
                    width: 2,
                    height: 1,
                },
                target: HitTarget::Menu,
            });
        }
        let search_text = if compact {
            "⌕".to_owned()
        } else if app.searching || !app.search_query.is_empty() {
            format!(
                "⌕ {}{}",
                app.search_query,
                if app.searching { "█" } else { "" }
            )
        } else {
            format!("⌕ {}", app.settings.ui("Search"))
        };
        put(
            sidebar,
            1,
            1,
            &search_text,
            CellStyle::foreground(app.theme.palette.muted),
            sidebar.width - 4,
        );
        hitboxes.push(HitBox {
            rect: Rect {
                x: sidebar.x + 1,
                y: 2,
                width: sidebar.width.saturating_sub(2),
                height: 1,
            },
            target: HitTarget::Search,
        });
        draw_chats(sidebar, app, &mut hitboxes, &mut placements);
    }

    {
        let rail = windows.get_mut(panels.folders).unwrap();
        rail.x = 0;
        rail.y = 0;
        rail.width = layout.rail;
        rail.height = i32::from(height);
        rail.visible = layout.rail > 0;
        rail.border = -1;
        rail.border_style = app
            .theme
            .style(app.theme.palette.muted, app.theme.palette.rail);
        rail.clear();
        clear_area(rail);
        draw_folder_rail(rail, app, &mut hitboxes);
    }

    {
        let content = windows.get_mut(panels.content).unwrap();
        content.x = sidebar_width;
        content.y = 0;
        content.width = content_width;
        content.visible = content_width > 0;
        content.height = i32::from(height);
        content.filling = false;
        content.border = 0;
        content.border_style = app
            .theme
            .style(app.theme.palette.border, app.theme.palette.chat);
        content.clear();
        clear_area(content);
        if content_width == 0 {
        } else if app.open_chat.is_some() {
            draw_messages(
                content,
                app,
                composer_height,
                &mut hitboxes,
                &mut placements,
            );
            let full_height = content.height;
            // Menus live in the content layer, below the composer window.
            // Size their viewport to the chat body so even the last action
            // (Delete) cannot end up hidden under a multiline draft.
            content.height = body_height + 2;
            draw_message_menu(content, app, &mut hitboxes, &mut placements);
            draw_attachment_menu(content, app, &mut hitboxes, &mut placements);
            reactions::draw_picker(content, app, &mut hitboxes, &mut placements);
            content.height = full_height;
        } else {
            draw_welcome(content, app);
        }
    }

    {
        let composer = windows.get_mut(panels.composer).unwrap();
        composer.x = sidebar_width + 1;
        composer.y = body_height;
        composer.width = content_width.saturating_sub(2);
        composer.visible = content_width > 0 && composer_height > 0;
        composer.height = composer_height;
        composer.filling = false;
        if composer.visible {
            input_cursor = composer::draw(composer, app, &mut hitboxes, &mut placements);
        }
    }

    {
        let panel = windows.get_mut(panels.emoji).unwrap();
        panel.visible = emoji_width > 0;
        if panel.visible {
            panel.x = i32::from(width) - emoji_width;
            panel.y = 0;
            panel.width = emoji_width;
            panel.height = i32::from(height);
            panel.filling = false;
            panel.border = 0;
            panel.border_style = CellStyle::new(
                Some(app.theme.palette.border),
                Some(app.theme.palette.window),
            );
            panel.clear();
            clear_area(panel);
            emoji::draw_picker(panel, app, &mut hitboxes, &mut placements);
        }
    }

    {
        let panel = windows.get_mut(panels.chat_search).unwrap();
        panel.visible = app.chat_search.is_some() && content_width > 8;
        if panel.visible {
            panel.width = content_width.min(60);
            panel.height = i32::from(height).saturating_sub(2).min(18);
            panel.x = sidebar_width + content_width - panel.width;
            panel.y = 2.min(i32::from(height).saturating_sub(panel.height));
            panel.filling = false;
            let rect = Rect {
                x: panel.x,
                y: panel.y,
                width: panel.width,
                height: panel.height,
            };
            placements.retain(|image| {
                let x = i32::from(image.column);
                let y = i32::from(image.row);
                x + image.columns as i32 <= rect.x
                    || x >= rect.x + rect.width
                    || y + image.rows as i32 <= rect.y
                    || y >= rect.y + rect.height
            });
            chat_search::draw(panel, app, &mut hitboxes);
        }
    }

    {
        let panel = windows.get_mut(panels.details).unwrap();
        panel.visible = app.details.is_some();
        panel.width = 64.min(i32::from(width).saturating_sub(2));
        panel.height = 34.min(i32::from(height).saturating_sub(2));
        panel.x = (i32::from(width) - panel.width) / 2;
        panel.y = (i32::from(height) - panel.height) / 2;
        panel.border = 0;
        panel.filling = false;
        panel.border_style = app
            .theme
            .style(app.theme.palette.border, app.theme.palette.window);
        panel.clear();
        if panel.visible {
            let rect = Rect {
                x: panel.x,
                y: panel.y,
                width: panel.width,
                height: panel.height,
            };
            app.details_rect = Some(rect);
            placements.retain(|image| {
                let x = i32::from(image.column);
                let y = i32::from(image.row);
                x + image.columns as i32 <= rect.x
                    || x >= rect.x + rect.width
                    || y + image.rows as i32 <= rect.y
                    || y >= rect.y + rect.height
            });
            clear_area(panel);
            details::draw_details(panel, app, &mut hitboxes, &mut placements);
        }
    }

    if app.theme_preview.is_some() {
        let window = windows.get_mut(panels.details).unwrap();
        window.visible = true;
        window.x = sidebar_width;
        window.width = available;
        window.height = i32::from(height);
        window.clear();
        clear_area(window);
        hitboxes.clear();
        placements.retain(|image| i32::from(image.column) + image.columns as i32 <= sidebar_width);
        details::draw_theme_preview(window, app, &mut hitboxes);
    }

    {
        let window = windows.get_mut(panels.stories).unwrap();
        window.visible = app.stories.popup || app.gifts.popup;
        app.stories_rect = None;
        app.gifts_rect = None;
        if window.visible {
            window.width = 52.min(i32::from(width).saturating_sub(2));
            window.height = 22.min(i32::from(height).saturating_sub(2));
            window.x = (i32::from(width) - window.width) / 2;
            window.y = (i32::from(height) - window.height) / 2;
            window.filling = false;
            let rect = Rect {
                x: window.x,
                y: window.y,
                width: window.width,
                height: window.height,
            };
            if app.gifts.popup {
                app.gifts_rect = Some(rect);
            } else {
                app.stories_rect = Some(rect);
            }
            placements.retain(|image| {
                let x = i32::from(image.column);
                let y = i32::from(image.row);
                x + image.columns as i32 <= rect.x
                    || x >= rect.x + rect.width
                    || y + image.rows as i32 <= rect.y
                    || y >= rect.y + rect.height
            });
            if app.gifts.popup {
                window.border = 0;
                window.border_style = CellStyle::new(
                    Some(app.theme.palette.border),
                    Some(app.theme.palette.window),
                );
                window.name = None;
                window.clear();
                clear_area(window);
                gifts::draw_popup(window, app, &mut hitboxes, &mut placements);
            } else {
                stories::draw_popup(window, app, &mut hitboxes);
            }
        }
    }

    {
        let window = windows.get_mut(panels.settings).unwrap();
        window.visible = app.menu_open || app.deleting.is_some();
        if window.visible {
            let rect = if app.deleting.is_some() {
                let mut r = settings::bounds(i32::from(width), i32::from(height));
                r.width = r.width.min(48);
                r.height = r.height.min(12);
                r.x = (i32::from(width) - r.width) / 2;
                r.y = (i32::from(height) - r.height) / 2;
                r
            } else {
                settings::bounds(i32::from(width), i32::from(height))
            };
            window.x = rect.x;
            window.y = rect.y;
            window.width = rect.width;
            window.height = rect.height;
            window.border = 0;
            window.filling = false;
            window.border_style = CellStyle::new(
                Some(app.theme.palette.border),
                Some(app.theme.palette.window),
            );
            window.clear();
            clear_area(window);
            hitboxes.clear();
            placements.retain(|p| {
                i32::from(p.column) >= rect.x + rect.width
                    || i32::from(p.column) + p.columns as i32 <= rect.x
                    || i32::from(p.row) >= rect.y + rect.height
                    || i32::from(p.row) + p.rows as i32 <= rect.y
            });
            if app.deleting.is_some() {
                interaction::draw_delete(window, app, &mut hitboxes);
            } else {
                settings::draw(window, app, &mut hitboxes);
            }
        }
    }
    app.sync_inline_playback(&mut placements);
    if !app.settings.animation {
        for placement in &mut placements {
            if let ImageAsset::Animated(frames) = &placement.asset {
                placement.asset = ImageAsset::Memory(frames.frames[0].clone());
            }
        }
    }
    app.queue_reaction_visuals(&hitboxes);
    app.queue_reply_previews(&hitboxes);
    app.queue_forward_sources(&hitboxes);
    app.queue_emoji_visuals(&hitboxes);
    app.queue_draft_visuals();
    app.queue_sidebar_emoji(&hitboxes);
    app.queue_gift_visuals(&hitboxes);
    app.queue_post_previews(&hitboxes);
    app.queue_author_avatars(&hitboxes);
    app.hitboxes = hitboxes;
    write!(terminal.stdout, "\x1b[?2026h")?;
    let render_result = (|| -> Result<()> {
        windows.render_windows(terminal)?;
        app.inline_images
            .render(&mut terminal.stdout, &placements)?;
        if let Some((row, column)) = input_cursor.filter(|_| {
            !app.menu_open
                && app.deleting.is_none()
                && app.details.is_none()
                && app.chat_search.is_none()
                && !app.stories.popup
                && !app.gifts.popup
        }) {
            write!(terminal.stdout, "\x1b[{};{}H\x1b[?25h", row + 1, column + 1)?;
        } else {
            write!(terminal.stdout, "\x1b[?25l")?;
        }
        Ok(())
    })();
    let finish_result = write!(terminal.stdout, "\x1b[?2026l").and_then(|_| terminal.refresh());
    render_result?;
    finish_result.map_err(Into::into)
}

struct PaneLayout {
    rail: i32,
    chats: i32,
    chat_x: i32,
}

fn pane_layout(screen_width: u16, compact: bool) -> PaneLayout {
    let width = i32::from(screen_width);
    let rail = if width >= 66 { 11 } else { 0 };
    let chats = if compact {
        9
    } else {
        ((width - rail) * 32 / 100).clamp(16, 40)
    }
    .min((width - rail) / 2);
    PaneLayout {
        rail,
        chats,
        chat_x: rail + chats,
    }
}

fn sidebar_width(screen_width: u16, compact: bool) -> i32 {
    pane_layout(screen_width, compact).chat_x
}

fn sidebar_toggle_hitbox(window: &Window) -> HitBox {
    HitBox {
        rect: Rect {
            x: window.x + 2,
            y: window.y + 1,
            width: 2,
            height: 1,
        },
        target: HitTarget::ToggleSidebar,
    }
}

fn draw_appearance(
    window: &mut Window,
    app: &App,
    hitboxes: &mut Vec<HitBox>,
    content_x: i32,
    content_y: i32,
    content_width: i32,
) {
    let accent = CellStyle::new(
        Some(app.theme.palette.accent),
        Some(app.theme.palette.window),
    );
    let plain = CellStyle::new(Some(app.theme.palette.text), Some(app.theme.palette.window));
    let compact = content_width < 35;
    put(
        window,
        content_y + i32::from(!compact),
        content_x,
        app.settings
            .label("lng_media_color_theme", "Color theme", "Цветовая тема"),
        accent,
        content_width,
    );
    put(
        window,
        content_y + i32::from(!compact) + 1,
        content_x,
        &format!(
            "{}: {}",
            app.settings.label("", "Current", "Текущая"),
            app.theme.name
        ),
        plain,
        content_width,
    );
    let columns = if content_width >= 58 {
        3
    } else if content_width >= 35 {
        2
    } else {
        1
    };
    let card_width = (content_width / columns).max(8);
    for (slot, source) in ["terminal", "classic", "day", "tinted", "night"]
        .into_iter()
        .enumerate()
    {
        let Some(sample) = theme::Theme::builtin(source) else {
            continue;
        };
        let x = content_x + (slot as i32 % columns) * card_width;
        let y = content_y
            + if compact { 2 } else { 4 }
            + (slot as i32 / columns) * if columns == 1 { 1 } else { 3 };
        let active = app.theme.name.eq_ignore_ascii_case(&sample.name);
        put(window, y, x, if active { "●" } else { "○" }, accent, 2);
        put(window, y, x + 2, &sample.name, plain, card_width - 3);
        if columns > 1 {
            for cell in 0..(card_width - 3).min(14) {
                let color = if cell < 4 {
                    sample.palette.window
                } else if cell < 9 {
                    sample.palette.chat
                } else {
                    sample.palette.accent
                };
                window.posstyled(y + 1, x + cell, ' ', CellStyle::background(color));
            }
        }
        hitboxes.push(HitBox {
            rect: Rect {
                x: window.x + 1 + x,
                y: window.y + 1 + y,
                width: card_width - 1,
                height: if columns > 1 { 2 } else { 1 },
            },
            target: HitTarget::Theme(source.to_owned()),
        });
    }
    let rows = (5 + columns - 1) / columns;
    let options_y = content_y + if compact { 8 } else { 5 + rows * 3 };
    for (row, (label, target)) in [
        (
            format!(
                "{}: {}",
                app.settings.label("", "Chat background", "Фон чата"),
                if app.theme.transparent {
                    app.settings.label("", "terminal", "терминал")
                } else {
                    app.settings.label("", "theme", "тема")
                }
            ),
            HitTarget::ThemeBackground,
        ),
        (
            format!(
                "{}: {}",
                app.settings
                    .label("", "Nerd Font icons", "Значки Nerd Font"),
                if app.nerd_fonts {
                    app.settings.label("", "on", "вкл")
                } else {
                    app.settings.label("", "off", "выкл")
                }
            ),
            HitTarget::NerdFonts,
        ),
        (
            app.settings
                .label("", "Import Telegram theme…", "Импорт темы Telegram…")
                .to_owned(),
            HitTarget::ImportTheme,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let y = options_y + row as i32;
        put(window, y, content_x, &label, plain, content_width);
        hitboxes.push(HitBox {
            rect: Rect {
                x: window.x + 1 + content_x,
                y: window.y + 1 + y,
                width: content_width,
                height: 1,
            },
            target,
        });
    }
}

fn folder_icon(folder: &Folder, nerd_fonts: bool) -> &str {
    if !nerd_fonts {
        return match folder.id {
            0 => "@",
            -1 => "A",
            _ => "#",
        };
    }
    match folder.id {
        0 => "\u{f086}",
        -1 => "\u{f187}",
        _ => {
            let icon = folder.icon.as_deref().unwrap_or_default().trim();
            let normalized = icon.trim_matches(['\u{fe0e}', '\u{fe0f}']);
            match normalized {
                "💬" | "🗨" => "\u{f086}",
                "👤" | "👨" | "👩" => "\u{f007}",
                "👥" | "👪" => "\u{f0c0}",
                "🤖" => "\u{f544}",
                "📢" | "📣" => "\u{f0a1}",
                "✅" | "✔" => "\u{f00c}",
                "⭐" | "🌟" => "\u{f005}",
                "❤" | "💙" | "💜" | "🩷" => "\u{f004}",
                "💼" | "👜" => "\u{f0b1}",
                "🏠" | "🏡" => "\u{f015}",
                "🎮" => "\u{f11b}",
                "🎵" | "🎶" => "\u{f001}",
                "🎨" => "\u{f1fc}",
                "💡" => "\u{f0eb}",
                "💰" | "💵" => "\u{f0d6}",
                "✈" => "\u{f072}",
                "🚀" => "\u{f135}",
                "⚽" => "\u{f1e3}",
                "📕" | "📚" | "📖" => "\u{f02d}",
                "🐱" | "🐈" => "\u{f6be}",
                "👍" => "\u{f164}",
                "📁" | "📂" | "🗂" => "\u{f07b}",
                _ if icon.is_ascii() && icon.len() == 1 && !icon.chars().any(char::is_control) => {
                    icon
                }
                _ => "\u{f07b}",
            }
        }
    }
}

fn draw_folder_rail(window: &mut Window, app: &App, hitboxes: &mut Vec<HitBox>) {
    if !window.visible {
        return;
    }
    let inner = window.width - 2;
    put(
        window,
        window.height.saturating_sub(3),
        (inner - 2) / 2,
        if app.nerd_fonts { "\u{f013}" } else { "*" },
        app.theme
            .style(app.theme.palette.accent, app.theme.palette.rail),
        2,
    );
    hitboxes.push(folder_menu_hitbox(window));
    let capacity = (window.height.saturating_sub(5) / 3).max(1) as usize;
    let selected = app
        .folders
        .iter()
        .position(|folder| folder.id == app.active_folder)
        .unwrap_or(0);
    let start = selected.saturating_sub(capacity.saturating_sub(1));
    for (slot, folder) in app.folders.iter().skip(start).take(capacity).enumerate() {
        let y = 1 + slot as i32 * 3;
        let color = if folder.id == app.active_folder {
            app.theme.palette.accent
        } else {
            app.theme.palette.muted
        };
        let icon = folder_icon(folder, app.nerd_fonts);
        put(
            window,
            y,
            (inner - display_width(icon) as i32) / 2,
            icon,
            app.theme.style(color, app.theme.palette.rail),
            inner,
        );
        let label = fit_label(&folder.name, inner.max(1) as usize);
        put(
            window,
            y + 1,
            ((inner - display_width(&label) as i32) / 2).max(0),
            &label,
            app.theme.style(color, app.theme.palette.rail),
            inner,
        );
        if folder.id == app.active_folder {
            window.posstyled(
                y,
                0,
                '▏',
                CellStyle::new(Some(color), window.border_style.background),
            );
        }
        hitboxes.push(HitBox {
            rect: Rect {
                x: 0,
                y: window.y + 1 + y,
                width: window.width,
                height: 3,
            },
            target: HitTarget::Folder(folder.id),
        });
    }
}

fn folder_menu_hitbox(window: &Window) -> HitBox {
    HitBox {
        rect: Rect {
            x: window.x,
            y: window.y + window.height.saturating_sub(2),
            width: window.width,
            height: 1,
        },
        target: HitTarget::Menu,
    }
}

fn draw_chats(
    window: &mut Window,
    app: &App,
    hitboxes: &mut Vec<HitBox>,
    placements: &mut Vec<ImagePlacement>,
) {
    let compact = app.sidebar_is_compact();
    stories::draw_bar(window, app, hitboxes, placements);
    let story_rows = stories::bar_height(app);
    let visible_indices = app.visible_chat_indices();
    if visible_indices.is_empty() {
        put(
            window,
            6 + story_rows,
            1,
            if compact {
                "—"
            } else if app.global_search_loading {
                app.settings.label("", "Searching…", "Поиск…")
            } else {
                app.settings.ui("No matching chats")
            },
            CellStyle::foreground(app.theme.palette.muted),
            window.width - 4,
        );
        return;
    }
    // Browsing the left pane is independent of the currently open conversation.
    let selected = app.chat_index;
    let available_rows = window.height.saturating_sub(5 + story_rows) as usize;
    let available = (available_rows / 3).max(1);
    let selected_position = visible_indices
        .iter()
        .position(|index| *index == selected)
        .unwrap_or(0);
    let start = selected_position.saturating_sub(available.saturating_sub(1));
    for (slot, index) in visible_indices
        .iter()
        .copied()
        .skip(start)
        .take(available)
        .enumerate()
    {
        let chat = &app.chats[index];
        let y = 3 + story_rows + slot as i32 * 3;
        put(
            window,
            y,
            if compact { 0 } else { 1 },
            if index == selected { "›" } else { " " },
            CellStyle::foreground(app.theme.palette.accent),
            2,
        );
        let (columns, rows) = media::square_cells(4, 2);
        let avatar_x = if compact {
            (window.width - 2 - columns as i32).max(0) / 2
        } else {
            2
        };
        if let Some(AvatarState::Ready(avatar)) = app.avatars.get(&index) {
            placements.push(ImagePlacement {
                message_id: avatar_asset_id(index),
                asset: ImageAsset::Persistent(avatar.round.clone()),
                row: (window.y + 1 + y).max(0) as u16,
                column: (window.x + 1 + avatar_x).max(0) as u16,
                rows,
                columns,
                crop: false,
            });
        } else {
            put(
                window,
                y,
                avatar_x + 1,
                if app.tgp_supported {
                    "○"
                } else if app.nerd_fonts {
                    "\u{f03e}"
                } else {
                    "!"
                },
                CellStyle::foreground(app.theme.palette.muted),
                2,
            );
        }
        if compact {
            if chat.unread > 0 {
                put(
                    window,
                    y + 1,
                    window.width - 3,
                    "•",
                    CellStyle::foreground(app.theme.palette.accent),
                    1,
                );
            }
        } else {
            put(
                window,
                y,
                7,
                &chat.name,
                CellStyle::foreground(if index == selected {
                    app.theme.palette.accent
                } else {
                    app.theme.palette.text
                }),
                window.width - 10,
            );
            if chat.unread > 0 {
                let unread = chat.unread.to_string();
                put(
                    window,
                    y,
                    window.width - unread.len() as i32 - 3,
                    &unread,
                    CellStyle::foreground(app.theme.palette.accent),
                    unread.len() as i32,
                );
            }
            put(
                window,
                y + 1,
                7,
                &chat.last.replace('\n', " "),
                CellStyle::foreground(app.theme.palette.muted),
                window.width - 10,
            );
            if let (
                Some(message),
                Some(SidebarEmoji {
                    state: CustomEmojiState::Ready(images),
                    ..
                }),
            ) = (
                chat.last_message.as_ref(),
                app.sidebar_emoji.get(&chat.peer.id),
            ) {
                inline_emoji::draw_sidebar(
                    window,
                    index,
                    &chat.last,
                    message,
                    images,
                    inline_emoji::TextArea {
                        origin: (y + 1, 7),
                        width: window.width.saturating_sub(10).max(1) as usize,
                        lines: 1,
                        code: None,
                    },
                    placements,
                );
            }
        }
        hitboxes.push(HitBox {
            rect: Rect {
                x: window.x + 1,
                y: window.y + 1 + y,
                width: window.width.saturating_sub(2),
                height: 3,
            },
            target: HitTarget::Chat(index),
        });
    }
}

fn draw_welcome(window: &mut Window, app: &App) {
    let width = window.width.saturating_sub(6);
    let center = (window.height / 2).saturating_sub(2);
    put(
        window,
        center,
        3,
        app.settings
            .label("", "Telegram for your terminal", "Telegram в терминале"),
        CellStyle::foreground(app.theme.palette.accent),
        width,
    );
    if let Some(chat) = app.chats.get(app.chat_index) {
        put(
            window,
            center + 2,
            3,
            &format!("Open {} with Enter or a mouse click", chat.name),
            CellStyle::foreground(app.theme.palette.muted),
            width,
        );
    }
}

fn draw_messages(
    window: &mut Window,
    app: &App,
    composer_height: i32,
    hitboxes: &mut Vec<HitBox>,
    placements: &mut Vec<ImagePlacement>,
) {
    if app.current_chat().is_none() {
        return;
    }
    let inner_width = window.width.saturating_sub(2);
    hitboxes.push(HitBox {
        rect: Rect {
            x: window.x + 1,
            y: window.y + 1,
            width: inner_width,
            height: 2,
        },
        target: HitTarget::Profile,
    });
    if let Some(chat_index) = app.open_chat
        && let Some(AvatarState::Ready(avatar)) = app.avatars.get(&chat_index)
    {
        let (columns, rows) = media::square_cells(4, 2);
        placements.push(ImagePlacement {
            message_id: header_avatar_asset_id(chat_index),
            asset: ImageAsset::Persistent(avatar.square.clone()),
            row: (window.y + 1).max(0) as u16,
            column: (window.x + 3).max(0) as u16,
            rows,
            columns,
            crop: false,
        });
    } else {
        put(
            window,
            0,
            2,
            if app.tgp_supported {
                "○"
            } else if app.nerd_fonts {
                "\u{f03e}"
            } else {
                "!"
            },
            CellStyle::foreground(app.theme.palette.muted),
            inner_width - 4,
        );
    }
    put(
        window,
        0,
        7,
        app.view_name(),
        CellStyle::foreground(app.theme.palette.text),
        inner_width - 21,
    );
    if inner_width >= 32 {
        let x = inner_width - 11;
        put(
            window,
            0,
            x,
            app.settings.label("", "Stories ›", "Истории ›"),
            CellStyle::foreground(app.theme.palette.accent),
            9,
        );
        hitboxes.push(HitBox {
            rect: Rect {
                x: window.x + 1 + x,
                y: window.y + 1,
                width: 9,
                height: 1,
            },
            target: HitTarget::Stories,
        });
    }
    put(
        window,
        1,
        7,
        if app.messages_loading || app.loading_older {
            app.settings
                .label("", "loading history…", "загрузка истории…")
        } else {
            app.view_presence()
        },
        CellStyle::foreground(app.theme.palette.muted),
        inner_width - 10,
    );
    for column in 1..inner_width.saturating_sub(1) {
        window.posstyled(
            2,
            column,
            '─',
            CellStyle::foreground(app.theme.palette.muted),
        );
    }

    if app.messages.is_empty() {
        put(
            window,
            5,
            3,
            if app.messages_loading {
                app.settings
                    .label("", "Loading messages…", "Загрузка сообщений…")
            } else {
                app.settings.ui("No messages yet")
            },
            CellStyle::foreground(app.theme.palette.muted),
            inner_width - 6,
        );
        return;
    }

    let area_top = if details::draw_pin_bar(window, app, hitboxes) {
        5
    } else {
        4
    };
    // The outer chat frame reserves two rows even when no composer is shown.
    let bottom = message_bottom(window.height, composer_height);
    let mut cursor = bottom;
    let mut end = app
        .messages
        .len()
        .saturating_sub(app.scroll_from_bottom)
        .max(1);
    if end < app.messages.len() {
        while end > 0
            && app.messages[end].grouped_id().is_some()
            && app.messages[end].grouped_id() == app.messages[end - 1].grouped_id()
        {
            end -= 1;
        }
    }
    let mut visible = Vec::new();
    for indices in message_groups(&app.messages, end).into_iter().rev() {
        let author_width = if show_message_author(app, &app.messages[indices[0]], inner_width) {
            6
        } else {
            0
        };
        let mut spec = bubble_spec(app, &indices, inner_width - author_width, bottom - area_top);
        if indices.len() == 1
            && media::is_circle(&app.messages[indices[0]])
            && app.inline_video.as_ref().is_some_and(|(id, playback)| {
                *id == app.messages[indices[0]].id() && !playback.ended
            })
        {
            let overhead = spec.height - spec.preview_rows;
            let max_rows = (bottom - area_top - overhead).clamp(1, PREVIEW_ROWS * 2);
            let (columns, rows) =
                media::square_cells((inner_width - 8).clamp(1, 68) as u32, max_rows as u32);
            spec.height += rows as i32 - spec.preview_rows;
            spec.preview_rows = rows as i32;
            spec.width = spec.width.max(columns as i32 + 4);
        }
        let y = cursor - spec.height;
        if y < area_top {
            break;
        }
        visible.push((indices, y, spec));
        cursor = y - 1;
    }
    visible.reverse();
    for (indices, y, spec) in visible {
        draw_bubble(window, app, &indices, y, &spec, hitboxes, placements);
    }

    if (app.scroll_from_bottom > 0 || app.has_newer_messages) && bottom > area_top {
        let row = bottom.saturating_sub(1);
        let column = inner_width.saturating_sub(8);
        put(
            window,
            row,
            column,
            "↓ latest",
            CellStyle::foreground(app.theme.palette.accent),
            8,
        );
        hitboxes.push(latest_hitbox(window, row, column));
    }
}

fn message_bottom(window_height: i32, composer_height: i32) -> i32 {
    window_height.saturating_sub(composer_height.max(2))
}

fn latest_hitbox(window: &Window, row: i32, column: i32) -> HitBox {
    HitBox {
        rect: Rect {
            x: window.x + 1 + column,
            y: window.y + 1 + row,
            width: 8,
            height: 1,
        },
        target: HitTarget::Latest,
    }
}

fn draw_message_menu(
    window: &mut Window,
    app: &App,
    hitboxes: &mut Vec<HitBox>,
    placements: &mut Vec<ImagePlacement>,
) {
    let Some(menu) = &app.message_menu else {
        return;
    };
    if app.message_index_by_id(menu.message_id).is_none() {
        return;
    }
    let width = 30.min(window.width.saturating_sub(4)).max(18);
    let can_edit = app
        .message_index_by_id(menu.message_id)
        .and_then(|index| app.messages.get(index))
        .is_some_and(|message| message.outgoing() && !message.text().is_empty());
    let height = 8 + i32::from(can_edit);
    let (x, y) = message_menu_origin(window, (menu.anchor_x, menu.anchor_y), width, height);
    let rect = Rect {
        x: window.x + 1 + x,
        y: window.y + 1 + y,
        width,
        height,
    };
    for row in 0..height {
        for column in 0..width {
            window.posstyled(y + row, x + column, ' ', window.border_style);
        }
    }

    let quick_start = placements.len();
    reactions::draw_quick(
        window,
        app,
        (y, x + 1),
        menu.message_id,
        hitboxes,
        placements,
    );
    for placement in &mut placements[quick_start..] {
        if let ImageAsset::Animated(frames) = &placement.asset {
            placement.asset = ImageAsset::Overlay(frames.current().clone(), i32::MAX);
        } else if let ImageAsset::Memory(image) = &placement.asset {
            placement.asset = ImageAsset::Overlay(image.clone(), i32::MAX);
        }
    }

    let mut actions = vec![
        (
            app.settings.ui("All reactions...").to_owned(),
            MessageAction::MoreReactions,
        ),
        (
            app.settings.ui("Remove my reactions").to_owned(),
            MessageAction::RemoveReaction,
        ),
        (
            format!("↩ {}", app.settings.ui("Reply")),
            MessageAction::Reply,
        ),
        (app.settings.ui("Copy text").to_owned(), MessageAction::Copy),
        (
            app.settings.ui("Forward…").to_owned(),
            MessageAction::Forward,
        ),
        (
            app.settings
                .ui(if app.is_pinned(menu.message_id) {
                    "Unpin"
                } else {
                    "Pin"
                })
                .to_owned(),
            MessageAction::Pin,
        ),
        (app.settings.ui("Delete…").to_owned(), MessageAction::Delete),
    ];
    if can_edit {
        actions.insert(3, (app.settings.ui("Edit").to_owned(), MessageAction::Edit));
    }
    let labels: Vec<&str> = actions.iter().map(|(label, _)| label.as_str()).collect();
    for (row, (label, action)) in actions.iter().enumerate() {
        put(
            window,
            y + 1 + row as i32,
            x + 1,
            label,
            CellStyle::foreground(if matches!(action, MessageAction::Delete) {
                app.theme.palette.error
            } else {
                app.theme.palette.text
            }),
            width - 2,
        );
        hitboxes.push(HitBox {
            rect: Rect {
                x: window.x + 1 + x,
                y: window.y + 1 + y + 1 + row as i32,
                width,
                height: 1,
            },
            target: HitTarget::MessageAction {
                message_id: menu.message_id,
                action: action.clone(),
            },
        });
    }
    if app.tgp_supported {
        let overlay = app
            .menu_overlay_cache
            .borrow()
            .as_ref()
            .and_then(|(id, image)| (*id == menu.message_id).then(|| image.clone()));
        let overlay = overlay.or_else(|| {
            let image = render_menu_overlay(app, width, height, &labels).ok()?;
            *app.menu_overlay_cache.borrow_mut() = Some((menu.message_id, image.clone()));
            Some(image)
        });
        if let Some(image) = overlay {
            placements.push(ImagePlacement {
                message_id: -1_900_000_001,
                asset: ImageAsset::Overlay(image, i32::MAX - 1),
                row: rect.y.max(0) as u16,
                column: rect.x.max(0) as u16,
                rows: height as u32,
                columns: width as u32,
                crop: false,
            });
        }
    }
}

fn render_menu_overlay(
    app: &App,
    width: i32,
    height: i32,
    labels: &[&str],
) -> Result<media::MemoryImage> {
    let cell_width = 12;
    let cell_height = (cell_width as f64 / media::terminal_cell_aspect())
        .round()
        .clamp(16.0, 36.0) as i32;
    let background = app.theme.palette.window.hex();
    let foreground = app.theme.palette.text.hex();
    let danger = app.theme.palette.error.hex();
    let mut svg = format!(
        "<svg xmlns='http://www.w3.org/2000/svg' width='{}' height='{}'><rect width='100%' height='100%' rx='12' fill='{background}'/>",
        width * cell_width,
        height * cell_height
    );
    for (slot, key) in reactions::quick_choices(app).iter().enumerate() {
        let label = match key {
            reactions::Key::Emoji(emoji) => emoji.as_str(),
            reactions::Key::Custom(_) => "◇",
        };
        svg.push_str(&format!("<text x='{}' y='{}' fill='{foreground}' font-family='sans-serif' font-size='18'>{}</text>",
            (slot as i32 * 4 + 2) * cell_width, cell_height - 5, svg_escape(label)));
    }
    for (row, label) in labels.iter().enumerate() {
        let color = if row + 1 == labels.len() {
            &danger
        } else {
            &foreground
        };
        svg.push_str(&format!(
            "<text x='{}' y='{}' fill='{color}' font-family='sans-serif' font-size='16'>{}</text>",
            cell_width,
            (row as i32 + 2) * cell_height - 6,
            svg_escape(label)
        ));
    }
    svg.push_str("</svg>");
    let mut converter = Command::new("rsvg-convert")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    converter
        .stdin
        .take()
        .context("SVG renderer stdin unavailable")?
        .write_all(svg.as_bytes())?;
    let png = converter.wait_with_output()?;
    anyhow::ensure!(png.status.success(), "could not render message menu");
    media::MemoryImage::from_png(png.stdout)
}

fn svg_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn message_menu_origin(window: &Window, anchor: (i32, i32), width: i32, height: i32) -> (i32, i32) {
    (
        (anchor.0 - window.x - 1).clamp(1, window.width.saturating_sub(width + 2).max(1)),
        (anchor.1 - window.y - 1).clamp(3, window.height.saturating_sub(height + 2).max(3)),
    )
}

fn draw_attachment_menu(
    window: &mut Window,
    app: &App,
    hitboxes: &mut Vec<HitBox>,
    placements: &mut Vec<ImagePlacement>,
) {
    use crate::telegram::AttachmentKind;
    if app.pending_attachments.is_some() {
        paste::draw_pending(window, app, hitboxes, placements);
        return;
    }
    if !app.attachment_menu && !app.attaching {
        return;
    }
    let width = window.width.saturating_sub(8).clamp(12, 58);
    let height = if app.attaching {
        window.height.saturating_sub(10).clamp(5, 16)
    } else {
        5
    };
    let x = 2;
    let y = window.height.saturating_sub(height + 5).max(3);
    let rect = Rect {
        x: window.x + 1 + x,
        y: window.y + 1 + y,
        width,
        height,
    };
    placements.retain(|image| {
        let image_rect = Rect {
            x: i32::from(image.column),
            y: i32::from(image.row),
            width: image.columns as i32,
            height: image.rows as i32,
        };
        !(image_rect.x < rect.x + rect.width
            && image_rect.x + image_rect.width > rect.x
            && image_rect.y < rect.y + rect.height
            && image_rect.y + image_rect.height > rect.y)
    });
    for row in y..y + height {
        for col in x..x + width {
            window.posstyled(row, col, ' ', window.border_style);
        }
    }
    if app.attaching {
        put(
            window,
            y,
            x + 1,
            &app.attachment_directory.to_string_lossy(),
            CellStyle::foreground(app.theme.palette.accent),
            width - 2,
        );
        for (slot, path) in app
            .attachment_entries
            .iter()
            .skip(app.attachment_scroll)
            .take((height - 3).max(0) as usize)
            .enumerate()
        {
            let name = if app.attachment_directory.parent() == Some(path.as_path()) {
                "..".to_owned()
            } else {
                path.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            };
            let icon = if app.nerd_fonts {
                if path.is_dir() {
                    "\u{f07b}"
                } else {
                    "\u{f15b}"
                }
            } else if path.is_dir() {
                "/"
            } else {
                "-"
            };
            let row = y + 1 + slot as i32;
            put(
                window,
                row,
                x + 1,
                &format!("{icon}  {name}"),
                CellStyle::foreground(app.theme.palette.text),
                width - 2,
            );
            hitboxes.push(HitBox {
                rect: Rect {
                    x: rect.x,
                    y: window.y + 1 + row,
                    width,
                    height: 1,
                },
                target: HitTarget::AttachmentPath(path.clone()),
            });
        }
        put(
            window,
            y + height - 2,
            x + 1,
            app.settings.label(
                "",
                "Choose a file or type a path below",
                "Выберите файл или введите путь ниже",
            ),
            CellStyle::foreground(app.theme.palette.muted),
            width - 2,
        );
        put(
            window,
            y + height - 1,
            x + 1,
            app.settings.label(
                "",
                "Send attachment     Esc cancel",
                "Отправить вложение     Esc отмена",
            ),
            CellStyle::foreground(app.theme.palette.accent),
            width - 2,
        );
        hitboxes.push(HitBox {
            rect: Rect {
                x: rect.x,
                y: rect.y + height - 1,
                width: 18.min(width),
                height: 1,
            },
            target: HitTarget::AttachmentSend,
        });
    } else {
        for (slot, (label, kind)) in [
            ("Photo (compressed) / video", AttachmentKind::Media),
            ("File (no compression)", AttachmentKind::File),
            ("Audio", AttachmentKind::Audio),
        ]
        .into_iter()
        .enumerate()
        {
            let row = y + 1 + slot as i32;
            put(
                window,
                row,
                x + 1,
                app.settings.label(
                    "",
                    label,
                    match kind {
                        AttachmentKind::Media => "Фото (сжатое) / видео",
                        AttachmentKind::File => "Файл (без сжатия)",
                        AttachmentKind::Audio => "Аудио",
                    },
                ),
                CellStyle::foreground(app.theme.palette.text),
                width - 2,
            );
            hitboxes.push(HitBox {
                rect: Rect {
                    x: rect.x,
                    y: window.y + 1 + row,
                    width,
                    height: 1,
                },
                target: HitTarget::AttachmentKind(kind),
            });
        }
        put(
            window,
            y + 4,
            x + 1,
            app.settings.label(
                "",
                "Paste from clipboard · Ctrl+V",
                "Вставить из буфера · Ctrl+V",
            ),
            CellStyle::foreground(app.theme.palette.accent),
            width - 2,
        );
        hitboxes.push(HitBox {
            rect: Rect {
                x: rect.x,
                y: rect.y + 4,
                width,
                height: 1,
            },
            target: HitTarget::Clipboard,
        });
    }
}

struct BubbleSpec {
    code: Option<code::Layout>,
    width: i32,
    height: i32,
    lines: Vec<String>,
    has_media: bool,
    inline_preview: bool,
    preview_rows: i32,
    album_count: usize,
    is_audio: bool,
    is_sticker: bool,
    has_custom_emoji: bool,
    reactions: Option<String>,
    reply: Option<replies::Preview>,
    forward: Option<forwarded::Preview>,
}

fn album_grid_height(count: usize) -> i32 {
    match count {
        0 | 1 => 0,
        2 => 6,
        3 | 4 => 8,
        5 => 8,
        _ => count.div_ceil(3) as i32 * 3,
    }
}

fn split_axis(total: i32, parts: usize, gap: i32) -> Vec<(i32, i32)> {
    let parts = parts.max(1) as i32;
    let usable = (total - gap * (parts - 1)).max(parts);
    let base = usable / parts;
    let remainder = usable % parts;
    let mut offset = 0;
    (0..parts)
        .map(|part| {
            let size = base + i32::from(part < remainder);
            let result = (offset, size);
            offset += size + gap;
            result
        })
        .collect()
}

fn album_layout(count: usize, width: i32) -> (Vec<Rect>, i32) {
    let gap = 1;
    let total_height = album_grid_height(count);
    let mut tiles = Vec::with_capacity(count);
    match count {
        0 | 1 => {}
        2 => {
            for (x, tile_width) in split_axis(width, 2, gap) {
                tiles.push(Rect {
                    x,
                    y: 0,
                    width: tile_width,
                    height: total_height,
                });
            }
        }
        3 => {
            let columns = split_axis(width, 2, gap);
            tiles.push(Rect {
                x: columns[0].0,
                y: 0,
                width: columns[0].1,
                height: total_height,
            });
            for (y, tile_height) in split_axis(total_height, 2, 0) {
                tiles.push(Rect {
                    x: columns[1].0,
                    y,
                    width: columns[1].1,
                    height: tile_height,
                });
            }
        }
        4 => {
            let columns = split_axis(width, 2, gap);
            let rows = split_axis(total_height, 2, 0);
            for (y, tile_height) in rows {
                for (x, tile_width) in &columns {
                    tiles.push(Rect {
                        x: *x,
                        y,
                        width: *tile_width,
                        height: tile_height,
                    });
                }
            }
        }
        5 => {
            for (x, tile_width) in split_axis(width, 2, gap) {
                tiles.push(Rect {
                    x,
                    y: 0,
                    width: tile_width,
                    height: 4,
                });
            }
            for (x, tile_width) in split_axis(width, 3, gap) {
                tiles.push(Rect {
                    x,
                    y: 4,
                    width: tile_width,
                    height: 4,
                });
            }
        }
        _ => {
            let columns = split_axis(width, 3, gap);
            for slot in 0..count {
                let row = slot / 3;
                let column = slot % 3;
                tiles.push(Rect {
                    x: columns[column].0,
                    y: row as i32 * 3,
                    width: columns[column].1,
                    height: 3,
                });
            }
        }
    }
    (tiles, total_height)
}

fn message_groups(messages: &[Message], end: usize) -> Vec<Vec<usize>> {
    let mut result: Vec<Vec<usize>> = Vec::new();
    let mut grouped: HashMap<i64, usize> = HashMap::new();
    let targets: HashSet<_> = messages.iter().map(Message::id).collect();
    for (index, message) in messages.iter().enumerate().take(end) {
        if compat::record(message).is_some_and(|r| targets.contains(&r.target)) {
            continue;
        }
        if let Some(grouped_id) = message.grouped_id() {
            if let Some(position) = grouped.get(&grouped_id).copied() {
                result[position].push(index);
            } else {
                grouped.insert(grouped_id, result.len());
                result.push(vec![index]);
            }
        } else {
            result.push(vec![index]);
        }
    }
    result
}

fn reaction_summary(app: &App, message: &Message) -> Option<String> {
    let reactions = reactions::display_counts(app, message);
    let summary = reactions
        .iter()
        .filter_map(|reaction| {
            let tl::enums::ReactionCount::Count(reaction) = reaction;
            let symbol = match &reaction.reaction {
                tl::enums::Reaction::Emoji(emoji) => emoji.emoticon.as_str(),
                tl::enums::Reaction::CustomEmoji(_) => "? ",
                tl::enums::Reaction::Paid => "⭐",
                tl::enums::Reaction::Empty => return None,
            };
            Some(format!(
                "{}{} {}",
                if reaction.chosen_order.is_some() {
                    "·"
                } else {
                    ""
                },
                symbol,
                reaction.count
            ))
        })
        .collect::<Vec<_>>()
        .join("  ");
    (!summary.is_empty()).then_some(summary)
}

fn bubble_spec(
    app: &App,
    indices: &[usize],
    available_width: i32,
    available_height: i32,
) -> BubbleSpec {
    let messages = &app.messages;
    let reply = indices
        .iter()
        .find_map(|index| replies::preview(app, &messages[*index]));
    let forward = indices
        .iter()
        .find_map(|index| forwarded::preview(app, &messages[*index]));
    let album_count = indices.len();
    let max_width = (available_width * 72 / 100)
        .clamp(10, 72)
        .min(available_width.saturating_sub(2).max(10));
    let text_width = max_width.saturating_sub(4).max(8) as usize;
    let caption = indices
        .iter()
        .map(|index| &messages[*index])
        .find(|message| !message.text().trim().is_empty())
        .unwrap_or(&messages[indices[0]]);
    let mut code = code::layout(app, caption, text_width);
    let mut body = indices
        .iter()
        .map(|index| messages[*index].text().trim())
        .find(|text| !text.is_empty())
        .unwrap_or_default()
        .to_owned();
    let has_media = indices
        .iter()
        .any(|index| messages[*index].media().is_some());
    if body.is_empty() && !has_media {
        body = "service message".to_owned();
    }
    let mut lines = wrap_text(&body, text_width);
    let max_lines = if album_count > 1 { 2 } else { 6 };
    if code.is_none() && lines.len() > max_lines {
        lines.truncate(max_lines);
        if let Some(last) = lines.last_mut() {
            last.push('…');
        }
    }
    let inline_preview = indices
        .iter()
        .any(|index| media::has_inline_preview(&messages[*index]));
    let is_audio = album_count == 1
        && (media::is_audio(&messages[indices[0]]) || media::is_circle(&messages[indices[0]]));
    let is_sticker = album_count == 1
        && (media::is_sticker(&messages[indices[0]]) || media::is_gif(&messages[indices[0]]));
    let has_custom_emoji = indices
        .iter()
        .any(|index| media::has_custom_emoji(&messages[*index]));
    let reactions = indices
        .iter()
        .find_map(|index| reaction_summary(app, &messages[*index]));
    let media_height = if album_count > 1 {
        album_grid_height(album_count)
    } else if inline_preview {
        PREVIEW_ROWS + i32::from(is_audio)
    } else if is_audio {
        2
    } else if has_media {
        1
    } else {
        0
    };
    if let Some(layout) = &mut code {
        let overhead = media_height
            + 2
            + i32::from(reactions.is_some())
            + 2 * i32::from(reply.is_some())
            + i32::from(forward.is_some());
        layout.truncate((available_height - overhead).max(1) as usize);
        lines = layout.rows.iter().map(code::Row::text).collect();
    }
    let content_width = lines
        .iter()
        .map(|line| display_width(line) as i32)
        .max()
        .unwrap_or(0);
    let media_width = if album_count > 1 {
        40.min(max_width.saturating_sub(4))
    } else if has_media {
        27
    } else {
        0
    };
    let reaction_width = reactions
        .as_ref()
        .map_or(0, |label| display_width(label) as i32);
    let reply_width = reply.as_ref().map_or(0, |reply| {
        display_width(&reply.author).max(display_width(&reply.text)) as i32 + 2
    });
    let forward_width = forward
        .as_ref()
        .map_or(0, |forward| display_width(&forward.text) as i32);
    let width = if code.is_some() {
        max_width
    } else {
        (content_width
            .max(media_width)
            .max(reaction_width)
            .max(reply_width)
            .max(forward_width)
            + 4)
        .clamp(10, max_width)
    };
    let height = (lines.len() as i32
        + media_height
        + 2
        + i32::from(reactions.is_some())
        + 2 * i32::from(reply.is_some()))
        + i32::from(forward.is_some());
    let height = height.max(3);
    BubbleSpec {
        code,
        width,
        height,
        lines,
        has_media,
        inline_preview,
        preview_rows: PREVIEW_ROWS,
        album_count,
        is_audio,
        is_sticker,
        has_custom_emoji,
        reactions,
        reply,
        forward,
    }
}

fn show_message_author(app: &App, message: &Message, width: i32) -> bool {
    !message.outgoing()
        && width >= 20
        && app
            .discussion
            .as_ref()
            .map(|view| &view.peer_object)
            .or_else(|| app.current_chat().map(|chat| &chat.peer_object))
            .is_some_and(|peer| !matches!(peer, Peer::User(_)))
        && message
            .sender()
            .is_some_and(|sender| matches!(sender, Peer::User(_)))
}

fn draw_bubble(
    window: &mut Window,
    app: &App,
    indices: &[usize],
    y: i32,
    spec: &BubbleSpec,
    hitboxes: &mut Vec<HitBox>,
    placements: &mut Vec<ImagePlacement>,
) {
    let index = indices[0];
    let message = &app.messages[index];
    let outgoing = message.outgoing();
    let inner_width = window.width.saturating_sub(2);
    let show_author = show_message_author(app, message, inner_width);
    let x = if outgoing {
        inner_width.saturating_sub(spec.width + 2)
    } else if show_author {
        6
    } else {
        2
    };
    if show_author && let Some(peer) = message.sender().cloned() {
        hitboxes.push(HitBox {
            rect: Rect {
                x: window.x + 1,
                y: window.y + 1 + y,
                width: 4,
                height: 2,
            },
            target: HitTarget::MessageAuthor(Box::new(peer.clone())),
        });
        if let Some(AvatarState::Ready(avatar)) = app.author_avatars.get(&peer.id()) {
            placements.push(ImagePlacement {
                message_id: -1_430_000_000
                    - (peer
                        .id()
                        .bot_api_dialog_id()
                        .unwrap_or_default()
                        .unsigned_abs()
                        % 100_000_000) as i32,
                asset: ImageAsset::Persistent(avatar.round.clone()),
                row: (window.y + 1 + y).max(0) as u16,
                column: (window.x + 1).max(0) as u16,
                rows: 2,
                columns: 4,
                crop: false,
            });
        } else if !app.tgp_supported {
            put(
                window,
                y,
                1,
                if app.nerd_fonts { "\u{f007}" } else { "@" },
                CellStyle::foreground(app.theme.palette.muted),
                2,
            );
        }
    }
    let selected = indices.contains(&app.message_index);
    if selected {
        window.posstyled(
            y + 1,
            x,
            '›',
            CellStyle::foreground(app.theme.palette.accent),
        );
    }
    let bubble_rect = Rect {
        x: window.x + 1 + x,
        y: window.y + 1 + y,
        width: spec.width,
        height: spec.height,
    };
    hitboxes.push(HitBox {
        rect: bubble_rect,
        target: HitTarget::Message(index),
    });

    let sender = if outgoing {
        "You"
    } else {
        Telegram::peer_name(message.sender())
    };
    put(
        window,
        y,
        x + 2,
        sender,
        CellStyle::foreground(if outgoing {
            app.theme.palette.text
        } else {
            app.theme.palette.accent
        }),
        spec.width - 4,
    );

    let mut row = y + 1;
    if let Some(forward) = &spec.forward {
        forwarded::draw(
            window,
            forward,
            (row, x + 2),
            spec.width - 4,
            &app.theme,
            hitboxes,
        );
        row += 1;
    }
    if let Some(reply) = &spec.reply {
        replies::draw(
            window,
            reply,
            (row, x + 2),
            spec.width - 4,
            &app.theme,
            hitboxes,
        );
        row += 2;
    }
    let text_start = row;
    if let Some(layout) = &spec.code {
        code::draw(window, app, layout, (row, x + 2), spec.width - 4, hitboxes);
        row += spec.lines.len() as i32;
    } else {
        for line in &spec.lines {
            put(
                window,
                row,
                x + 2,
                line,
                CellStyle::foreground(if outgoing {
                    app.theme.palette.outgoing
                } else {
                    app.theme.palette.incoming
                }),
                spec.width - 4,
            );
            row += 1;
        }
    }
    let caption_message = indices
        .iter()
        .map(|index| &app.messages[*index])
        .find(|m| !m.text().trim().is_empty())
        .unwrap_or(message);
    details::draw_text_links(
        window,
        caption_message,
        &spec.lines,
        (text_start, x + 2),
        spec.width - 4,
        hitboxes,
        spec.code.as_ref().map(|layout| &layout.positions),
    );

    if spec.has_custom_emoji {
        inline_emoji::draw(
            window,
            app,
            caption_message,
            inline_emoji::TextArea {
                origin: (text_start, x + 2),
                width: (spec.width - 4).max(1) as usize,
                lines: spec.lines.len(),
                code: spec.code.as_ref(),
            },
            placements,
        );
    }

    let media_start = row;
    if spec.has_media {
        let preview_status = app.previews.get(&message.id());
        if !spec.inline_preview && spec.album_count == 1 {
            let label = media::label(message).unwrap_or_else(|| "📎 attachment".to_owned());
            put(
                window,
                row,
                x + 2,
                &label,
                CellStyle::foreground(app.theme.palette.accent),
                spec.width - 4,
            );
            row += 1;
        }

        if spec.is_audio {
            let snapshot = app.audio_snapshot(message.id());
            let duration = if snapshot.duration > 0.0 {
                snapshot.duration
            } else {
                media::audio_duration(message)
            };
            let ratio = if duration > 0.0 {
                (snapshot.position / duration).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let bar_width = (spec.width - 22).max(6) as usize;
            let played = (ratio * bar_width as f64).round() as usize;
            let bar: String = (0..bar_width)
                .map(|position| if position < played { '━' } else { '─' })
                .collect();
            let button = if snapshot.loading {
                "[…]"
            } else if snapshot.active && !snapshot.paused {
                "[||]"
            } else {
                "[>]"
            };
            let elapsed = format_clock(snapshot.position);
            let total = format_clock(duration);
            let controls = format!("{button} {elapsed} {bar} {total}");
            put(
                window,
                row,
                x + 2,
                &controls,
                CellStyle::foreground(app.theme.palette.accent),
                spec.width - 4,
            );
            let timeline_local_x = x + 2 + button.len() as i32 + 1 + elapsed.len() as i32 + 1;
            hitboxes.push(HitBox {
                rect: Rect {
                    x: window.x + 1 + x + 2,
                    y: window.y + 1 + row,
                    width: spec.width - 4,
                    height: 1,
                },
                target: HitTarget::Audio {
                    index,
                    timeline_x: window.x + 1 + timeline_local_x,
                    timeline_width: bar_width as i32,
                },
            });
            row += 1;
        }

        if spec.album_count > 1 {
            let content_width = (spec.width - 4).max(2);
            let (tiles, _) = album_layout(indices.len(), content_width);
            for (member_index, tile) in indices.iter().copied().zip(tiles) {
                let member = &app.messages[member_index];
                let cell_x = x + 2 + tile.x;
                let cell_y = row + tile.y;
                if let Some(PreviewState::Ready(asset)) = app.previews.get(&member.id()) {
                    placements.push(ImagePlacement {
                        message_id: member.id(),
                        asset: asset.clone(),
                        row: (window.y + 1 + cell_y).max(0) as u16,
                        column: (window.x + 1 + cell_x).max(0) as u16,
                        rows: tile.height as u32,
                        columns: tile.width as u32,
                        crop: true,
                    });
                } else if !app.tgp_supported {
                    details::placeholder(
                        window,
                        app,
                        Rect {
                            x: cell_x,
                            y: cell_y,
                            width: tile.width,
                            height: tile.height,
                        },
                    );
                }
                hitboxes.push(HitBox {
                    rect: Rect {
                        x: window.x + 1 + cell_x,
                        y: window.y + 1 + cell_y,
                        width: tile.width,
                        height: tile.height,
                    },
                    target: HitTarget::Media(member_index),
                });
            }
        } else if spec.inline_preview {
            if media::is_sticker(message) {
                hitboxes.push(HitBox {
                    rect: Rect {
                        x: window.x + 1 + x + 2,
                        y: window.y + 1 + row,
                        width: (spec.width - 4).max(1),
                        height: spec.preview_rows,
                    },
                    target: HitTarget::StickerPack(index),
                });
            }
            if let Some(PreviewState::Ready(asset)) = preview_status {
                let max_columns = (spec.width - 4).max(1) as u32;
                let (columns, rows) = asset
                    .dimensions()
                    .map(|(width, height)| {
                        media::fit_dimensions(width, height, max_columns, spec.preview_rows as u32)
                    })
                    .unwrap_or((max_columns, spec.preview_rows as u32));
                let column_offset = ((max_columns.saturating_sub(columns)) / 2) as i32;
                placements.push(ImagePlacement {
                    message_id: message.id(),
                    asset: asset.clone(),
                    row: (window.y + 1 + row).max(0) as u16,
                    column: (window.x + 1 + x + 2 + column_offset).max(0) as u16,
                    rows,
                    columns,
                    crop: false,
                });
            } else if !app.tgp_supported {
                details::placeholder(
                    window,
                    app,
                    Rect {
                        x: x + 2,
                        y: row,
                        width: spec.width - 4,
                        height: spec.preview_rows,
                    },
                );
            }
        }
    }

    if spec.reactions.is_some() {
        reactions::draw_chips(
            window,
            app,
            message,
            (y + spec.height - 2, x + 2),
            spec.width - 4,
            hitboxes,
            placements,
        );
    }

    let time = message.date().strftime("%H:%M").to_string();
    put(
        window,
        y + spec.height - 1,
        x + spec.width - time.len() as i32 - if outgoing { 5 } else { 2 },
        &time,
        CellStyle::foreground(app.theme.palette.muted),
        time.len() as i32,
    );
    if outgoing {
        let read = message.id() <= app.read_outbox_max;
        put(
            window,
            y + spec.height - 1,
            x + spec.width - 3,
            if read { "✓✓" } else { "✓" },
            CellStyle::foreground(if read {
                app.theme.palette.accent
            } else {
                app.theme.palette.muted
            }),
            2,
        );
    }

    if spec.has_media && spec.album_count == 1 && !spec.is_audio && !spec.is_sticker {
        hitboxes.push(HitBox {
            rect: Rect {
                x: bubble_rect.x,
                y: window.y + 1 + media_start,
                width: bubble_rect.width,
                height: if spec.inline_preview {
                    spec.preview_rows
                } else {
                    1
                },
            },
            target: HitTarget::Media(index),
        });
    }
    if media::is_circle(message) {
        hitboxes.push(HitBox {
            rect: Rect {
                x: bubble_rect.x,
                y: window.y + 2 + media_start,
                width: bubble_rect.width,
                height: spec.preview_rows,
            },
            target: HitTarget::Audio {
                index,
                timeline_x: i32::MAX,
                timeline_width: 1,
            },
        });
    }

    if let Some(replies) = message.reply_count() {
        let label = format!("💬 {replies}");
        let width = UnicodeWidthStr::width(label.as_str()) as i32;
        put(
            window,
            y + spec.height - 1,
            x + 2,
            &label,
            CellStyle::foreground(app.theme.palette.accent),
            width,
        );
        hitboxes.push(HitBox {
            rect: Rect {
                x: bubble_rect.x + 2,
                y: bubble_rect.y + spec.height - 1,
                width,
                height: 1,
            },
            target: HitTarget::Comments(index),
        });
    }
}

fn wrap_text(text: &str, max_width: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let max_width = max_width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut width = 0usize;
    for grapheme in UnicodeSegmentation::graphemes(text, true) {
        if grapheme == "\n" {
            lines.push(line.trim_end().to_owned());
            line.clear();
            width = 0;
            continue;
        }
        let character_width = display_width(grapheme).max(1);
        if width + character_width > max_width && !line.is_empty() {
            lines.push(line.trim_end().to_owned());
            line.clear();
            width = 0;
            if grapheme.chars().all(char::is_whitespace) {
                continue;
            }
        }
        line.push_str(grapheme);
        width += character_width;
    }
    if !line.is_empty() || lines.is_empty() {
        lines.push(line.trim_end().to_owned());
    }
    lines
}

fn pop_grapheme(text: &mut String) {
    if let Some((index, _)) = UnicodeSegmentation::grapheme_indices(text.as_str(), true).next_back()
    {
        text.truncate(index);
    }
}

fn format_clock(seconds: f64) -> String {
    let seconds = seconds.max(0.0).round() as u64;
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

fn clear_area(window: &mut Window) {
    for row in 0..window.height.saturating_sub(2) {
        for column in 0..window.width.saturating_sub(2) {
            window.posstyled(row, column, ' ', window.border_style);
        }
    }
}

fn avatar_asset_id(index: usize) -> i32 {
    -1 - i32::try_from(index).unwrap_or(i32::MAX / 4)
}

fn custom_emoji_asset_id(message_id: i32, slot: usize) -> i32 {
    // Distinct slots of adjacent messages must not alias, nor overlap formula/picker layers.
    let key = u64::from(message_id.unsigned_abs()) * 4096 + slot as u64;
    -2_140_000_000 + (key % 200_000_000) as i32
}

fn header_avatar_asset_id(index: usize) -> i32 {
    -1_000_000 - i32::try_from(index).unwrap_or(i32::MAX / 4)
}

fn put(window: &mut Window, y: i32, x: i32, text: &str, style: CellStyle, max_width: i32) {
    let style = CellStyle::new(
        style.foreground,
        style.background.or(window.border_style.background),
    );
    if max_width <= 0 || y < 0 || y >= window.height.saturating_sub(2) {
        return;
    }
    let mut offset = 0i32;
    for grapheme in UnicodeSegmentation::graphemes(text, true) {
        if grapheme.chars().any(char::is_control) {
            continue;
        }
        let character_width = display_width(grapheme).max(1) as i32;
        if offset + character_width > max_width {
            if max_width >= 1 {
                window.posstyled(y, x + max_width - 1, '.', style);
            }
            break;
        }
        window.posstyled_grapheme(y, x + offset, grapheme, style);
        offset += character_width;
    }
}

fn display_width(text: &str) -> usize {
    UnicodeSegmentation::graphemes(text, true)
        .map(|grapheme| {
            if grapheme.chars().any(
                |c| matches!(c as u32, 0xe000..=0xf8ff | 0xf0000..=0xffffd | 0x100000..=0x10fffd),
            ) {
                2
            } else {
                UnicodeWidthStr::width(grapheme)
            }
        })
        .sum()
}

fn fit_label(text: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for grapheme in UnicodeSegmentation::graphemes(text, true) {
        let size = display_width(grapheme);
        if used + size > width {
            break;
        }
        result.push_str(grapheme);
        used += size;
    }
    result
}

fn input_text(terminal: &Terminal) -> Option<String> {
    let count = *terminal.readed.lock().ok()?;
    let bytes = terminal.reading_buffer.lock().ok()?;
    decoded_input_text(&bytes[..count])
}

fn decoded_input_text(bytes: &[u8]) -> Option<String> {
    let text = terminalib::terminal::associated_key_text(bytes)
        .or_else(|| std::str::from_utf8(bytes).ok().map(str::to_owned))?;
    (!text.chars().any(char::is_control)).then_some(text)
}

fn copy_to_clipboard(terminal: &mut Terminal, text: &str) -> Result<()> {
    let encoded = base64_encode(text.as_bytes());
    write!(terminal.stdout, "\x1b]52;c;{encoded}\x07")?;
    terminal.refresh()?;
    Ok(())
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let value = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        encoded.push(ALPHABET[((value >> 18) & 0x3f) as usize] as char);
        encoded.push(ALPHABET[((value >> 12) & 0x3f) as usize] as char);
        encoded.push(if chunk.len() > 1 {
            ALPHABET[((value >> 6) & 0x3f) as usize] as char
        } else {
            '='
        });
        encoded.push(if chunk.len() > 2 {
            ALPHABET[(value & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_composer_keeps_last_media_and_comment_hitbox_inside_chat() {
        for (height, composer_height) in [(30, 0), (30, 3), (30, 7), (12, 0)] {
            let bottom = message_bottom(height, composer_height);
            let bubble_height = 8.min(bottom.saturating_sub(4));
            let y = bottom - bubble_height;
            let last_row = y + bubble_height - 1;
            assert!(last_row < height - 2);
            assert!(last_row >= 4);
            assert_eq!(last_row, bottom - 1);
        }
        assert_eq!(message_bottom(30, 0), 28);
        assert_eq!(message_bottom(30, 3), 27);
    }

    #[test]
    fn all_context_menu_actions_fit_above_single_and_multiline_composers() {
        for composer_height in [3, 7] {
            let screen = 30;
            let body = screen - composer_height;
            let mut window = Window::new(0);
            window.width = 70;
            window.height = body + 2;
            let (_, y) = message_menu_origin(&window, (60, body), 30, 8);
            let last_action_absolute = y + 1 + 7;
            assert!(
                last_action_absolute < body + 1,
                "Delete overlaps the composer"
            );
        }
    }

    #[test]
    fn composer_preserves_explicit_newlines() {
        assert_eq!(
            composer::layout("first\nsecond\n", 30).lines,
            ["first", "second", ""]
        );
        assert_eq!(composer::layout("😀\nя", 8).lines, ["😀", "я"]);
        assert_eq!(composer::layout("", 8).lines, [""]);
    }

    #[test]
    fn custom_emoji_slots_do_not_alias_adjacent_messages_or_overlay_layers() {
        let mut ids = HashSet::new();
        for message in 1..100 {
            for slot in 0..40 {
                let id = custom_emoji_asset_id(message, slot);
                assert!((-2_140_000_000..-1_940_000_000).contains(&id));
                assert!(ids.insert(id));
            }
        }
    }

    #[test]
    fn replacing_a_static_sticker_changes_its_graphics_identity() {
        let mut png = vec![0; 24];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        png[8..12].copy_from_slice(&13u32.to_be_bytes());
        png[12..16].copy_from_slice(b"IHDR");
        png[16..20].copy_from_slice(&1u32.to_be_bytes());
        png[20..24].copy_from_slice(&1u32.to_be_bytes());
        let first = ImageAsset::Animated(media::StickerFrames::still(
            media::MemoryImage::from_png(png.clone()).unwrap(),
        ));
        let second = ImageAsset::Animated(media::StickerFrames::still(
            media::MemoryImage::from_png(png).unwrap(),
        ));
        assert_ne!(first.frame_index(), second.frame_index());
        assert_eq!(first.frame_index(), first.clone().frame_index());
    }

    #[test]
    fn animated_frame_swap_places_the_new_image_before_deleting_the_old() {
        let mut png = vec![0; 24];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        png[8..12].copy_from_slice(&13u32.to_be_bytes());
        png[12..16].copy_from_slice(b"IHDR");
        png[16..20].copy_from_slice(&1u32.to_be_bytes());
        png[20..24].copy_from_slice(&1u32.to_be_bytes());
        let placement = |image| ImagePlacement {
            message_id: 42,
            asset: ImageAsset::Memory(image),
            row: 2,
            column: 3,
            rows: 2,
            columns: 4,
            crop: false,
        };
        let mut images = InlineImages::default();
        let first = media::MemoryImage::from_png(png.clone()).unwrap();
        let second = media::MemoryImage::from_png(png).unwrap();
        images
            .render(&mut Vec::new(), &[placement(first.clone())])
            .unwrap();
        let mut output = Vec::new();
        images.render(&mut output, &[placement(second)]).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.find("a=t,").unwrap() < output.find("a=p,").unwrap());
        assert!(output.find("a=p,").unwrap() < output.find("a=d,d=I,").unwrap());
        assert!(!output.contains("\x1b[?2026"));
    }

    #[test]
    fn composer_text_accepts_plain_utf8_and_kitty_without_associated_text() {
        assert_eq!(decoded_input_text(b"a").as_deref(), Some("a"));
        assert_eq!(decoded_input_text("й".as_bytes()).as_deref(), Some("й"));
        assert_eq!(decoded_input_text(b"\x1b[0;1;1081u").as_deref(), Some("й"));
        assert_eq!(decoded_input_text(b"\x1b[1081::113u").as_deref(), Some("й"));
        assert_eq!(decoded_input_text(b"\x1b[97;5u"), None);
    }

    #[test]
    fn animated_uploads_do_not_starve_other_visible_images() {
        let mut png = vec![0; 24];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        png[8..12].copy_from_slice(&13u32.to_be_bytes());
        png[12..16].copy_from_slice(b"IHDR");
        png[16..20].copy_from_slice(&1u32.to_be_bytes());
        png[20..24].copy_from_slice(&1u32.to_be_bytes());
        let image = media::MemoryImage::from_png(png).unwrap();
        let mut images = InlineImages::default();
        for frame in 1..=8 {
            let placements: Vec<_> = (1..=5)
                .map(|message_id| ImagePlacement {
                    message_id,
                    asset: ImageAsset::Video(image.clone(), frame),
                    row: message_id as u16,
                    column: 0,
                    rows: 1,
                    columns: 1,
                    crop: false,
                })
                .collect();
            images.render(&mut Vec::new(), &placements).unwrap();
            if frame >= 3 {
                assert_eq!(images.images.len(), 5);
                assert!(
                    images
                        .images
                        .values()
                        .all(|entry| frame - entry.frame_index <= 2)
                );
            }
        }
    }

    #[test]
    fn all_visible_images_survive_the_per_frame_upload_budget() {
        let image = media::MemoryImage::from_png(
            include_bytes!("../vendor/terminalib/tests/imgs/err.png").to_vec(),
        )
        .unwrap();
        let placements: Vec<_> = (1..=40)
            .map(|message_id| ImagePlacement {
                message_id,
                asset: ImageAsset::Memory(image.clone()),
                row: message_id as u16,
                column: 0,
                rows: 1,
                columns: 1,
                crop: false,
            })
            .collect();
        let mut images = InlineImages::default();
        for _ in 0..20 {
            images.render(&mut Vec::new(), &placements).unwrap();
        }
        assert_eq!(images.images.len(), 40);
        assert!(!images.pending_upload);
        let mut output = Vec::new();
        images.render(&mut output, &placements).unwrap();
        assert!(output.is_empty());
        assert_eq!(images.images.len(), 40);
    }

    #[test]
    fn failed_image_previews_retry_with_bounded_delay() {
        assert_eq!(preview_retry_delay(1), Duration::from_secs(2));
        assert_eq!(preview_retry_delay(2), Duration::from_secs(4));
        assert_eq!(preview_retry_delay(100), Duration::from_secs(30));
    }

    #[test]
    fn preview_selection_is_bounded_and_stays_near_the_viewport() {
        assert!(preview_message_indices(0, 0, 0).is_empty());
        let indices = preview_message_indices(100_000, 1_000, 20);
        assert_eq!(indices[0], 98_999);
        assert_eq!(indices.len(), 66);
        assert_eq!(
            indices.iter().copied().collect::<HashSet<_>>().len(),
            indices.len()
        );
        assert!(
            indices
                .iter()
                .all(|index| index.abs_diff(98_999) <= 20 || index.abs_diff(20) <= 12)
        );
        assert_eq!(preview_message_indices(1, usize::MAX, usize::MAX), vec![0]);
    }

    #[test]
    fn dropping_audio_stops_the_process_and_removes_temporary_media() {
        let path = std::env::temp_dir().join(format!("telega-audio-drop-{}", std::process::id()));
        fs::write(&path, b"temporary audio").unwrap();
        let child = Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        drop(AudioPlayback {
            message_id: 1,
            child: Some(child),
            path: Some(path.clone()),
            duration: 60.0,
            position: 0.0,
            started: Instant::now(),
            paused: false,
            loading: false,
            requested_ratio: 0.0,
            last_tick: Instant::now(),
        });
        assert!(!path.exists());
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }

    #[test]
    fn compact_chat_list_frees_space_without_moving_the_folder_rail() {
        for width in [40, 65, 66, 100, 180] {
            let full = pane_layout(width, false);
            let compact = pane_layout(width, true);
            assert_eq!(full.rail, compact.rail);
            assert_eq!(compact.chats, 9);
            assert!(compact.chat_x < full.chat_x);
            assert_eq!(sidebar_width(width, true), compact.chat_x);
            assert_eq!(sidebar_width(width, false), full.chat_x);
            let (columns, _) = media::square_cells(4, 2);
            assert!(columns as i32 <= compact.chats - 2);
            assert!(compact.chat_x < i32::from(width));
        }
    }

    #[test]
    fn the_two_hamburger_buttons_have_distinct_actions_and_hitboxes() {
        for compact in [false, true] {
            let layout = pane_layout(100, compact);
            let mut sidebar = Window::new(0);
            sidebar.x = layout.rail;
            sidebar.width = layout.chats;
            let mut rail = Window::new(0);
            rail.width = layout.rail;
            rail.height = 34;
            let toggle = sidebar_toggle_hitbox(&sidebar);
            let menu = folder_menu_hitbox(&rail);
            assert!(matches!(toggle.target, HitTarget::ToggleSidebar));
            assert!(matches!(menu.target, HitTarget::Menu));
            assert!(toggle.rect.contains(layout.rail + 2, 1));
            assert!(!menu.rect.contains(layout.rail + 2, 1));
            assert!(menu.rect.contains(2, 32));
            assert!(!toggle.rect.contains(2, 32));
        }
    }

    #[test]
    fn escape_and_q_quit_only_from_the_initial_folder_without_an_open_chat() {
        for key in [27, i32::from(b'q')] {
            assert_eq!(
                back_action(key, 0, false, false, false),
                Some(BackAction::Quit)
            );
            for folder in [-1, 2, 37] {
                assert_eq!(
                    back_action(key, folder, false, false, false),
                    Some(BackAction::Home)
                );
            }
            for folder in [0, -1, 2] {
                assert_eq!(
                    back_action(key, folder, true, false, false),
                    Some(BackAction::CloseChat)
                );
                assert_eq!(
                    back_action(key, folder, true, true, false),
                    Some(BackAction::CloseDetails)
                );
                for chat_open in [false, true] {
                    assert_eq!(
                        back_action(key, folder, chat_open, true, true),
                        Some(BackAction::CloseOverlay)
                    );
                    assert_eq!(
                        back_action(key, folder, chat_open, false, true),
                        Some(BackAction::CloseOverlay)
                    );
                }
            }
        }
    }

    #[test]
    fn other_keys_do_not_trigger_back_or_exit() {
        for key in [0, KEY_LEFT, KEY_UP, 13, i32::from(b'Q'), i32::from(b'i')] {
            assert_eq!(back_action(key, 0, false, false, false), None);
        }
    }

    #[test]
    fn latest_button_has_a_hitbox_over_every_label_cell() {
        let mut window = Window::new(0);
        window.x = 30;
        window.y = 2;
        let hitbox = latest_hitbox(&window, 20, 45);
        assert!(matches!(hitbox.target, HitTarget::Latest));
        assert!(hitbox.rect.contains(76, 23));
        assert!(hitbox.rect.contains(83, 23));
        assert!(!hitbox.rect.contains(84, 23));
        assert!(!hitbox.rect.contains(76, 24));
    }

    #[test]
    fn folder_icons_never_disappear_for_empty_or_invisible_server_values() {
        let mut folder = Folder {
            id: 7,
            name: "Work".to_owned(),
            icon: None,
        };
        for icon in [
            None,
            Some(""),
            Some("  "),
            Some("\u{fe0f}"),
            Some("\u{200d}"),
            Some("\u{1b}[31m"),
        ] {
            folder.icon = icon.map(str::to_owned);
            assert_eq!(folder_icon(&folder, true), "\u{f07b}");
            assert_eq!(folder_icon(&folder, false), "#");
        }
    }

    #[test]
    fn standard_folder_emoji_use_nerd_glyphs_with_optional_variation_selector() {
        let mut folder = Folder {
            id: 7,
            name: "Travel".to_owned(),
            icon: Some("✈\u{fe0f}".to_owned()),
        };
        assert_eq!(folder_icon(&folder, true), "\u{f072}");
        folder.icon = Some("✈".to_owned());
        assert_eq!(folder_icon(&folder, true), "\u{f072}");
        folder.icon = Some("🦄".to_owned());
        assert_eq!(folder_icon(&folder, true), "\u{f07b}");
        assert_eq!(folder_icon(&folder, false), "#");
        folder.id = 0;
        assert_eq!(folder_icon(&folder, false), "@");
        folder.id = -1;
        assert_eq!(folder_icon(&folder, true), "\u{f187}");
    }

    #[test]
    fn complex_unmapped_folder_icon_uses_stable_glyph() {
        let folder = Folder {
            id: 8,
            name: "Custom".into(),
            icon: Some("🧑‍💻".into()),
        };
        assert_eq!(folder_icon(&folder, true), "\u{f07b}");
    }

    #[test]
    fn input_rendering_handles_wide_emoji_without_panicking() {
        let mut window = Window::new(0);
        window.width = 12;
        window.height = 4;
        put(
            &mut window,
            0,
            0,
            "hello 👋 world",
            CellStyle::default(),
            10,
        );
        assert!(window.cells().count() > 0);
    }

    #[test]
    fn wraps_cyrillic_and_wide_emoji_to_cell_width() {
        let lines = wrap_text("Привет 👨‍💻 из Telegram", 10);
        assert!(lines.len() > 1);
        assert!(
            lines
                .iter()
                .all(|line| UnicodeWidthStr::width(line.as_str()) <= 10)
        );
    }

    #[test]
    fn backspace_removes_a_complete_emoji_grapheme() {
        let mut text = "ok 👨‍💻".to_owned();
        pop_grapheme(&mut text);
        assert_eq!(text, "ok ");
    }

    #[test]
    fn hitbox_uses_half_open_bounds() {
        let rect = Rect {
            x: 10,
            y: 5,
            width: 4,
            height: 3,
        };
        assert!(rect.contains(10, 5));
        assert!(rect.contains(13, 7));
        assert!(!rect.contains(14, 7));
        assert!(!rect.contains(13, 8));
    }

    #[test]
    fn clipboard_base64_is_rfc_4648_compatible() {
        assert_eq!(base64_encode("Привет".as_bytes()), "0J/RgNC40LLQtdGC");
        assert_eq!(base64_encode(b"a"), "YQ==");
    }
}
