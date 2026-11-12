use super::*;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum Tab {
    #[default]
    Appearance,
    Media,
    Folders,
    Storage,
    Controls,
    Languages,
}
const TABS: [(Tab, &str); 6] = [
    (Tab::Appearance, "Appearance"),
    (Tab::Media, "Media"),
    (Tab::Folders, "Folders"),
    (Tab::Storage, "Storage"),
    (Tab::Controls, "Controls"),
    (Tab::Languages, "Languages"),
];
#[derive(Clone, Copy)]
pub(super) enum Toggle {
    Autoplay,
    Animation,
    Sidebar,
}

pub(super) struct State {
    pub autoplay: bool,
    pub animation: bool,
    tab: Tab,
    focused_nav: bool,
    selected: usize,
    path: Option<String>,
    language_input: Option<String>,
    pub(super) language_code: String,
    language_name: String,
    strings: HashMap<String, String>,
    languages: Vec<tl::types::LangPackLanguage>,
    languages_loading: bool,
    languages_loaded: bool,
    language_request: u64,
}
impl Default for State {
    fn default() -> Self {
        Self {
            autoplay: true,
            animation: true,
            tab: Tab::default(),
            focused_nav: true,
            selected: 0,
            path: None,
            language_input: None,
            language_code: "en".into(),
            language_name: "English".into(),
            strings: HashMap::new(),
            languages: Vec::new(),
            languages_loading: false,
            languages_loaded: false,
            language_request: 0,
        }
    }
}
impl State {
    pub fn restore(root: &std::path::Path) -> Self {
        let text = fs::read_to_string(root.join("preferences")).unwrap_or_default();
        let language_code = fs::read_to_string(root.join("language"))
            .unwrap_or_else(|_| "en".into())
            .trim()
            .to_owned();
        Self {
            autoplay: !text.lines().any(|l| l == "autoplay=0"),
            animation: !text.lines().any(|l| l == "animation=0"),
            language_name: match language_code.as_str() {
                "ru" => "Русский",
                "en" => "English",
                _ => "Custom",
            }
            .into(),
            language_code,
            ..Self::default()
        }
    }
    fn save(&self, root: &std::path::Path) -> Result<()> {
        fs::create_dir_all(root)?;
        fs::write(
            root.join("preferences"),
            format!(
                "autoplay={}\nanimation={}\n",
                u8::from(self.autoplay),
                u8::from(self.animation)
            ),
        )?;
        Ok(())
    }
}

pub(super) enum Event {
    Languages(std::result::Result<Vec<tl::types::LangPackLanguage>, String>),
    Language {
        request: u64,
        code: String,
        result: std::result::Result<(String, HashMap<String, String>), String>,
    },
}

