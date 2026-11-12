use super::*;

#[derive(Clone)]
pub(super) struct State {
    cursor: usize,
    preferred: Option<usize>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            cursor: usize::MAX,
            preferred: None,
        }
    }
}
#[derive(Clone, Copy)]
struct Stop {
    byte: usize,
    row: usize,
    column: usize,
}
pub(super) struct Layout {
    pub lines: Vec<String>,
    pub positions: HashMap<usize, code::Position>,
    stops: Vec<Stop>,
}

pub(super) fn text_width(window_width: i32) -> usize {
    window_width.saturating_sub(13).max(2) as usize
}

pub(super) fn layout(text: &str, width: usize) -> Layout {
    let width = width.max(2);
    let mut layout = Layout {
        lines: vec![String::new()],
        positions: HashMap::new(),
        stops: Vec::new(),
    };
    let (mut row, mut column, mut offset) = (0, 0, 0);
    for (byte, grapheme) in UnicodeSegmentation::grapheme_indices(text, true) {
        let length = grapheme.encode_utf16().count();
        let newline = grapheme == "\n" || grapheme == "\r\n";
        let cells = if grapheme == "\t" {
            4 - column % 4
        } else {
            display_width(grapheme).max(1)
        };
        if !newline && column + cells > width && column > 0 {
            row += 1;
            column = 0;
            layout.lines.push(String::new());
        }
        layout.stops.push(Stop { byte, row, column });
        if newline {
            row += 1;
            column = 0;
            layout.lines.push(String::new());
        } else {
            layout.positions.insert(
                offset,
                code::Position {
                    row,
                    column,
                    cells,
                    length,
                    code: false,
                },
            );
            if grapheme == "\t" {
                layout.lines[row].push_str(&" ".repeat(cells));
            } else {
                layout.lines[row].push_str(grapheme);
            }
            column += cells;
        }
        offset += length;
    }
    if column >= width {
        row += 1;
        column = 0;
        layout.lines.push(String::new());
    }
    layout.stops.push(Stop {
        byte: text.len(),
        row,
        column,
    });
    layout
}

impl State {
    pub(super) fn activate_at(
        &mut self,
        focused: &mut bool,
        text: &str,
        width: usize,
        area: Rect,
        point: (u16, u16),
    ) {
        *focused = true;
        let row = (i32::from(point.1) - area.y).clamp(0, area.height.saturating_sub(1).max(0));
        let column = (i32::from(point.0) - area.x).clamp(0, width as i32);
        self.place_cursor(
            text,
            width,
            area.height.max(1) as usize,
            row as usize,
            column as usize,
        );
    }

    pub(super) fn place_cursor(
        &mut self,
        text: &str,
        width: usize,
        rows: usize,
        row: usize,
        column: usize,
    ) {
        let layout = layout(text, width);
        let visible_rows = rows.max(1);
        let start = self
            .stop(text, &layout)
            .row
            .saturating_sub(visible_rows - 1);
        let target_row = start + row;
        let stop = layout
            .stops
            .iter()
            .filter(|stop| stop.row == target_row)
            .min_by_key(|stop| stop.column.abs_diff(column))
            .or_else(|| layout.stops.last());
        if let Some(stop) = stop {
            self.cursor = stop.byte;
            self.preferred = None;
        }
    }

