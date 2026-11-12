use super::*;

pub(super) struct TextArea<'a> {
    pub origin: (i32, i32),
    pub width: usize,
    pub lines: usize,
    pub code: Option<&'a code::Layout>,
}

pub(super) fn draw(
    window: &mut Window,
    app: &App,
    message: &Message,
    area: TextArea<'_>,
    placements: &mut Vec<ImagePlacement>,
) {
    if !app.tgp_supported {
        return;
    }
    let Some(CustomEmojiState::Ready(images)) = app.custom_emoji.get(&message.id()) else {
        return;
    };
    let entities = crate::custom_emoji::entities(message);
    let positions = fallback_positions(message.text(), &entities, area.width, area.code);
    draw_loaded(window, message.id(), images, &positions, &area, placements);
}

pub(super) fn draw_draft(
    window: &mut Window,
    app: &App,
    positions: &HashMap<usize, code::Position>,
    start: usize,
    spec: composer::TextAreaSpec,
    placements: &mut Vec<ImagePlacement>,
) {
    if !app.tgp_supported {
        return;
    }
    let layout = code::Layout {
        rows: Vec::new(),
        positions: positions
            .iter()
            .filter(|(_, position)| position.row >= start && position.row < start + spec.lines)
            .map(|(&offset, position)| {
                let mut p = *position;
                p.row -= start;
                (offset, p)
            })
            .collect(),
    };
    let entities: Vec<_> = app
        .draft_custom
        .iter()
        .map(|e| (e.offset, e.length, e.id))
        .collect();
    let positions = fallback_positions(&app.input, &entities, spec.width, Some(&layout));
    let images: Vec<_> = app
        .draft_custom
        .iter()
        .map(|e| app.emoji_state.visual(e.id).cloned())
        .collect();
    let mut draft_placements = Vec::new();
    draw_loaded(
        window,
        0,
        &images,
        &positions,
        &TextArea {
            origin: (spec.top, 4),
            width: spec.width,
            lines: spec.lines,
            code: None,
        },
        &mut draft_placements,
    );
    for (slot, mut placement) in draft_placements.into_iter().enumerate() {
        placement.message_id = -1_700_000_000 - slot as i32;
        placement.crop = false;
        placements.push(placement);
    }
}

pub(super) fn draw_sidebar(
    window: &mut Window,
    chat_index: usize,
    text: &str,
    message: &Message,
    images: &[Option<media::StickerFrames>],
    area: TextArea<'_>,
    placements: &mut Vec<ImagePlacement>,
) {
    let entities = crate::custom_emoji::entities(message);
    let positions = fallback_positions(text, &entities, area.width, None);
    let mut local = Vec::new();
    draw_loaded(window, 0, images, &positions, &area, &mut local);
    for (slot, mut placement) in local.into_iter().enumerate() {
        placement.message_id = -1_520_000_000 - (chat_index as i32 * 64 + slot as i32);
        placement.crop = false;
        placements.push(placement);
    }
}

// Use the same cell positions for the image and for suppressing its text fallback.
// Do not remove characters from the source: UTF-16 entities, wrapping and Copy
// must continue to refer to the original text, including multi-codepoint emoji.
fn fallback_positions(
    text: &str,
    entities: &[(i32, i32, i64)],
    width: usize,
    code: Option<&code::Layout>,
) -> Vec<Vec<code::Position>> {
    let plain;
    let positions = if let Some(layout) = code {
        &layout.positions
    } else {
        let trimmed = text.trim();
        let mut offset = text[..text.len() - text.trim_start().len()]
            .encode_utf16()
            .count();
        let (mut row, mut column) = (0, 0);
        let width = width.max(1);
        let mut result = HashMap::new();
        for grapheme in UnicodeSegmentation::graphemes(trimmed, true) {
            let length = grapheme.encode_utf16().count();
            if grapheme == "\n" {
                row += 1;
                column = 0;
                offset += length;
                continue;
            }
            let cells = display_width(grapheme).max(1);
            if column + cells > width && column > 0 {
                row += 1;
                column = 0;
                if grapheme.chars().all(char::is_whitespace) {
                    offset += length;
                    continue;
                }
            }
            result.insert(
                offset,
                code::Position {
                    row,
                    column,
                    cells,
                    length,
                    code: false,
                },
            );
            column += cells;
            offset += length;
        }
        plain = result;
        &plain
    };
    entities
        .iter()
        .map(|&(offset, length, _)| {
            if length <= 0 || crate::custom_emoji::slice_utf16(text, offset, length).is_none() {
                return Vec::new();
            }
            let start = offset as usize;
            let end = start + length as usize;
            let mut cells: Vec<_> = positions
                .iter()
                .filter_map(|(&offset, position)| {
                    (offset >= start && offset + position.length <= end && !position.code)
                        .then_some(*position)
                })
                .collect();
            cells.sort_by_key(|position| (position.row, position.column));
            cells
        })
        .collect()
}