impl State {
    pub(super) fn label<'a>(&'a self, key: &str, english: &'a str, russian: &'a str) -> &'a str {
        self.strings
            .get(key)
            .map(String::as_str)
            .unwrap_or(if self.language_code == "ru" {
                russian
            } else {
                english
            })
    }
    fn tab_label(&self, tab: Tab) -> &str {
        match tab {
            Tab::Appearance => self.label("lng_edit_channel_color", "Appearance", "Внешний вид"),
            Tab::Media => self.label("lng_media_type_media", "Media", "Медиа"),
            Tab::Folders => self.label("lng_settings_section_filters", "Folders", "Папки"),
            Tab::Storage => self.label("", "Storage", "Хранилище"),
            Tab::Controls => self.label("", "Controls", "Управление"),
            Tab::Languages => self.label("lng_settings_language", "Language", "Язык"),
        }
    }
    pub(super) fn ui<'a>(&'a self, english: &'a str) -> &'a str {
        let (key, russian) = match english {
            "Settings" => ("lng_menu_settings", "Настройки"),
            "Profile" => ("lng_settings_color_tab_profile", "Профиль"),
            "Posts" => ("lng_media_type_posts", "Публикации"),
            "Gifts" => ("lng_media_type_gifts", "Подарки"),
            "Stories" => ("lng_media_type_stories", "Истории"),
            "Media" => ("lng_media_type_media", "Медиа"),
            "Files" => ("lng_media_type_files", "Файлы"),
            "Music" => ("lng_all_music", "Музыка"),
            "Voice" => ("lng_all_voice", "Голосовые"),
            "Links" => ("lng_all_links", "Ссылки"),
            "Pinned" => ("lng_sr_chat_pinned", "Закрепы"),
            "Search" => ("lng_dlg_filter", "Поиск"),
            "Send" => ("lng_send_button", "Отправить"),
            "Cancel" => ("lng_cancel", "Отмена"),
            "Copy" => ("lng_code_block_header_copy", "Копировать"),
            "Reply" => ("lng_context_reply_msg", "Ответить"),
            "Edit" => ("lng_context_edit_msg", "Изменить"),
            "Delete for everyone" => ("lng_delete_for_everyone_check", "Удалить у всех"),
            "Photo" => ("lng_attach_photo", "Фото"),
            "File" => ("lng_attach_file", "Файл"),
            "Audio" => ("lng_attach_audio", "Аудио"),
            "Emoji" => ("lng_switch_emoji", "Эмодзи"),
            "Stickers" => ("lng_switch_stickers", "Стикеры"),
            "Custom" => ("", "Особые"),
            "Recent" => ("lng_recent_title", "Недавние"),
            "All reactions" => ("lng_manage_peer_reactions_all", "Все реакции"),
            "Deleted message" => ("lng_deleted_message", "Удалённое сообщение"),
            "You" => ("lng_from_you", "Вы"),
            "Sticker" => ("lng_in_dlg_sticker", "Стикер"),
            "Video message" => ("lng_media_round", "Видеосообщение"),
            "Audio / voice message" => ("", "Аудио / голосовое сообщение"),
            "Photo / video" => ("", "Фото / видео"),
            "Attachment" => ("", "Вложение"),
            "Service message" => ("", "Служебное сообщение"),
            "Reply to message" => ("", "Ответ на сообщение"),
            "Message unavailable" => ("", "Сообщение недоступно"),
            "Loading message…" => ("", "Загрузка сообщения…"),
            "No results" => ("lng_search_messages_none", "Нет результатов"),
            "Loading..." => ("lng_profile_loading", "Загрузка..."),
            "No messages" => ("lng_forum_no_messages", "Нет сообщений"),
            "Today" => ("", "Сегодня"),
            "Yesterday" => ("", "Вчера"),
            "Load more" => ("", "Загрузить ещё"),
            "No attachments" => ("", "Нет вложений"),
            "No publications" => ("", "Нет публикаций"),
            "No visible gifts" => ("", "Нет доступных подарков"),
            "No active stories" => ("", "Нет активных историй"),
            "No available stories" => ("", "Нет доступных историй"),
            "No matching chats" => ("", "Чаты не найдены"),
            "No messages yet" => ("", "Сообщений пока нет"),
            "All reactions..." => ("lng_manage_peer_reactions_all", "Все реакции..."),
            "Remove my reactions" => ("", "Убрать мои реакции"),
            "Copy text" => ("", "Копировать текст"),
            "Forward…" => ("", "Переслать…"),
            "Delete…" => ("", "Удалить…"),
            "Unpin" => ("lng_context_unpin_msg", "Открепить"),
            "Pin" => ("lng_context_pin_msg", "Закрепить"),
            "Ready" => ("", "Готово"),
            "Sent" => ("lng_sr_chat_sent", "Отправлено"),
            "Copied" => ("", "Скопировано"),
            "Username copied" => ("", "Имя пользователя скопировано"),
            "Message deleted" => ("", "Сообщение удалено"),
            "Reactions updated" => ("", "Реакции обновлены"),
            "Reaction removed" => ("", "Реакция снята"),
            "Language applied" => ("", "Язык применён"),
            "Invalid language pack code or link" => ("", "Неверный код языка или ссылка"),
            _ => return english,
        };
        self.label(key, english, russian)
    }
}