    pub(super) fn byte(&self, text: &str) -> usize {
        let cursor = self.cursor.min(text.len());
        UnicodeSegmentation::grapheme_indices(text, true)
            .map(|(byte, _)| byte)
            .chain(std::iter::once(text.len()))
            .take_while(|byte| *byte <= cursor)
            .last()
            .unwrap_or(0)
    }
    fn stop(&self, text: &str, layout: &Layout) -> Stop {
        let byte = self.byte(text);
        *layout
            .stops
            .iter()
            .find(|s| s.byte == byte)
            .unwrap_or_else(|| layout.stops.last().unwrap())
    }
    pub(super) fn on_first_line(&self, text: &str, width: usize) -> bool {
        self.stop(text, &layout(text, width)).row == 0
    }
    pub(super) fn move_cursor(&mut self, key: i32, text: &str, width: usize) {
        let layout = layout(text, width);
        let current = self.stop(text, &layout);
        let index = layout
            .stops
            .iter()
            .position(|s| s.byte == current.byte)
            .unwrap();
        let target = match key {
            KEY_LEFT => {
                self.preferred = None;
                layout.stops[index.saturating_sub(1)]
            }
            KEY_RIGHT => {
                self.preferred = None;
                layout.stops[(index + 1).min(layout.stops.len() - 1)]
            }
            KEY_UP | KEY_DOWN => {
                let column = *self.preferred.get_or_insert(current.column);
                let row = if key == KEY_UP {
                    current.row.saturating_sub(1)
                } else {
                    current.row + 1
                };
                *layout
                    .stops
                    .iter()
                    .filter(|s| s.row == row)
                    .min_by_key(|s| s.column.abs_diff(column))
                    .unwrap_or(&current)
            }
            _ => current,
        };
        self.cursor = target.byte;
    }
    pub(super) fn insert(
        &mut self,
        text: &mut String,
        draft: &mut Vec<crate::custom_emoji::Draft>,
        value: &str,
    ) {
        let byte = self.byte(text);
        let at = text[..byte].encode_utf16().count() as i32;
        let delta = value.encode_utf16().count() as i32;
        draft.retain(|emoji| !(emoji.offset < at && emoji.offset + emoji.length > at));
        for emoji in draft {
            if emoji.offset >= at {
                emoji.offset += delta;
            }
        }
        text.insert_str(byte, value);
        self.cursor = byte + value.len();
        self.preferred = None;
    }
    pub(super) fn backspace(
        &mut self,
        text: &mut String,
        draft: &mut Vec<crate::custom_emoji::Draft>,
    ) {
        let end = self.byte(text);
        let Some((start, grapheme)) =
            UnicodeSegmentation::grapheme_indices(&text[..end], true).next_back()
        else {
            return;
        };
        let at = text[..start].encode_utf16().count() as i32;
        let length = grapheme.encode_utf16().count() as i32;
        draft.retain(|emoji| emoji.offset + emoji.length <= at || emoji.offset >= at + length);
        for emoji in draft {
            if emoji.offset >= at + length {
                emoji.offset -= length;
            }
        }
        text.replace_range(start..end, "");
        self.cursor = start;
        self.preferred = None;
    }
}

impl App {
    pub(super) fn activate_composer_at(&mut self, x: u16, y: u16) {
        if !self.require_write() {
            return;
        }
        if let Some(area) = self
            .hitboxes
            .iter()
            .rev()
            .find(|hit| matches!(hit.target, HitTarget::Composer))
            .map(|hit| hit.rect)
        {
            self.message_menu = None;
            self.forwarding = None;
            self.attachment_menu = false;
            if self.details.is_some() {
                self.close_details();
            }
            self.composer.activate_at(
                &mut self.composing,
                &self.input,
                self.composer_width,
                area,
                (x, y),
            );
        }
    }

    pub(super) fn insert_draft_text(&mut self, text: &str) {
        self.composer
            .insert(&mut self.input, &mut self.draft_custom, text);
    }
}

pub(super) fn height(app: &App, window_width: i32) -> i32 {
    if !app.can_compose() && !app.theme_input && !app.attaching && app.saving.is_none() {
        return 0;
    }
    let rows = layout(&app.input, text_width(window_width))
        .lines
        .len()
        .clamp(1, 5) as i32;
    rows + 2 + i32::from(app.reply_to.is_some() || app.editing.is_some())
}

