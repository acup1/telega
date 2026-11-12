use super::*;

#[derive(Default)]
pub(super) struct State {
    pub query: String,
    pub results: Vec<Message>,
    pub selected: usize,
    pub loading: bool,
    pub has_more: bool,
    request: Option<u64>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl State {
    fn cancel_request(&mut self) {
        self.request = None;
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.loading = false;
    }

    fn accepts(&self, request: u64) -> bool {
        self.request == Some(request)
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.cancel_request();
    }
}

impl App {
    fn request_chat_search(&mut self, append: bool, debounce: bool) {
        let Some(peer) = self.current_peer() else {
            return;
        };
        let Some(search) = &mut self.chat_search else {
            return;
        };
        if append && (search.loading || !search.has_more) {
            return;
        }
        search.cancel_request();
        if !append {
            search.results.clear();
            search.selected = 0;
            search.has_more = false;
        }
        if search.query.trim().is_empty() {
            return;
        }
        let offset = if append {
            search.results.last().map(Message::id).unwrap_or(0)
        } else {
            0
        };
        search.loading = true;
        self.search_request = self.search_request.wrapping_add(1);
        let request = self.search_request;
        search.request = Some(request);
        let query = search.query.clone();
        let client = self.telegram.client.clone();
        let tx = self.events_tx.clone();
        let view_id = self.view_id;
        search.task = Some(tokio::spawn(async move {
            if debounce {
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            let mut iter = client.search_messages(peer).query(&query).offset_id(offset);
            let result = async {
                let mut messages = Vec::new();
                for _ in 0..15 {
                    let Some(message) = iter.next().await? else {
                        break;
                    };
                    messages.push(message);
                }
                let has_more = messages.len() == 15;
                Ok((messages, has_more))
            }
            .await
            .map_err(|error: grammers_client::InvocationError| error.to_string());
            let _ = tx.send(UiEvent::ChatSearchResults {
                view_id,
                request,
                append,
                result,
            });
        }));
    }

    pub(super) fn chat_search_event(
        &mut self,
        view_id: u64,
        request: u64,
        append: bool,
        result: std::result::Result<(Vec<Message>, bool), String>,
    ) {
        if view_id != self.view_id || request != self.search_request {
            return;
        }
        let Some(search) = &mut self.chat_search else {
            return;
        };
        if !search.accepts(request) {
            return;
        }
        search.request = None;
        search.task = None;
        search.loading = false;
        match result {
            Ok((messages, has_more)) => {
                if !append {
                    search.results.clear();
                    search.selected = 0;
                }
                let mut known: HashSet<_> = search.results.iter().map(Message::id).collect();
                search
                    .results
                    .extend(messages.into_iter().filter(|m| known.insert(m.id())));
                search.has_more = has_more;
                self.status = if search.results.is_empty() {
                    "No messages found"
                } else {
                    "Search results ready"
                }
                .into();
            }
            Err(error) => self.set_error(error),
        }
    }

    pub(super) fn handle_chat_search_input(&mut self, key: i32, terminal: &mut Terminal) -> bool {
        if key == 6
            && self.open_chat.is_some()
            && !self.menu_open
            && self.deleting.is_none()
            && self.pending_attachments.is_none()
            && !self.attaching
            && self.saving.is_none()
            && self.theme_preview.is_none()
            && !self.theme_input
        {
            if self.chat_search.is_none() {
                self.chat_search = Some(State::default());
            }
            return true;
        }
        if self.chat_search.is_none() {
            return false;
        }
        if key == MOUSE_EVENT {
            let mouse = terminal.getmouse();
            if mouse.event == MouseEvent::LEFT
                && matches!(self.hit_test(mouse.x, mouse.y), Some(HitTarget::Composer))
            {
                self.chat_search = None;
                return false;
            }
        }
        match key {
            27 => self.chat_search = None,
            8 | 127 => {
                pop_grapheme(&mut self.chat_search.as_mut().unwrap().query);
                self.request_chat_search(false, true);
            }
            KEY_UP | KEY_DOWN => {
                let search = self.chat_search.as_mut().unwrap();
                search.selected = if key == KEY_UP {
                    search.selected.saturating_sub(1)
                } else {
                    (search.selected + 1).min(search.results.len().saturating_sub(1))
                };
            }
            10 | 13 => {
                if let Some(id) = self
                    .chat_search
                    .as_ref()
                    .and_then(|s| s.results.get(s.selected))
                    .map(Message::id)
                {
                    self.chat_search = None;
                    self.jump_to_message(id);
                } else {
                    self.request_chat_search(false, false);
                }
            }
            KEY_PASTE => {
                if let Some(value) = terminal.take_paste() {
                    self.chat_search.as_mut().unwrap().query.push_str(&value);
                    self.request_chat_search(false, true);
                }
            }
            MOUSE_EVENT => {
                let mouse = terminal.getmouse();
                if mouse.event == MouseEvent::LEFT {
                    match self.hit_test(mouse.x, mouse.y) {
                        Some(HitTarget::ChatSearchClose) => self.chat_search = None,
                        Some(HitTarget::ChatSearchMore) => self.request_chat_search(true, false),
                        Some(HitTarget::ChatSearchResult(index)) => {
                            if let Some(id) = self
                                .chat_search
                                .as_ref()
                                .and_then(|s| s.results.get(index))
                                .map(Message::id)
                            {
                                self.chat_search = None;
                                self.jump_to_message(id);
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {
                if let Some(text) = input_text(terminal)
                    && !text.chars().any(char::is_control)
                {
                    self.chat_search.as_mut().unwrap().query.push_str(&text);
                    self.request_chat_search(false, true);
                }
            }
        }
        true
    }
}

pub(super) fn draw(window: &mut Window, app: &App, hits: &mut Vec<HitBox>) {
    let Some(search) = &app.chat_search else {
        return;
    };
    window.border = 0;
    window.border_style = CellStyle::new(
        Some(app.theme.palette.border),
        Some(app.theme.palette.window),
    );
    window.name = Some(
        app.settings
            .label("", " Search messages ", " Поиск сообщений ")
            .into(),
    );
    window.clear();
    clear_area(window);
    let text = CellStyle::foreground(app.theme.palette.text);
    let accent = CellStyle::foreground(app.theme.palette.accent);
    put(
        window,
        0,
        1,
        &format!("⌕ {}█", search.query),
        accent,
        window.width - 6,
    );
    put(window, 0, window.width - 4, "×", accent, 2);
    hits.push(HitBox {
        rect: Rect {
            x: window.x + window.width - 4,
            y: window.y + 1,
            width: 3,
            height: 1,
        },
        target: HitTarget::ChatSearchClose,
    });
    let visible = window.height.saturating_sub(4).max(0) as usize;
    let start = search.selected.saturating_sub(visible.saturating_sub(1));
    for (row, (index, message)) in search
        .results
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .enumerate()
    {
        let snippet = if message.text().is_empty() {
            media::label(message).unwrap_or_else(|| "Media".to_owned())
        } else {
            message.text().replace('\n', " ")
        };
        put(
            window,
            row as i32 + 1,
            1,
            if index == search.selected { "›" } else { " " },
            accent,
            2,
        );
        put(
            window,
            row as i32 + 1,
            4,
            &format!("{}  {}", message.id(), snippet),
            text,
            window.width - 8,
        );
        hits.push(HitBox {
            rect: Rect {
                x: window.x + 1,
                y: window.y + row as i32 + 2,
                width: window.width - 2,
                height: 1,
            },
            target: HitTarget::ChatSearchResult(index),
        });
    }
    let footer = if search.loading {
        "Searching…"
    } else if search.has_more {
        "More results ↓"
    } else if search.results.is_empty() {
        "Type to search"
    } else {
        "Enter: jump to result"
    };
    put(
        window,
        window.height - 3,
        2,
        app.settings.label(
            "",
            footer,
            match footer {
                "Searching…" => "Поиск…",
                "More results ↓" => "Ещё результаты ↓",
                "Type to search" => "Введите запрос",
                _ => "Enter: перейти к результату",
            },
        ),
        accent,
        window.width - 6,
    );
    if search.has_more && !search.loading {
        hits.push(HitBox {
            rect: Rect {
                x: window.x + 2,
                y: window.y + window.height - 2,
                width: window.width - 4,
                height: 1,
            },
            target: HitTarget::ChatSearchMore,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending_search() -> (State, tokio::sync::oneshot::Receiver<()>) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let state = State {
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            has_more: false,
            request: Some(42),
            loading: true,
            task: Some(tokio::spawn(async move {
                std::future::pending::<()>().await;
                let _ = sender.send(());
            })),
        };
        (state, receiver)
    }

    #[tokio::test]
    async fn clearing_or_closing_search_cancels_pending_query_and_rejects_its_response() {
        let (mut state, cancelled) = pending_search();
        assert!(state.accepts(42));
        state.cancel_request();
        assert!(!state.accepts(42));
        assert!(!state.loading);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), cancelled)
                .await
                .unwrap()
                .is_err()
        );

        let (state, closed) = pending_search();
        drop(state);
        assert!(
            tokio::time::timeout(Duration::from_secs(1), closed)
                .await
                .unwrap()
                .is_err()
        );
        assert!(!State::default().accepts(42));
    }
}