impl App {
    pub(super) fn load_languages(&mut self) {
        if self.settings.languages_loaded || self.settings.languages_loading {
            return;
        }
        self.settings.languages_loading = true;
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        tokio::spawn(async move {
            let result = client
                .invoke(&tl::functions::langpack::GetLanguages {
                    lang_pack: "tdesktop".into(),
                })
                .await
                .map(|languages| languages.into_iter().map(Into::into).collect())
                .map_err(|error| error.to_string());
            let _ = tx.send(UiEvent::Language(Event::Languages(result)));
        });
    }

    pub(super) fn request_language(&mut self, source: String) {
        let code = source
            .trim()
            .trim_start_matches("https://t.me/setlanguage/")
            .trim_start_matches("t.me/setlanguage/")
            .to_owned();
        if code.is_empty()
            || code.len() > 64
            || !code
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
        {
            self.status = "Invalid language pack code or link".into();
            self.status_error = true;
            return;
        }
        self.settings.language_request = self.settings.language_request.wrapping_add(1);
        let request = self.settings.language_request;
        if code == "en" {
            self.language_event(Event::Language {
                request,
                code,
                result: Ok(("English".into(), HashMap::new())),
            });
            return;
        }
        self.status = "Loading language pack…".into();
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        tokio::spawn(async move {
            let result = async {
                let language: tl::types::LangPackLanguage = client
                    .invoke(&tl::functions::langpack::GetLanguage {
                        lang_pack: "tdesktop".into(),
                        lang_code: code.clone(),
                    })
                    .await?
                    .into();
                let pack: tl::types::LangPackDifference = client
                    .invoke(&tl::functions::langpack::GetLangPack {
                        lang_pack: "tdesktop".into(),
                        lang_code: code.clone(),
                    })
                    .await?
                    .into();
                let strings = pack
                    .strings
                    .into_iter()
                    .filter_map(|item| match item {
                        tl::enums::LangPackString::String(item) => Some((item.key, item.value)),
                        _ => None,
                    })
                    .collect();
                Ok::<_, grammers_client::InvocationError>((language.native_name, strings))
            }
            .await
            .map_err(|error| error.to_string());
            let _ = tx.send(UiEvent::Language(Event::Language {
                request,
                code,
                result,
            }));
        });
    }

    pub(super) fn language_event(&mut self, event: Event) {
        match event {
            Event::Languages(result) => {
                self.settings.languages_loading = false;
                match result {
                    Ok(languages) => {
                        self.settings.languages = languages;
                        self.settings.languages_loaded = true;
                    }
                    Err(error) => self.set_error(error),
                }
            }
            Event::Language {
                request,
                code,
                result,
            } if request == self.settings.language_request => match result {
                Ok((name, strings)) => {
                    self.settings.language_code = code.clone();
                    self.settings.language_name = name;
                    self.settings.strings = strings;
                    self.menu_overlay_cache.borrow_mut().take();
                    if let Err(error) = fs::write(self.settings_root.join("language"), code) {
                        self.set_error(error);
                    } else {
                        self.status = "Language applied".into();
                        self.status_error = false;
                    }
                }
                Err(error) => self.set_error(error),
            },
            _ => {}
        }
    }

