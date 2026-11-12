use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use syntect::easy::HighlightLines;
use syntect::highlighting::ThemeSet;
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;
use terminalib::color::Rgb;

use super::*;

#[derive(Default)]
pub(super) struct Cache {
    view_id: u64,
    blocks: HashMap<u64, Arc<Vec<Vec<Span>>>>,
}

impl Cache {
    pub fn reset(&mut self, view_id: u64) {
        if self.view_id != view_id {
            *self = Self {
                view_id,
                ..Self::default()
            };
        }
    }
}

#[derive(Clone)]
struct Span {
    text: String,
    color: Option<Rgb>,
}

#[derive(Clone, Default)]
pub(super) struct Row {
    spans: Vec<Span>,
    background: Option<Rgb>,
    header: Option<(String, String)>,
}

impl Row {
    pub fn text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }
}

#[derive(Clone, Copy)]
pub(super) struct Position {
    pub row: usize,
    pub column: usize,
    pub cells: usize,
    pub length: usize,
    pub code: bool,
}

#[derive(Default)]
pub(super) struct Layout {
    pub rows: Vec<Row>,
    pub positions: HashMap<usize, Position>,
}

impl Layout {
    pub fn truncate(&mut self, count: usize) {
        if self.rows.len() > count {
            self.rows.truncate(count.max(1));
            if let Some(row) = self.rows.last_mut() {
                row.spans = vec![Span {
                    text: "…".to_owned(),
                    color: None,
                }];
            }
        }
    }
}

struct Builder {
    layout: Layout,
    current: Row,
    column: usize,
    width: usize,
    continuation: usize,
}

impl Builder {
    fn new(width: usize) -> Self {
        Self {
            layout: Layout::default(),
            current: Row::default(),
            column: 0,
            width: width.max(1),
            continuation: 0,
        }
    }
    fn newline(&mut self) {
        let background = self.current.background;
        self.layout.rows.push(std::mem::take(&mut self.current));
        self.current.background = background;
        self.column = 0;
    }
    fn boundary(&mut self, background: Option<Rgb>) {
        if self.column > 0 {
            self.newline();
        }
        self.current.background = background;
    }
    fn append(&mut self, text: &str, color: Option<Rgb>, mut offset: Option<usize>) {
        for grapheme in UnicodeSegmentation::graphemes(text, true) {
            let length = grapheme.encode_utf16().count();
            if grapheme == "\n" || grapheme == "\r\n" {
                self.newline();
            } else if !grapheme.chars().any(char::is_control) || grapheme == "\t" {
                let symbol = if grapheme == "\t" {
                    " ".repeat(4 - self.column % 4)
                } else {
                    grapheme.to_owned()
                };
                let cells = display_width(&symbol).max(1);
                if self.column + cells > self.width && self.column > 0 {
                    self.newline();
                }
                if self.column == 0 && self.continuation > 0 {
                    let indent = self.continuation.min(self.width.saturating_sub(cells));
                    self.current.spans.push(Span {
                        text: " ".repeat(indent),
                        color,
                    });
                    self.column = indent;
                }
                if let Some(offset) = offset {
                    self.layout.positions.insert(
                        offset,
                        Position {
                            row: self.layout.rows.len(),
                            column: self.column,
                            cells: cells.min(self.width),
                            length,
                            code: self.current.background.is_some(),
                        },
                    );
                }
                if let Some(last) = self
                    .current
                    .spans
                    .last_mut()
                    .filter(|span| span.color == color)
                {
                    last.text.push_str(&symbol);
                } else {
                    self.current.spans.push(Span {
                        text: symbol,
                        color,
                    });
                }
                self.column += cells;
            }
            if let Some(value) = &mut offset {
                *value += length;
            }
        }
    }
    fn finish(mut self) -> Layout {
        if self.column > 0 || self.layout.rows.is_empty() {
            self.newline();
        }
        self.layout
    }
}

fn utf16_byte(text: &str, offset: usize) -> Option<usize> {
    let mut units = 0;
    for (byte, character) in text.char_indices() {
        if units == offset {
            return Some(byte);
        }
        units += character.len_utf16();
        if units > offset {
            return None;
        }
    }
    (units == offset).then_some(text.len())
}