pub(super) fn draw(
    window: &mut Window,
    app: &App,
    hits: &mut Vec<HitBox>,
    images: &mut Vec<ImagePlacement>,
) -> Option<(u16, u16)> {
    window.border = 0;
    window.border_style = app.theme.style(
        if app.composing {
            app.theme.palette.accent
        } else {
            app.theme.palette.border
        },
        app.theme.palette.chat,
    );
    window.name = Some(fit_label(
        if app.status_error {
            app.settings.ui(&app.status)
        } else if app.editing.is_some() {
            app.settings
                .label("", " Edit message ", " Изменение сообщения ")
        } else if app.composing {
            app.settings.label("", " Message ", " Сообщение ")
        } else {
            app.settings
                .label("", " Message · e to write ", " Сообщение · e для ввода ")
        },
        window.width.saturating_sub(6).max(0) as usize,
    ));
    window.clear();
    clear_area(window);
    let plain = CellStyle::foreground(app.theme.palette.text);
    let accent = CellStyle::foreground(app.theme.palette.accent);
    let inner = window.width - 2;
    if app.theme_input || app.attaching || app.saving.is_some() {
        put(window, 0, 1, &format!("{}█", app.input), plain, inner - 2);
        return None;
    }
    let top = i32::from(app.reply_to.is_some() || app.editing.is_some());
    if top > 0 {
        let label = if let Some(editing) = &app.editing {
            format!(
                "{} · {}",
                app.settings.ui("Edit"),
                editing.original.text().replace('\n', " ")
            )
        } else {
            let reply = app
                .reply_to
                .and_then(|id| app.messages.iter().find(|m| m.id() == id));
            format!(
                "{} · {}",
                app.settings.ui("Reply"),
                reply.map_or("message".to_owned(), |m| m.text().replace('\n', " "))
            )
        };
        put(window, 0, 4, &label, accent, inner - 8);
        put(window, 0, inner - 3, "×", accent, 2);
    }
    let width = text_width(window.width);
    let layout = layout(&app.input, width);
    let cursor = app.composer.stop(&app.input, &layout);
    let capacity = (window.height - 2 - top).max(1) as usize;
    let start = cursor.row.saturating_sub(capacity - 1);
    for (row, line) in layout.lines.iter().skip(start).take(capacity).enumerate() {
        put(window, top + row as i32, 4, line, plain, width as i32);
    }
    if app.input.is_empty() && !app.composing {
        put(
            window,
            top,
            4,
            app.settings
                .label("lng_message_ph", "Write a message…", "Написать сообщение…"),
            CellStyle::foreground(app.theme.palette.muted),
            width as i32,
        );
    }
    inline_emoji::draw_draft(
        window,
        app,
        &layout.positions,
        start,
        TextAreaSpec {
            top,
            width,
            lines: capacity,
        },
        images,
    );
    add_mouse_targets(window, top, width, capacity, hits);
    for (x, label) in [
        (0, if app.nerd_fonts { "\u{f0c6}" } else { "+" }),
        (inner - 7, if app.nerd_fonts { "\u{f118}" } else { ":)" }),
        (inner - 3, if app.nerd_fonts { "\u{f1d8}" } else { ">" }),
    ] {
        if x >= 0 {
            put(window, top, x, label, accent, 2);
        }
    }
    app.composing.then_some((
        (window.y + 1 + top + cursor.row as i32 - start as i32).max(0) as u16,
        (window.x + 1 + 4 + cursor.column as i32).max(0) as u16,
    ))
}

fn add_mouse_targets(window: &Window, top: i32, width: usize, rows: usize, hits: &mut Vec<HitBox>) {
    // The field's border and padding are part of the clickable composer. The
    // text rectangle is last among Composer targets, for cursor coordinates.
    hits.push(HitBox {
        rect: Rect {
            x: window.x,
            y: window.y,
            width: window.width,
            height: window.height,
        },
        target: HitTarget::Composer,
    });
    hits.push(target(
        window,
        4,
        top,
        width as i32,
        rows as i32,
        HitTarget::Composer,
    ));
    let inner = window.width - 2;
    if top > 0 {
        hits.push(target(
            window,
            inner - 4,
            0,
            4,
            1,
            HitTarget::CancelComposeAction,
        ));
    }
    for (x, action) in [
        (0, HitTarget::Attachment),
        (inner - 7, HitTarget::EmojiToggle),
        (inner - 3, HitTarget::Send),
    ] {
        if x >= 0 {
            hits.push(target(window, x, top, 3, 1, action));
        }
    }
}