    pub(super) fn handle_settings_input(&mut self, key: i32, terminal: &Terminal) -> bool {
        if !self.menu_open || self.theme_preview.is_some() {
            return false;
        }
        if let Some(code) = &mut self.settings.language_input {
            match key {
                27 => self.settings.language_input = None,
                8 | 127 => pop_grapheme(code),
                10 | 13 => {
                    let code = self.settings.language_input.take().unwrap_or_default();
                    self.request_language(code);
                }
                _ => {
                    if let Some(text) = input_text(terminal)
                        && !text.chars().any(char::is_control)
                    {
                        code.push_str(&text);
                    }
                }
            }
            return true;
        }
        if let Some(path) = &mut self.settings.path {
            match key {
                27 => self.settings.path = None,
                8 | 127 => pop_grapheme(path),
                10 | 13 => {
                    let source = self.settings.path.take().unwrap_or_default();
                    if !source.trim().is_empty() {
                        self.request_theme(source.trim().to_owned());
                    }
                }
                _ => {
                    if let Some(text) = input_text(terminal)
                        && !text.chars().any(char::is_control)
                    {
                        path.push_str(&text);
                    }
                }
            }
            return true;
        }
        match key {
            27 => self.menu_open = false,
            KEY_LEFT => self.settings.focused_nav = true,
            9 => self.settings.focused_nav = !self.settings.focused_nav,
            KEY_RIGHT => self.settings.focused_nav = false,
            KEY_UP | KEY_DOWN if self.settings.focused_nav => {
                let current = TABS
                    .iter()
                    .position(|(t, _)| *t == self.settings.tab)
                    .unwrap_or(0);
                let next = if key == KEY_UP {
                    (current + TABS.len() - 1) % TABS.len()
                } else {
                    (current + 1) % TABS.len()
                };
                self.settings.tab = TABS[next].0;
                self.settings.selected = 0;
                if self.settings.tab == Tab::Languages {
                    self.load_languages();
                }
            }
            KEY_UP => self.settings.selected = self.settings.selected.saturating_sub(1),
            KEY_DOWN => self.settings.selected += 1,
            10 | 13 => {
                if self.settings.focused_nav {
                    self.settings.focused_nav = false;
                    return true;
                }
                let actions: Vec<_> = self
                    .hitboxes
                    .iter()
                    .filter(|h| actionable(&h.target))
                    .map(|h| h.target.clone())
                    .collect();
                if let Some(action) = actions
                    .get(self.settings.selected % actions.len().max(1))
                    .cloned()
                {
                    self.setting_action(action);
                }
            }
            MOUSE_EVENT => {
                let mouse = terminal.getmouse();
                match mouse.event {
                    MouseEvent::LEFT => match self.hit_test(mouse.x, mouse.y) {
                        Some(HitTarget::SettingsPanel) => {}
                        Some(HitTarget::SettingsTab(tab)) => {
                            self.settings.tab = tab;
                            self.settings.focused_nav = true;
                            self.settings.selected = 0;
                            if tab == Tab::Languages {
                                self.load_languages();
                            }
                        }
                        Some(HitTarget::CloseSettings) => self.menu_open = false,
                        Some(action) if actionable(&action) => {
                            self.settings.focused_nav = false;
                            self.setting_action(action);
                        }
                        _ => self.menu_open = false,
                    },
                    MouseEvent::WHEELUP => {
                        self.settings.selected = self.settings.selected.saturating_sub(1)
                    }
                    MouseEvent::WHEELDOWN => self.settings.selected += 1,
                    _ => {}
                }
            }
            _ => {}
        }
        true
    }
    fn setting_action(&mut self, action: HitTarget) {
        match action {
            HitTarget::Theme(source) => self.request_theme(source),
            HitTarget::ThemeBackground => {
                self.theme.transparent = !self.theme.transparent;
                if let Err(e) = self.theme.save(&self.settings_root) {
                    self.set_error(e);
                }
            }
            HitTarget::NerdFonts => {
                self.nerd_fonts = !self.nerd_fonts;
                if let Err(e) = fs::write(
                    self.settings_root.join("nerd-fonts"),
                    if self.nerd_fonts { "1" } else { "0" },
                ) {
                    self.set_error(e);
                }
            }
            HitTarget::ImportTheme => self.settings.path = Some(String::new()),
            HitTarget::Language(code) => self.request_language(code),
            HitTarget::LanguageInput => self.settings.language_input = Some(String::new()),
            HitTarget::Folder(id) => {
                self.menu_open = false;
                self.switch_folder(id);
            }
            HitTarget::SettingsToggle(toggle) => {
                match toggle {
                    Toggle::Autoplay => self.settings.autoplay = !self.settings.autoplay,
                    Toggle::Animation => self.settings.animation = !self.settings.animation,
                    Toggle::Sidebar => self.sidebar_collapsed = !self.sidebar_collapsed,
                }
                if let Err(e) = self.settings.save(&self.settings_root) {
                    self.set_error(e);
                }
            }
            _ => {}
        }
    }
}

