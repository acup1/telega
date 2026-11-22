use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use terminalib::terminal::Terminal;
use terminalib::tgp::{ImageData, ImageGroup, TerminalImage};
use terminalib::{KEY_LEFT, KEY_RIGHT, MOUSE_EVENT, MouseEvent, WindowManager};

const OVERLAY_Z_INDEX: i32 = i32::MAX - 1;
#[derive(Clone)]
pub struct MemoryImage {
    png: Arc<Vec<u8>>,
    width: u32,
    height: u32,
}
impl MemoryImage {
    pub fn identity(&self) -> usize {
        Arc::as_ptr(&self.png) as usize
    }
    pub fn from_png(png: Vec<u8>) -> Result<Self> {
        let (width, height) = png_dimensions(&png)?;
        Ok(Self {
            png: Arc::new(png),
            width,
            height,
        })
    }

    pub fn png(&self) -> &[u8] {
        self.png.as_slice()
    }

    pub const fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

pub struct ViewerContext<'a> {
    pub terminal: &'a mut Terminal,
    pub windows: &'a mut WindowManager,
    pub tgp_supported: bool,
    pub nerd_fonts: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewerAction {
    Previous,
    Next,
    Close,
    React,
    Reply,
}

pub fn show_unsupported(
    terminal: &mut Terminal,
    windows: &mut WindowManager,
    nerd_fonts: bool,
) -> Result<ViewerAction> {
    let (height, width) = terminal.getsize();
    let row = u32::from(height) / 2;
    let column = u32::from(width) / 2;
    terminal.setpos(row as u16, column as u16);
    write!(
        terminal.stdout,
        "{}",
        if nerd_fonts { "\u{f03e}" } else { "!" }
    )?;
    terminal.refresh()?;
    let action = wait_for_viewer_action(
        terminal,
        row,
        column,
        1,
        if nerd_fonts { 2 } else { 1 },
        false,
    )?;
    terminal.setpos(row as u16, column as u16);
    write!(terminal.stdout, "  ")?;
    terminal.refresh()?;
    windows.invalidate();
    Ok(action)
}

pub struct DisplayedImage {
    pub(crate) image: TerminalImage,
    pub(crate) row: u32,
    pub(crate) column: u32,
    pub(crate) rows: u32,
    pub(crate) columns: u32,
}

impl DisplayedImage {
    pub fn bounds(&self) -> (u32, u32, u32, u32) {
        (self.row, self.column, self.rows, self.columns)
    }
    pub fn delete(mut self, terminal: &mut Terminal) -> Result<()> {
        self.image.delete(&mut terminal.stdout)?;
        terminal.refresh()?;
        Ok(())
    }
}

pub fn show_memory_overlay(image: &MemoryImage, terminal: &mut Terminal) -> Result<DisplayedImage> {
    let data = ImageData::from_png_bytes(image.png().to_vec())?;
    let mut terminal_image = TerminalImage::new(0x5447_0010, data)?;
    let (height, width) = terminal.getsize();
    let (pixel_width, pixel_height) = image.dimensions();
    let (columns, rows) = fit_dimensions(
        pixel_width,
        pixel_height,
        u32::from(width),
        u32::from(height).saturating_sub(2).max(1),
    );
    let row = u32::from(height).saturating_sub(2).saturating_sub(rows) / 2;
    let column = u32::from(width).saturating_sub(columns) / 2;
    terminal.setpos(row as u16, column as u16);
    terminal_image.upload(&mut terminal.stdout)?;
    place_overlay(&terminal_image, &mut terminal.stdout, columns, rows)?;
    terminal.refresh()?;
    Ok(DisplayedImage {
        image: terminal_image,
        row,
        column,
        rows,
        columns,
    })
}

const STORY_ACTIONS: &str = "[R] React    [M] Reply";

pub(crate) fn story_action_hit(
    terminal: &mut Terminal,
    x: u32,
    y: u32,
    video: bool,
) -> Option<ViewerAction> {
    let (height, width) = terminal.getsize();
    let row = u32::from(height.saturating_sub(if video { 2 } else { 1 }));
    let start = u32::from(width).saturating_sub(STORY_ACTIONS.len() as u32) / 2;
    if y != row || x < start || x >= start + STORY_ACTIONS.len() as u32 {
        return None;
    }
    Some(if x - start < 13 {
        ViewerAction::React
    } else {
        ViewerAction::Reply
    })
}

pub fn draw_story_action_bar(terminal: &mut Terminal, video: bool) -> Result<()> {
    let (height, width) = terminal.getsize();
    let row = height.saturating_sub(if video { 2 } else { 1 });
    let start = usize::from(width).saturating_sub(STORY_ACTIONS.len()) / 2;
    terminal.setpos(row, start as u16);
    write!(terminal.stdout, "\x1b[0m{STORY_ACTIONS}")?;
    terminal.refresh()?;
    Ok(())
}

pub fn poll_viewer_action(
    terminal: &mut Terminal,
    bounds: (u32, u32, u32, u32),
    timeout_ms: u64,
    story_mode: bool,
) -> Result<Option<ViewerAction>> {
    let key = terminal.get_key(timeout_ms, false)?;
    let (row, column, rows, columns) = bounds;
    Ok(match key {
        27 => Some(ViewerAction::Close),
        value if story_mode && value == i32::from(b'r') => Some(ViewerAction::React),
        value if story_mode && value == i32::from(b'm') => Some(ViewerAction::Reply),
        KEY_LEFT => Some(ViewerAction::Previous),
        KEY_RIGHT => Some(ViewerAction::Next),
        MOUSE_EVENT => {
            let mouse = terminal.getmouse();
            let x = u32::from(mouse.x);
            let y = u32::from(mouse.y);
            let inside = rows > 0
                && columns > 0
                && x >= column
                && x < column.saturating_add(columns)
                && y >= row
                && y < row.saturating_add(rows);
            if mouse.event == MouseEvent::LEFT
                && story_mode
                && let Some(action) = story_action_hit(terminal, x, y, false)
            {
                return Ok(Some(action));
            }
            (mouse.event == MouseEvent::LEFT && !inside).then_some(ViewerAction::Close)
        }
        _ => None,
    })
}

pub fn show_tgp(
    path: &Path,
    terminal: &mut Terminal,
    windows: &mut WindowManager,
    story_mode: bool,
) -> Result<ViewerAction> {
    show_image(
        ImageData::from_png_file(path)?,
        terminal,
        windows,
        story_mode,
    )
}

pub fn show_image(
    data: ImageData,
    terminal: &mut Terminal,
    windows: &mut WindowManager,
    story_mode: bool,
) -> Result<ViewerAction> {
    let dims = (data.width(), data.height());
    let mut image = TerminalImage::new(0x5447_0001, data)?;
    let (height, width) = terminal.getsize();
    let usable_height = if story_mode {
        height.saturating_sub(2)
    } else {
        height
    };
    let (columns, rows) =
        fit_dimensions(dims.0, dims.1, u32::from(width), u32::from(usable_height));
    let row = u32::from(usable_height).saturating_sub(rows) / 2;
    let column = u32::from(width).saturating_sub(columns) / 2;
    terminal.setpos(row as u16, column as u16);
    image.upload(&mut terminal.stdout)?;
    place_overlay(&image, &mut terminal.stdout, columns, rows)?;
    if story_mode {
        draw_story_action_bar(terminal, false)?;
    }
    terminal.refresh()?;
    let action = wait_for_viewer_action(terminal, row, column, rows, columns, story_mode)?;
    image.delete(&mut terminal.stdout)?;
    terminal.refresh()?;
    windows.invalidate();
    Ok(action)
}

pub fn show_tgp_animation(
    paths: &[PathBuf],
    terminal: &mut Terminal,
    windows: &mut WindowManager,
) -> Result<ViewerAction> {
    let first = paths.first().context("sticker has no renderable frames")?;
    let data = ImageData::from_png_file(first)?;
    let mut image = TerminalImage::new(0x5447_0002, data)?;
    let (height, width) = terminal.getsize();
    let (columns, rows) = fit_cells(first, u32::from(width), u32::from(height))?;
    let row = u32::from(height).saturating_sub(rows) / 2;
    let column = u32::from(width).saturating_sub(columns) / 2;
    terminal.setpos(row as u16, column as u16);
    image.upload(&mut terminal.stdout)?;
    place_overlay(&image, &mut terminal.stdout, columns, rows)?;
    for path in paths.iter().skip(1) {
        image.push_frame(
            &mut terminal.stdout,
            ImageData::from_png_file(path)?,
            std::time::Duration::from_millis(83),
        )?;
    }
    image.start_animation(&mut terminal.stdout, None)?;
    terminal.refresh()?;
    let action = wait_for_viewer_action(terminal, row, column, rows, columns, false)?;
    image.stop_animation(&mut terminal.stdout)?;
    image.delete(&mut terminal.stdout)?;
    terminal.refresh()?;
    windows.invalidate();
    Ok(action)
}

pub(crate) fn place_overlay(
    image: &TerminalImage,
    writer: &mut impl Write,
    columns: u32,
    rows: u32,
) -> Result<()> {
    image.place_at_z(writer, columns, rows, OVERLAY_Z_INDEX)?;
    Ok(())
}

fn wait_for_viewer_action(
    terminal: &mut Terminal,
    row: u32,
    column: u32,
    rows: u32,
    columns: u32,
    story_mode: bool,
) -> Result<ViewerAction> {
    loop {
        if let Some(action) =
            poll_viewer_action(terminal, (row, column, rows, columns), u64::MAX, story_mode)?
        {
            return Ok(action);
        }
    }
}

pub fn fit_cells(path: &Path, max_columns: u32, max_rows: u32) -> Result<(u32, u32)> {
    let (pixel_width, pixel_height) = image_dimensions(path)?;
    Ok(fit_dimensions(
        pixel_width,
        pixel_height,
        max_columns,
        max_rows,
    ))
}

pub fn fit_dimensions(
    pixel_width: u32,
    pixel_height: u32,
    max_columns: u32,
    max_rows: u32,
) -> (u32, u32) {
    let source_ratio = pixel_width as f64 / pixel_height.max(1) as f64;
    let cell_aspect = terminal_cell_aspect();
    let max_columns = max_columns.max(1);
    let max_rows = max_rows.max(1);
    let mut best = (1, 1);
    let mut best_error = f64::INFINITY;
    let mut best_area = 0;
    for rows in 1..=max_rows {
        for columns in 1..=max_columns {
            let displayed_ratio = columns as f64 * cell_aspect / rows as f64;
            let error = (displayed_ratio / source_ratio).ln().abs();
            let area = columns * rows;
            if error + 1e-9 < best_error || ((error - best_error).abs() <= 1e-9 && area > best_area)
            {
                best = (columns, rows);
                best_error = error;
                best_area = area;
            }
        }
    }
    best
}

pub fn square_cells(max_columns: u32, max_rows: u32) -> (u32, u32) {
    fit_dimensions(1, 1, max_columns, max_rows)
}

pub fn image_dimensions(path: &Path) -> Result<(u32, u32)> {
    let mut header = [0u8; 24];
    File::open(path)
        .with_context(|| format!("cannot open {}", path.display()))?
        .read_exact(&mut header)
        .with_context(|| format!("cannot read PNG header from {}", path.display()))?;
    png_dimensions(&header).with_context(|| format!("{} is not a PNG image", path.display()))
}

pub fn png_dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        bail!("missing PNG header");
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    if width == 0 || height == 0 {
        bail!("invalid PNG dimensions");
    }
    Ok((width, height))
}