fn draw_loaded(
    window: &mut Window,
    message_id: i32,
    images: &[Option<media::StickerFrames>],
    positions: &[Vec<code::Position>],
    area: &TextArea<'_>,
    placements: &mut Vec<ImagePlacement>,
) {
    for (slot, (image, fallback)) in images.iter().zip(positions).enumerate() {
        let Some(image) = image else {
            continue;
        };
        let Some(position) = fallback
            .first()
            .filter(|position| position.row < area.lines)
        else {
            continue;
        };
        for position in fallback.iter().filter(|position| position.row < area.lines) {
            let row = area.origin.0 + position.row as i32;
            let col = area.origin.1 + position.column as i32;
            let cells = position
                .cells
                .min(area.width.saturating_sub(position.column));
            let base_style = window
                .cells()
                .find(|entry| entry.y == row && entry.x == col)
                .map_or(window.border_style, |entry| entry.style);
            // Clear the *whole* fallback, including both cells of a wide glyph.
            // Preserve each cell's existing background (also with transparent themes).
            let styles: Vec<_> = (0..cells)
                .map(|cell| {
                    window
                        .cells()
                        .find(|entry| entry.y == row && entry.x == col + cell as i32)
                        .map_or(base_style, |entry| entry.style)
                })
                .collect();
            for (cell, style) in styles.into_iter().enumerate() {
                window.posstyled(row, col + cell as i32, ' ', style);
            }
        }
        placements.push(ImagePlacement {
            message_id: custom_emoji_asset_id(message_id, slot),
            asset: ImageAsset::Animated(image.clone()),
            row: (window.y + 1 + area.origin.0 + position.row as i32).max(0) as u16,
            column: (window.x + 1 + area.origin.1 + position.column as i32).max(0) as u16,
            rows: 1,
            columns: 2,
            crop: true,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_preview_keeps_custom_emoji_utf16_position_after_newline_is_flattened() {
        let text = "A\n😀B";
        let preview = text.replace('\n', " ");
        let positions = fallback_positions(&preview, &[(2, 2, 99)], 12, None);
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0][0].row, 0);
        assert_eq!(positions[0][0].column, 2);
        assert_eq!(positions[0][0].cells, 2);
        assert!(
            fallback_positions(&preview, &[(2, 2, 99)], 2, None)[0]
                .iter()
                .all(|p| p.row > 0)
        );
    }

    fn image() -> media::StickerFrames {
        let mut png = vec![0; 24];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        png[16..20].copy_from_slice(&1u32.to_be_bytes());
        png[20..24].copy_from_slice(&1u32.to_be_bytes());
        media::StickerFrames::still(media::MemoryImage::from_png(png).unwrap())
    }

    #[test]
    fn ready_image_hides_only_its_fallback_without_changing_other_text_or_background() {
        let mut window = Window::new(0);
        window.width = 40;
        window.height = 12;
        let style = CellStyle::new(
            Some(terminalib::Rgb::new(200, 200, 200)),
            Some(terminalib::Rgb::new(10, 20, 30)),
        );
        put(&mut window, 2, 3, "A👨‍💻B 😀C", style, 30);
        let text = "A👨‍💻B 😀C";
        let positions = fallback_positions(text, &[(1, 5, 10), (8, 2, 20)], 30, None);
        let area = TextArea {
            origin: (2, 3),
            width: 30,
            lines: 1,
            code: None,
        };
        let mut images = Vec::new();
        draw_loaded(
            &mut window,
            7,
            &[Some(image()), None],
            &positions,
            &area,
            &mut images,
        );
        let cell = |x| {
            window
                .cells()
                .find(|cell| cell.y == 2 && cell.x == x)
                .unwrap()
        };
        assert_eq!(cell(3).sym, "A");
        assert_eq!(cell(4).sym, " ");
        assert_eq!(cell(5).sym, " ");
        assert_eq!(cell(6).sym, "B");
        assert_eq!(cell(8).sym, "😀");
        assert_eq!(cell(4).style, style);
        assert_eq!(cell(5).style, style);
        assert_eq!(images.len(), 1);
        assert_eq!((images[0].row, images[0].column), (3, 5));
        assert_eq!(text, "A👨‍💻B 😀C");
    }

    #[test]
    fn unloaded_images_and_offscreen_entities_keep_text_fallbacks() {
        let mut window = Window::new(0);
        window.width = 20;
        window.height = 10;
        put(&mut window, 0, 0, "😀", CellStyle::default(), 10);
        let positions = fallback_positions("😀", &[(0, 2, 10)], 10, None);
        let mut placements = Vec::new();
        let mut area = TextArea {
            origin: (0, 0),
            width: 10,
            lines: 1,
            code: None,
        };
        draw_loaded(&mut window, 1, &[None], &positions, &area, &mut placements);
        assert_eq!(window.cells().next().unwrap().sym, "😀");
        area.lines = 0;
        draw_loaded(
            &mut window,
            1,
            &[Some(image())],
            &positions,
            &area,
            &mut placements,
        );
        assert_eq!(window.cells().next().unwrap().sym, "😀");
        assert!(placements.is_empty());
    }

    #[test]
    fn wrapping_trimming_variation_selectors_and_invalid_utf16_ranges() {
        let text = "  ab👨‍💻x ❤️z";
        let positions = fallback_positions(text, &[(4, 5, 1), (11, 2, 2), (5, 1, 3)], 3, None);
        assert_eq!(
            (
                positions[0][0].row,
                positions[0][0].column,
                positions[0][0].cells
            ),
            (1, 0, 2)
        );
        assert_eq!(
            (
                positions[1][0].row,
                positions[1][0].column,
                positions[1][0].cells
            ),
            (2, 0, 2)
        );
        assert!(positions[2].is_empty());
    }

    #[test]
    fn multiple_grapheme_fallbacks_clear_every_occupied_cell_even_across_lines() {
        let positions = fallback_positions("x😀🙂y", &[(1, 4, 1)], 3, None);
        assert_eq!(positions[0].len(), 2);
        assert_eq!((positions[0][0].row, positions[0][0].column), (0, 1));
        assert_eq!((positions[0][1].row, positions[0][1].column), (1, 0));
    }
}