fn key(text: &str, language: &str, light: bool) -> u64 {
    let mut hash = DefaultHasher::new();
    (text, language, light).hash(&mut hash);
    hash.finish()
}

fn light_theme(theme: &theme::Theme) -> bool {
    let color = theme.palette.chat;
    u32::from(color.r) * 299 + u32::from(color.g) * 587 + u32::from(color.b) * 114 > 150_000
}

fn syntax_language(language: &str) -> String {
    match language.trim().to_ascii_lowercase().as_str() {
        "python" | "python3" => "py",
        "rust" => "rs",
        "javascript" => "js",
        "typescript" => "ts",
        "c++" => "cpp",
        "c#" | "csharp" => "cs",
        "shell" | "bash" | "zsh" => "sh",
        "golang" => "go",
        "yml" => "yaml",
        "md" => "markdown",
        value => return value.to_owned(),
    }
    .to_owned()
}

fn colors(light: bool) -> (Rgb, Rgb) {
    if light {
        (Rgb::new(242, 244, 247), Rgb::new(36, 41, 46))
    } else {
        (Rgb::new(32, 39, 51), Rgb::new(216, 222, 233))
    }
}

fn highlighted(
    source: &str,
    language: &str,
    light: bool,
    cache: &mut Cache,
) -> Arc<Vec<Vec<Span>>> {
    let id = key(source, language, light);
    if let Some(lines) = cache.blocks.get(&id) {
        return lines.clone();
    }
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    static THEMES: OnceLock<ThemeSet> = OnceLock::new();
    let (_, foreground) = colors(light);
    let syntaxes = SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines);
    let themes = THEMES.get_or_init(ThemeSet::load_defaults);
    let token = syntax_language(language);
    let syntax = (!language.trim().is_empty())
        .then(|| {
            syntaxes
                .find_syntax_by_token(&token)
                .or_else(|| syntaxes.find_syntax_by_name(language))
        })
        .flatten();
    let selected_theme = &themes.themes[if light {
        "InspiredGitHub"
    } else {
        "base16-ocean.dark"
    }];
    let mut highlighter = syntax.map(|syntax| HighlightLines::new(syntax, selected_theme));
    let mut result = Vec::new();
    for line in LinesWithEndings::from(source) {
        let spans = highlighter
            .as_mut()
            .and_then(|h| h.highlight_line(line, syntaxes).ok())
            .map(|parts| {
                parts
                    .into_iter()
                    .map(|(style, text)| Span {
                        text: text.to_owned(),
                        color: Some(Rgb::new(
                            style.foreground.r,
                            style.foreground.g,
                            style.foreground.b,
                        )),
                    })
                    .collect()
            })
            .unwrap_or_else(|| {
                vec![Span {
                    text: line.to_owned(),
                    color: Some(foreground),
                }]
            });
        result.push(spans);
    }
    if result.is_empty() {
        result.push(vec![Span {
            text: String::new(),
            color: Some(foreground),
        }]);
    }
    let result = Arc::new(result);
    if cache.blocks.len() >= 128 {
        cache.blocks.clear();
    }
    cache.blocks.insert(id, result.clone());
    result
}