pub fn terminal_cell_aspect() -> f64 {
    if let Ok(value) = std::env::var("TELEGA_CELL_ASPECT")
        && let Ok(value) = value.parse::<f64>()
        && (0.1..=2.0).contains(&value)
    {
        return value;
    }
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) };
    if result == 0 && size.ws_col > 0 && size.ws_row > 0 && size.ws_xpixel > 0 && size.ws_ypixel > 0
    {
        let cell_width = f64::from(size.ws_xpixel) / f64::from(size.ws_col);
        let cell_height = f64::from(size.ws_ypixel) / f64::from(size.ws_row);
        if cell_height > 0.0 {
            return cell_width / cell_height;
        }
    }
    0.5
}

pub fn viewer_backdrop(terminal: &mut Terminal) -> Result<TerminalImage> {
    let mut image = TerminalImage::new(
        0x5447_0030,
        ImageData::solid(1, 1, terminalib::Rgba::new(0, 0, 0, 180))?,
    )?;
    let (height, width) = terminal.getsize();
    terminal.setpos(0, 0);
    image.upload(&mut terminal.stdout)?;
    image.place_at_z(
        &mut terminal.stdout,
        u32::from(width),
        u32::from(height),
        i32::MAX - 2,
    )?;
    terminal.refresh()?;
    Ok(image)
}

pub fn clear_viewer_layers(terminal: &mut Terminal) -> Result<()> {
    let mut group = ImageGroup::begin(&mut terminal.stdout)?;
    for id in [
        0x5447_0001u32,
        0x5447_0002,
        0x5447_0003,
        0x5447_0004,
        0x5447_0010,
        0x5447_0020,
        0x5447_0021,
        0x5447_0030,
    ] {
        write!(group, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\")?;
    }
    group.present()?;
    Ok(())
}