fn actionable(target: &HitTarget) -> bool {
    matches!(
        target,
        HitTarget::Theme(_)
            | HitTarget::ThemeBackground
            | HitTarget::NerdFonts
            | HitTarget::ImportTheme
            | HitTarget::SettingsToggle(_)
            | HitTarget::Folder(_)
            | HitTarget::Language(_)
            | HitTarget::LanguageInput
    )
}

pub(super) fn bounds(width: i32, height: i32) -> Rect {
    let w = width.clamp(0, 74);
    let h = height.clamp(0, 26);
    Rect {
        x: (width - w) / 2,
        y: (height - h) / 2,
        width: w,
        height: h,
    }
}

pub(super) fn draw(window: &mut Window, app: &App, hits: &mut Vec<HitBox>) {
    let accent = CellStyle::new(
        Some(app.theme.palette.accent),
        Some(app.theme.palette.window),
    );
    let plain = CellStyle::new(Some(app.theme.palette.text), Some(app.theme.palette.window));
    put(
        window,
        0,
        2,
        app.settings.ui("Settings"),
        accent,
        window.width - 8,
    );
    put(window, 0, window.width - 4, "×", accent, 2);
    hits.push(HitBox {
        rect: Rect {
            x: window.x,
            y: window.y,
            width: window.width,
            height: window.height,
        },
        target: HitTarget::SettingsPanel,
    });
    hits.push(HitBox {
        rect: Rect {
            x: window.x + window.width - 4,
            y: window.y + 1,
            width: 3,
            height: 1,
        },
        target: HitTarget::CloseSettings,
    });
    let side_by_side = window.width >= 52;
    let content_x = if side_by_side { 21 } else { 2 };
    let content_y = if side_by_side { 2 } else { 9 };
    let content_width = (window.width - content_x - 2).max(1);
    for (i, (tab, _)) in TABS.into_iter().enumerate() {
        let col = 2;
        let row = if side_by_side {
            2 + i as i32 * 3
        } else {
            2 + i as i32
        };
        let active = tab == app.settings.tab;
        let label = format!(
            "{} {}",
            if active { "›" } else { " " },
            app.settings.tab_label(tab)
        );
        put(
            window,
            row,
            col,
            &label,
            if active { accent } else { plain },
            if side_by_side { 17 } else { window.width - 6 },
        );
        hits.push(HitBox {
            rect: Rect {
                x: window.x + 1 + col,
                y: window.y + 1 + row,
                width: if side_by_side { 17 } else { window.width - 6 },
                height: 1,
            },
            target: HitTarget::SettingsTab(tab),
        });
    }
    if side_by_side {
        for row in 1..window.height.saturating_sub(3) {
            window.posstyled(
                row,
                19,
                '│',
                CellStyle::new(
                    Some(app.theme.palette.border),
                    Some(app.theme.palette.window),
                ),
            );
        }
    }
    if let Some(path) = &app.settings.path {
        put(
            window,
            content_y + 1,
            content_x,
            app.settings.label(
                "",
                "Theme path or t.me/addtheme URL",
                "Путь к теме или ссылка t.me/addtheme",
            ),
            accent,
            content_width,
        );
        put(
            window,
            content_y + 3,
            content_x,
            &format!("{path}█"),
            plain,
            content_width,
        );
        put(
            window,
            content_y + 5,
            content_x,
            app.settings.label(
                "",
                "Enter import · Esc cancel",
                "Enter — импорт · Esc — отмена",
            ),
            plain,
            content_width,
        );
        return;
    }
    if let Some(code) = &app.settings.language_input {
        put(
            window,
            content_y + 1,
            content_x,
            app.settings.label(
                "",
                "Language code or t.me/setlanguage link",
                "Код языка или ссылка t.me/setlanguage",
            ),
            accent,
            content_width,
        );
        put(
            window,
            content_y + 3,
            content_x,
            &format!("{code}█"),
            plain,
            content_width,
        );
        put(
            window,
            content_y + 5,
            content_x,
            app.settings.label(
                "",
                "Enter apply · Esc cancel",
                "Enter — применить · Esc — отмена",
            ),
            plain,
            content_width,
        );
        return;
    }
    let entries: Vec<(String, Option<HitTarget>)> = match app.settings.tab {
        Tab::Appearance => {
            draw_appearance(window, app, hits, content_x, content_y, content_width);
            Vec::new()
        }
        Tab::Media => vec![
            (
                format!(
                    "[{}] {}",
                    mark(app.settings.autoplay),
                    app.settings
                        .label("", "Autoplay GIF", "Автовоспроизведение GIF")
                ),
                Some(HitTarget::SettingsToggle(Toggle::Autoplay)),
            ),
            (
                format!(
                    "[{}] {}",
                    mark(app.settings.animation),
                    app.settings.label(
                        "",
                        "Animate stickers / custom emoji",
                        "Анимация стикеров и эмодзи"
                    )
                ),
                Some(HitTarget::SettingsToggle(Toggle::Animation)),
            ),
            (
                format!(
                    "TGP: {}",
                    if app.tgp_supported {
                        app.settings.label("", "supported", "поддерживается")
                    } else {
                        app.settings.label(
                            "",
                            "not supported; text fallback",
                            "недоступен; текстовая замена",
                        )
                    }
                ),
                None,
            ),
            (
                app.settings
                    .label(
                        "",
                        "LaTeX: two-dimensional terminal formulas",
                        "LaTeX: двумерные формулы в терминале",
                    )
                    .into(),
                None,
            ),
        ],
        Tab::Folders => app
            .folders
            .iter()
            .map(|f| (f.name.clone(), Some(HitTarget::Folder(f.id))))
            .chain(std::iter::once((
                format!(
                    "[{}] {}",
                    mark(app.sidebar_collapsed),
                    app.settings
                        .label("", "Compact chats", "Компактный список чатов")
                ),
                Some(HitTarget::SettingsToggle(Toggle::Sidebar)),
            )))
            .collect(),
        Tab::Storage => vec![
            (
                app.settings
                    .label(
                        "",
                        "Persistent media: avatars only",
                        "Постоянные медиа: только аватары",
                    )
                    .into(),
                None,
            ),
            (
                format!(
                    "{}: {}",
                    app.settings.label("", "Cache", "Кеш"),
                    app.settings_root.display()
                ),
                None,
            ),
            (
                app.settings
                    .label(
                        "",
                        "Messages and previews stay in RAM while the chat is open.",
                        "Сообщения и превью хранятся в памяти, пока открыт чат.",
                    )
                    .into(),
                None,
            ),
            (
                app.settings
                    .label(
                        "",
                        "Temporary media is removed when its view closes.",
                        "Временные медиа удаляются после закрытия просмотра.",
                    )
                    .into(),
                None,
            ),
            (
                format!(
                    "{}: {}",
                    app.settings.label("", "Diagnostics", "Диагностика"),
                    app.log_path.display()
                ),
                None,
            ),
        ],
        Tab::Controls => vec![
            (
                app.settings
                    .label(
                        "",
                        "Enter sends · Shift+Enter inserts a newline",
                        "Enter отправляет · Shift+Enter добавляет строку",
                    )
                    .into(),
                None,
            ),
            (
                app.settings
                    .label(
                        "",
                        "Esc leaves the composer; sending keeps input focus",
                        "Esc выходит из ввода; отправка сохраняет фокус",
                    )
                    .into(),
                None,
            ),
            (
                app.settings
                    .label(
                        "",
                        "Right click: message menu · double click: Reply",
                        "ПКМ: меню сообщения · двойной клик: ответ",
                    )
                    .into(),
                None,
            ),
            (
                app.settings
                    .label(
                        "",
                        "Ctrl+E emoji/stickers · Ctrl+V paste files",
                        "Ctrl+E эмодзи/стикеры · Ctrl+V вставка файлов",
                    )
                    .into(),
                None,
            ),
            (
                app.settings
                    .label(
                        "",
                        "Arrow keys scroll messages · latest jumps to the tail",
                        "Стрелки листают сообщения · Latest ведёт к концу",
                    )
                    .into(),
                None,
            ),
            (
                app.settings
                    .label(
                        "",
                        "Esc closes a modal, then chat; q/Esc exit from All chats",
                        "Esc закрывает окно и чат; q/Esc выходят из Всех чатов",
                    )
                    .into(),
                None,
            ),
        ],
        Tab::Languages => {
            let mut entries = vec![
                (
                    format!(
                        "{}: {} ({})",
                        app.settings.label("", "Current", "Текущий"),
                        app.settings.language_name,
                        app.settings.language_code
                    ),
                    None,
                ),
                ("English".into(), Some(HitTarget::Language("en".into()))),
                ("Русский".into(), Some(HitTarget::Language("ru".into()))),
            ];
            for language in app
                .settings
                .languages
                .iter()
                .filter(|language| language.lang_code != "en" && language.lang_code != "ru")
                .take(9)
            {
                entries.push((
                    language.native_name.clone(),
                    Some(HitTarget::Language(language.lang_code.clone())),
                ));
            }
            entries.push((
                app.settings
                    .label(
                        "",
                        "Custom language pack / link…",
                        "Свой языковой пакет / ссылка…",
                    )
                    .into(),
                Some(HitTarget::LanguageInput),
            ));
            entries
        }
    };
    for (row, (label, target)) in entries
        .into_iter()
        .take(window.height.saturating_sub(content_y + 3) as usize)
        .enumerate()
    {
        let y = content_y + 1 + row as i32;
        put(window, y, content_x, &label, plain, content_width);
        if let Some(target) = target {
            hits.push(HitBox {
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
    let actions: Vec<_> = hits.iter().filter(|h| actionable(&h.target)).collect();
    if !app.settings.focused_nav
        && let Some(hit) = actions.get(app.settings.selected % actions.len().max(1))
    {
        put(
            window,
            hit.rect.y - window.y - 1,
            content_x - 1,
            "›",
            accent,
            1,
        );
    }
}
fn mark(value: bool) -> &'static str {
    if value { "x" } else { " " }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn official_pack_strings_and_russian_fallback_localize_controls() {
        let mut state = State {
            language_code: "xx".into(),
            ..State::default()
        };
        state
            .strings
            .insert("lng_menu_settings".into(), "Paramètres".into());
        state
            .strings
            .insert("lng_settings_section_filters".into(), "Dossiers".into());
        assert_eq!(state.ui("Settings"), "Paramètres");
        assert_eq!(state.tab_label(Tab::Folders), "Dossiers");
        state.language_code = "ru".into();
        assert_eq!(state.ui("No attachments"), "Нет вложений");
    }
    #[test]
    fn settings_overlay_fits_small_and_large_terminals() {
        for (w, h) in [(140, 45), (60, 20), (8, 4)] {
            let r = bounds(w, h);
            assert!(r.x >= 0 && r.y >= 0 && r.x + r.width <= w && r.y + r.height <= h);
        }
        assert!(State::default().autoplay && State::default().animation);
    }
}