fn markdown(builder: &mut Builder, source: &str, foreground: Rgb, accent: Rgb) {
    let mut emphasis = 0usize;
    let mut lists = Vec::new();
    let mut item_indents = Vec::new();
    for event in Parser::new_ext(
        source,
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_TABLES,
    ) {
        match event {
            Event::Start(Tag::Heading { .. } | Tag::Strong | Tag::Emphasis) => emphasis += 1,
            Event::End(TagEnd::Strong | TagEnd::Emphasis) => emphasis = emphasis.saturating_sub(1),
            Event::End(TagEnd::Heading(_)) => {
                emphasis = emphasis.saturating_sub(1);
                builder.newline();
            }
            Event::Start(Tag::List(start)) => lists.push(start),
            Event::End(TagEnd::List(_)) => {
                lists.pop();
            }
            Event::Start(Tag::Item) => {
                if builder.column > 0 {
                    builder.newline();
                }
                builder.continuation = 0;
                let prefix = match lists.last_mut() {
                    Some(Some(number)) => {
                        let prefix = format!("{number}. ");
                        *number += 1;
                        prefix
                    }
                    _ => "• ".to_owned(),
                };
                builder.append(
                    &format!("{}{}", "  ".repeat(lists.len().saturating_sub(1)), prefix),
                    Some(accent),
                    None,
                );
                let indent = builder.column.min(builder.width.saturating_sub(1));
                item_indents.push(indent);
                builder.continuation = indent;
            }
            Event::Start(Tag::BlockQuote(_)) => {
                builder.boundary(builder.current.background);
                builder.append("▎ ", Some(accent), None);
            }
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) => builder.append(
                &text,
                Some(if emphasis > 0 { accent } else { foreground }),
                None,
            ),
            Event::Code(text) | Event::InlineMath(text) | Event::DisplayMath(text) => {
                builder.append(&text, Some(accent), None)
            }
            Event::SoftBreak if !item_indents.is_empty() => builder.newline(),
            Event::SoftBreak => builder.append(" ", Some(foreground), None),
            Event::End(TagEnd::Item) => {
                if builder.column > 0 {
                    builder.newline();
                }
                item_indents.pop();
                builder.continuation = item_indents.last().copied().unwrap_or(0);
            }
            Event::HardBreak
            | Event::End(TagEnd::Paragraph | TagEnd::CodeBlock | TagEnd::TableRow) => {
                if builder.column > 0 {
                    builder.newline();
                }
            }
            Event::End(TagEnd::TableCell) => builder.append("  ", Some(foreground), None),
            Event::TaskListMarker(checked) => {
                builder.append(if checked { "[x] " } else { "[ ] " }, Some(accent), None)
            }
            Event::Rule => {
                builder.boundary(builder.current.background);
                builder.append("────────", Some(accent), None);
                builder.newline();
            }
            _ => {}
        }
    }
}

pub(super) fn layout(app: &App, message: &Message, width: usize) -> Option<Layout> {
    let entities = message
        .fmt_entities()
        .map(Vec::as_slice)
        .unwrap_or_default();
    build(
        message.text(),
        entities,
        width,
        &app.theme,
        app.tgp_supported,
        &mut app.code_cache.borrow_mut(),
    )
}

fn build(
    text: &str,
    entities: &[tl::enums::MessageEntity],
    width: usize,
    theme: &theme::Theme,
    _graphics: bool,
    cache: &mut Cache,
) -> Option<Layout> {
    let mut blocks: Vec<_> = entities
        .iter()
        .filter_map(|entity| {
            let tl::enums::MessageEntity::Pre(entity) = entity else {
                return None;
            };
            let offset = usize::try_from(entity.offset).ok()?;
            let length = usize::try_from(entity.length).ok()?;
            let start = utf16_byte(text, offset)?;
            let end = utf16_byte(text, offset.checked_add(length)?)?;
            (start < end).then_some((start, end, offset, entity.language.as_str()))
        })
        .collect();
    if blocks.is_empty() {
        return None;
    }
    blocks.sort_by_key(|block| block.0);
    let light = light_theme(theme);
    let (background, foreground) = colors(light);
    let mut builder = Builder::new(width);
    let mut cursor = 0;
    for (start, end, offset, language) in blocks {
        if start < cursor {
            continue;
        }
        builder.append(
            &text[cursor..start],
            None,
            Some(text[..cursor].encode_utf16().count()),
        );
        builder.boundary(Some(background));
        let source = &text[start..end];
        builder.current.header = Some((
            language
                .trim()
                .chars()
                .filter(|c| !c.is_control())
                .collect(),
            source.to_owned(),
        ));
        builder.newline();
        match syntax_language(language).as_str() {
            "markdown" => markdown(&mut builder, source, foreground, theme.palette.accent),
            "latex" | "tex" | "math" => {
                builder.append(&super::latex::pretty(source), Some(foreground), None)
            }
            _ => {
                let lines = highlighted(source, language, light, cache);
                let mut offset = offset;
                for line in lines.iter() {
                    for span in line {
                        builder.append(&span.text, span.color, Some(offset));
                        offset += span.text.encode_utf16().count();
                    }
                }
            }
        }
        if builder.column > 0 {
            builder.newline();
        }
        builder.current.background = None;
        cursor = end;
    }
    builder.append(
        &text[cursor..],
        None,
        Some(text[..cursor].encode_utf16().count()),
    );
    Some(builder.finish())
}