pub(super) struct TextAreaSpec {
    pub top: i32,
    pub width: usize,
    pub lines: usize,
}
fn target(window: &Window, x: i32, y: i32, width: i32, height: i32, target: HitTarget) -> HitBox {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn first_mouse_click_focuses_and_places_cursor_before_typing() {
        let area = Rect {
            x: 20,
            y: 10,
            width: 30,
            height: 1,
        };
        for (point, expected) in [
            ((22, 10), "abX😀cd"),
            ((18, 9), "Xab😀cd"),
            ((55, 11), "ab😀cdX"),
        ] {
            let mut state = State::default();
            let mut focused = false;
            let mut text = "ab😀cd".to_owned();
            state.activate_at(&mut focused, &text, 30, area, point);
            assert!(focused);
            state.insert(&mut text, &mut Vec::new(), "X");
            assert_eq!(text, expected);
        }
    }

    #[test]
    fn whole_composer_is_clickable_while_action_buttons_keep_priority() {
        let mut window = Window::new(0);
        window.x = 40;
        window.y = 20;
        window.width = 50;
        window.height = 4;
        let mut hits = Vec::new();
        add_mouse_targets(&window, 1, text_width(50), 1, &mut hits);
        let hit = |x, y| {
            hits.iter()
                .rev()
                .find(|hit| hit.rect.contains(x, y))
                .map(|hit| &hit.target)
        };
        for (x, y) in [(40, 20), (65, 20), (65, 21), (65, 23), (89, 23)] {
            assert!(matches!(hit(x, y), Some(HitTarget::Composer)));
        }
        assert!(matches!(hit(41, 22), Some(HitTarget::Attachment)));
        assert!(matches!(hit(82, 22), Some(HitTarget::EmojiToggle)));
        assert!(matches!(hit(86, 22), Some(HitTarget::Send)));
        assert!(matches!(hit(86, 21), Some(HitTarget::CancelComposeAction)));
        assert!(hit(90, 22).is_none());
    }

    #[test]
    fn arrows_edit_graphemes_at_the_cursor_and_keep_utf16_emoji_offsets() {
        let mut state = State::default();
        let mut text = "a😀b".to_owned();
        let mut emoji = vec![crate::custom_emoji::Draft {
            offset: 1,
            length: 2,
            id: 1,
            alt: "😀".into(),
            free: false,
        }];
        state.move_cursor(KEY_LEFT, &text, 20);
        state.move_cursor(KEY_LEFT, &text, 20);
        state.insert(&mut text, &mut emoji, "я");
        assert_eq!(text, "aя😀b");
        assert_eq!(emoji[0].offset, 2);
        state.backspace(&mut text, &mut emoji);
        assert_eq!(text, "a😀b");
        assert_eq!(emoji[0].offset, 1);
        state.move_cursor(KEY_RIGHT, &text, 20);
        state.backspace(&mut text, &mut emoji);
        assert_eq!(text, "ab");
        assert!(emoji.is_empty());
    }
    #[test]
    fn layout_preserves_spaces_newlines_and_vertical_cursor_column() {
        let mut state = State::default();
        let text = "  abc\n  x\n  abc";
        let l = layout(text, 20);
        assert_eq!(l.lines, ["  abc", "  x", "  abc"]);
        state.move_cursor(KEY_UP, text, 20);
        assert_eq!(state.byte(text), 9);
        state.move_cursor(KEY_UP, text, 20);
        assert_eq!(state.byte(text), 5);
        assert!(state.on_first_line(text, 20));
        assert_eq!(layout("😀\nя\n", 8).lines, ["😀", "я", ""]);
    }
    #[test]
    fn mouse_places_cursor_at_grapheme_boundary() {
        let mut state = State::default();
        state.place_cursor("a😀b\nxyz", 20, 5, 0, 2);
        assert_eq!(state.byte("a😀b\nxyz"), 1);
        state.place_cursor("a😀b\nxyz", 20, 5, 1, 2);
        assert_eq!(&"a😀b\nxyz"[..state.byte("a😀b\nxyz")], "a😀b\nxy");
    }
}