pub(super) fn draw(
    window: &mut Window,
    app: &App,
    layout: &Layout,
    origin: (i32, i32),
    width: i32,
    hitboxes: &mut Vec<HitBox>,
) {
    draw_rows(
        window,
        layout,
        origin,
        width,
        (&app.theme, app.settings.ui("Copy")),
        hitboxes,
        &HashSet::new(),
    );
}

fn draw_rows(
    window: &mut Window,
    layout: &Layout,
    origin: (i32, i32),
    width: i32,
    visual: (&theme::Theme, &str),
    hitboxes: &mut Vec<HitBox>,
    image_rows: &HashSet<usize>,
) {
    let (theme, copy_label) = visual;
    for (index, row) in layout.rows.iter().enumerate() {
        let y = origin.0 + index as i32;
        if let Some(background) = row.background {
            for column in 0..width {
                window.posstyled(y, origin.1 + column, ' ', CellStyle::background(background));
            }
        }
        if let Some((language, source)) = &row.header {
            let style = CellStyle::new(Some(theme.palette.accent), row.background);
            put(window, y, origin.1, language, style, width - 6);
            put(
                window,
                y,
                origin.1 + width - 4,
                &fit_label(copy_label, 4),
                style,
                4,
            );
            hitboxes.push(HitBox {
                rect: Rect {
                    x: window.x + 1 + origin.1 + width - 4,
                    y: window.y + 1 + y,
                    width: 4,
                    height: 1,
                },
                target: HitTarget::CopyCode(source.clone()),
            });
        } else if !image_rows.contains(&index) {
            let mut column = 0;
            for span in &row.spans {
                put(
                    window,
                    y,
                    origin.1 + column,
                    &span.text,
                    CellStyle::new(span.color.or(Some(theme.palette.text)), row.background),
                    width - column,
                );
                column += display_width(&span.text) as i32;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(source: &str, language: &str, graphics: bool) -> Layout {
        build(
            source,
            &[tl::types::MessageEntityPre {
                offset: 0,
                length: source.encode_utf16().count() as i32,
                language: language.to_owned(),
            }
            .into()],
            48,
            &theme::Theme::default(),
            graphics,
            &mut Cache::default(),
        )
        .unwrap()
    }

    #[test]
    fn plain_blocks_keep_indent_blank_lines_opaque_background_and_exact_copy_source() {
        let source = "    indented\n\n\tline 2\n";
        let layout = block(source, "", false);
        assert!(layout.rows.iter().all(|row| row.background.is_some()));
        assert_eq!(
            layout.rows[0].header.as_ref().unwrap(),
            &(String::new(), source.to_owned())
        );
        assert_eq!(layout.rows[1].text(), "    indented");
        assert_eq!(layout.rows[2].text(), "");
        assert_eq!(layout.rows[3].text(), "    line 2");
        let colors: HashSet<_> = layout
            .rows
            .iter()
            .flat_map(|r| &r.spans)
            .map(|s| s.color)
            .collect();
        assert_eq!(colors.len(), 1);
    }

    #[test]
    fn drawing_code_keeps_an_opaque_background_and_a_precise_copy_hitbox() {
        let source = "    x = 42\n\nprint(x)";
        let layout = block(source, "python", false);
        let mut window = Window::new(0);
        window.x = 12;
        window.y = 3;
        window.width = 60;
        window.height = 25;
        let mut hits = Vec::new();
        draw_rows(
            &mut window,
            &layout,
            (2, 4),
            48,
            (&theme::Theme::default(), "Copy"),
            &mut hits,
            &HashSet::new(),
        );
        assert!(window.cells().all(|cell| cell.style.background.is_some()));
        assert_eq!(window.cells().count(), layout.rows.len() * 48);
        assert_eq!(hits.len(), 1);
        assert!(matches!(&hits[0].target,HitTarget::CopyCode(code) if code==source));
        assert_eq!(
            hits[0].rect,
            Rect {
                x: 61,
                y: 6,
                width: 4,
                height: 1
            }
        );
    }

    #[test]
    fn python_blocks_highlight_only_when_a_language_was_supplied() {
        let source = "def hello(name):\n    return \"Привет\" # comment";
        let layout = block(source, "python", false);
        assert_eq!(layout.rows[0].header.as_ref().unwrap().0, "python");
        let colors: HashSet<_> = layout
            .rows
            .iter()
            .flat_map(|r| &r.spans)
            .map(|s| s.color)
            .collect();
        assert!(colors.len() >= 3);
        assert!(layout.rows[2].text().starts_with("    return"));
        let unknown = block(source, "unknown-language", false);
        assert_eq!(
            unknown.rows[0].header.as_ref().unwrap().0,
            "unknown-language"
        );
    }

    #[test]
    fn code_entities_use_utf16_and_preserve_positions_after_inserted_headers() {
        let prefix = "😀 intro\n";
        let source = "  x = 1\n";
        let text = format!("{prefix}{source}after 👋");
        let offset = prefix.encode_utf16().count();
        let entities = [tl::types::MessageEntityPre {
            offset: offset as i32,
            length: source.encode_utf16().count() as i32,
            language: "py".to_owned(),
        }
        .into()];
        let layout = build(
            &text,
            &entities,
            48,
            &theme::Theme::default(),
            false,
            &mut Cache::default(),
        )
        .unwrap();
        assert!(layout.positions[&offset].code);
        let position = &layout.positions[&(offset + source.encode_utf16().count())];
        assert!(!position.code);
        assert_eq!(layout.rows[position.row].text(), "after 👋");
        assert_eq!(layout.rows[1].header.as_ref().unwrap().1, source);
    }

    #[test]
    fn markdown_is_rendered_but_copy_retains_original_markup() {
        let source = "# Heading\n\n- **bold** and `code`\n- [link](https://example.com)";
        let layout = block(source, "md", false);
        let text = layout
            .rows
            .iter()
            .map(Row::text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Heading"));
        assert!(text.contains("• bold and code"));
        assert!(text.contains("• link"));
        assert!(!text.contains("**") && !text.contains("# Heading"));
        assert_eq!(layout.rows[0].header.as_ref().unwrap().1, source);
    }

    #[test]
    fn latex_fractions_and_roots_have_two_dimensional_layout_and_copy_keeps_source() {
        let source = r"\frac{a}{b} + \sqrt{x}";
        let graphics = block(source, "latex", true);
        let text = block(source, "latex", false);
        assert_eq!(
            graphics.rows.iter().map(Row::text).collect::<Vec<_>>(),
            text.rows.iter().map(Row::text).collect::<Vec<_>>()
        );
        assert!(text.rows.iter().any(|row| row.text().contains('─')));
        assert!(text.rows.iter().any(|row| row.text().contains('√')));
        assert_eq!(graphics.rows[0].header.as_ref().unwrap().1, source);
    }

    #[test]
    fn malformed_utf16_ranges_are_ignored_and_cache_is_scoped_to_the_view() {
        let mut cache = Cache::default();
        cache.blocks.insert(1, Arc::default());
        cache.reset(2);
        assert!(cache.blocks.is_empty());
        assert!(
            build(
                "😀",
                &[tl::types::MessageEntityPre {
                    offset: 1,
                    length: 1,
                    language: String::new()
                }
                .into()],
                48,
                &theme::Theme::default(),
                false,
                &mut cache
            )
            .is_none()
        );
    }

    #[test]
    fn markdown_list_continuations_have_hanging_indents_and_keep_line_breaks() {
        let layout = block(
            "- first\n  continuation\n  - nested\n    more\n- second\n\n10. numbered\n    continuation",
            "md",
            false,
        );
        let rows: Vec<_> = layout.rows.iter().skip(1).map(Row::text).collect();
        assert_eq!(
            rows,
            [
                "• first",
                "  continuation",
                "  • nested",
                "    more",
                "• second",
                "10. numbered",
                "    continuation"
            ]
        );
        let mut builder = Builder::new(12);
        markdown(
            &mut builder,
            "- 1234567890123456789",
            Rgb::new(255, 255, 255),
            Rgb::new(0, 200, 255),
        );
        let layout = builder.finish();
        assert_eq!(layout.rows[0].text(), "• 1234567890");
        assert_eq!(layout.rows[1].text(), "  123456789");
    }
}
