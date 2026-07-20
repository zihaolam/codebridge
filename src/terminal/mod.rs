//! Server-owned terminal state and rendering.
//!
//! Codebridge deliberately follows Herdr's useful boundary here, without
//! adopting its general-purpose multiplexer product model: the daemon owns the
//! PTY and `libghostty-vt` state, while clients own only presentation and input
//! focus. Scrolling moves Ghostty's viewport, so reflow, alternate-screen
//! behavior, wide graphemes, and cursor visibility all come from the emulator
//! rather than ANSI-string reconstruction.

#[allow(
    dead_code,
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    clippy::all,
    rustdoc::all
)]
mod ffi;

use std::ffi::c_void;
use std::mem;
use std::ptr;
use std::slice;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use thiserror::Error;

const SUCCESS: ffi::GhosttyResult = ffi::GhosttyResult_GHOSTTY_SUCCESS;

#[derive(Debug, Error)]
#[error("libghostty-vt returned error {0}")]
pub struct TerminalError(ffi::GhosttyResult);

fn result(code: ffi::GhosttyResult) -> Result<(), TerminalError> {
    (code == SUCCESS).then_some(()).ok_or(TerminalError(code))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollMetrics {
    pub offset_from_bottom: usize,
    pub max_offset_from_bottom: usize,
    pub viewport_rows: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub x: u16,
    pub y: u16,
    pub visible: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedTerminal {
    pub buffer: Buffer,
    pub cursor: Option<Cursor>,
    pub mouse_reporting: bool,
    pub scroll: ScrollMetrics,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAction {
    Press,
    Release,
    Motion,
}

type Reply = dyn FnMut(&[u8]) + Send;

#[derive(Default)]
struct Callbacks {
    reply: Option<Box<Reply>>,
}

unsafe extern "C" fn write_pty(
    _terminal: ffi::GhosttyTerminal,
    userdata: *mut c_void,
    data: *const u8,
    len: usize,
) {
    if userdata.is_null() || (data.is_null() && len != 0) {
        return;
    }
    // SAFETY: userdata is installed from the pinned Box owned by Terminal.
    let callbacks = unsafe { &mut *userdata.cast::<Callbacks>() };
    let Some(reply) = callbacks.reply.as_mut() else {
        return;
    };
    let bytes = if len == 0 {
        &[]
    } else {
        // SAFETY: libghostty-vt guarantees callback bytes live for this call.
        unsafe { slice::from_raw_parts(data, len) }
    };
    reply(bytes);
}

pub struct Terminal {
    raw: ffi::GhosttyTerminal,
    render: ffi::GhosttyRenderState,
    callbacks: Box<Callbacks>,
}

// The daemon serializes all access to each Terminal behind a mutex.
unsafe impl Send for Terminal {}

impl Terminal {
    pub fn new(
        cols: u16,
        rows: u16,
        max_scrollback: usize,
        reply: impl FnMut(&[u8]) + Send + 'static,
    ) -> Result<Self, TerminalError> {
        let mut raw = ptr::null_mut();
        let options = ffi::GhosttyTerminalOptions {
            cols,
            rows,
            max_scrollback,
        };
        // SAFETY: both output and option pointers are valid for the call.
        result(unsafe { ffi::ghostty_terminal_new(ptr::null(), &mut raw, options) })?;

        let mut render = ptr::null_mut();
        // SAFETY: output pointer is valid and the default allocator is requested.
        if let Err(error) =
            result(unsafe { ffi::ghostty_render_state_new(ptr::null(), &mut render) })
        {
            // SAFETY: raw was successfully allocated above.
            unsafe { ffi::ghostty_terminal_free(raw) };
            return Err(error);
        }

        let mut terminal = Self {
            raw,
            render,
            callbacks: Box::new(Callbacks {
                reply: Some(Box::new(reply)),
            }),
        };
        let userdata = (&mut *terminal.callbacks as *mut Callbacks).cast();
        // SAFETY: the callback box is pinned by ownership in Terminal, and the
        // function pointer has GhosttyTerminalWritePtyFn's exact ABI.
        result(unsafe {
            ffi::ghostty_terminal_set(
                terminal.raw,
                ffi::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_USERDATA,
                userdata,
            )
        })?;
        result(unsafe {
            ffi::ghostty_terminal_set(
                terminal.raw,
                ffi::GhosttyTerminalOption_GHOSTTY_TERMINAL_OPT_WRITE_PTY,
                (write_pty as *const ()).cast(),
            )
        })?;
        Ok(terminal)
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        // SAFETY: raw is live and bytes remain valid for the call.
        unsafe { ffi::ghostty_terminal_vt_write(self.raw, bytes.as_ptr(), bytes.len()) };
    }

    pub fn synchronized_output_active(&self) -> Result<bool, TerminalError> {
        self.mode_enabled(2026)
    }

    pub fn mode_enabled(&self, mode: u16) -> Result<bool, TerminalError> {
        let mut active = false;
        result(unsafe { ffi::ghostty_terminal_mode_get(self.raw, mode, &mut active) })?;
        Ok(active)
    }

    pub fn encode_paste(&self, text: &[u8]) -> Result<Vec<u8>, TerminalError> {
        let bracketed = self.mode_enabled(2004)?;
        let mut input = text.to_vec();
        let mut output = vec![0u8; input.len().saturating_add(12)];
        let mut written = 0;
        let code = unsafe {
            ffi::ghostty_paste_encode(
                input.as_mut_ptr().cast(),
                input.len(),
                bracketed,
                output.as_mut_ptr().cast(),
                output.len(),
                &mut written,
            )
        };
        result(code)?;
        output.truncate(written);
        Ok(output)
    }

    pub fn mouse_reporting(&self) -> Result<bool, TerminalError> {
        Ok(self.mode_enabled(1000)? || self.mode_enabled(1002)? || self.mode_enabled(1003)?)
    }

    pub fn encode_mouse(
        &self,
        action: MouseAction,
        button: Option<u8>,
        modifiers: u16,
        x: u16,
        y: u16,
        any_button_pressed: bool,
    ) -> Result<Vec<u8>, TerminalError> {
        result(unsafe { ffi::ghostty_render_state_update(self.render, self.raw) })?;
        let mut encoder = ptr::null_mut();
        result(unsafe { ffi::ghostty_mouse_encoder_new(ptr::null(), &mut encoder) })?;
        let mut event = ptr::null_mut();
        if let Err(error) = result(unsafe { ffi::ghostty_mouse_event_new(ptr::null(), &mut event) })
        {
            unsafe { ffi::ghostty_mouse_encoder_free(encoder) };
            return Err(error);
        }
        unsafe {
            ffi::ghostty_mouse_encoder_setopt_from_terminal(encoder, self.raw);
            let size = ffi::GhosttyMouseEncoderSize {
                size: mem::size_of::<ffi::GhosttyMouseEncoderSize>(),
                screen_width: u32::from(
                    self.render_u16(ffi::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_COLS)?,
                ),
                screen_height: u32::from(
                    self.render_u16(ffi::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_ROWS)?,
                ),
                cell_width: 1,
                cell_height: 1,
                ..Default::default()
            };
            ffi::ghostty_mouse_encoder_setopt(
                encoder,
                ffi::GhosttyMouseEncoderOption_GHOSTTY_MOUSE_ENCODER_OPT_SIZE,
                (&size as *const ffi::GhosttyMouseEncoderSize).cast(),
            );
            ffi::ghostty_mouse_encoder_setopt(
                encoder,
                ffi::GhosttyMouseEncoderOption_GHOSTTY_MOUSE_ENCODER_OPT_ANY_BUTTON_PRESSED,
                (&any_button_pressed as *const bool).cast(),
            );
            let ffi_action = match action {
                MouseAction::Press => ffi::GhosttyMouseAction_GHOSTTY_MOUSE_ACTION_PRESS,
                MouseAction::Release => ffi::GhosttyMouseAction_GHOSTTY_MOUSE_ACTION_RELEASE,
                MouseAction::Motion => ffi::GhosttyMouseAction_GHOSTTY_MOUSE_ACTION_MOTION,
            };
            ffi::ghostty_mouse_event_set_action(event, ffi_action);
            if let Some(button) = button {
                ffi::ghostty_mouse_event_set_button(event, u32::from(button));
            } else {
                ffi::ghostty_mouse_event_clear_button(event);
            }
            ffi::ghostty_mouse_event_set_mods(event, modifiers);
            ffi::ghostty_mouse_event_set_position(
                event,
                ffi::GhosttyMousePosition {
                    x: f32::from(x) + 0.5,
                    y: f32::from(y) + 0.5,
                },
            );
        }
        let mut output = vec![0u8; 128];
        let mut written = 0usize;
        let code = unsafe {
            ffi::ghostty_mouse_encoder_encode(
                encoder,
                event,
                output.as_mut_ptr().cast(),
                output.len(),
                &mut written,
            )
        };
        unsafe {
            ffi::ghostty_mouse_event_free(event);
            ffi::ghostty_mouse_encoder_free(encoder);
        }
        result(code)?;
        output.truncate(written);
        Ok(output)
    }

    pub fn read_text_screen(
        &self,
        start: (u16, u32),
        end: (u16, u32),
    ) -> Result<String, TerminalError> {
        let point = |(x, y)| ffi::GhosttyPoint {
            tag: ffi::GhosttyPointTag_GHOSTTY_POINT_TAG_SCREEN,
            value: ffi::GhosttyPointValue {
                coordinate: ffi::GhosttyPointCoordinate { x, y },
            },
        };
        let mut start_ref = ffi::GhosttyGridRef {
            size: mem::size_of::<ffi::GhosttyGridRef>(),
            ..Default::default()
        };
        let mut end_ref = ffi::GhosttyGridRef {
            size: mem::size_of::<ffi::GhosttyGridRef>(),
            ..Default::default()
        };
        result(unsafe { ffi::ghostty_terminal_grid_ref(self.raw, point(start), &mut start_ref) })?;
        result(unsafe { ffi::ghostty_terminal_grid_ref(self.raw, point(end), &mut end_ref) })?;
        let selection = ffi::GhosttySelection {
            size: mem::size_of::<ffi::GhosttySelection>(),
            start: start_ref,
            end: end_ref,
            rectangle: false,
        };
        let options = ffi::GhosttyFormatterTerminalOptions {
            size: mem::size_of::<ffi::GhosttyFormatterTerminalOptions>(),
            emit: ffi::GhosttyFormatterFormat_GHOSTTY_FORMATTER_FORMAT_PLAIN,
            unwrap: true,
            trim: true,
            extra: ffi::GhosttyFormatterTerminalExtra {
                size: mem::size_of::<ffi::GhosttyFormatterTerminalExtra>(),
                screen: ffi::GhosttyFormatterScreenExtra {
                    size: mem::size_of::<ffi::GhosttyFormatterScreenExtra>(),
                    ..Default::default()
                },
                ..Default::default()
            },
            selection: &selection,
        };
        let mut formatter = ptr::null_mut();
        result(unsafe {
            ffi::ghostty_formatter_terminal_new(ptr::null(), &mut formatter, self.raw, options)
        })?;
        let mut output = ptr::null_mut();
        let mut length = 0;
        let code = unsafe {
            ffi::ghostty_formatter_format_alloc(formatter, ptr::null(), &mut output, &mut length)
        };
        unsafe { ffi::ghostty_formatter_free(formatter) };
        result(code)?;
        let text = if length == 0 {
            String::new()
        } else {
            let bytes = unsafe { slice::from_raw_parts(output.cast_const(), length) };
            String::from_utf8_lossy(bytes).into_owned()
        };
        unsafe { ffi::ghostty_free(ptr::null(), output, length) };
        Ok(text)
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<(), TerminalError> {
        // Cell pixel size is irrelevant to the terminal TUI renderer but must
        // be non-zero for XTWINOPS and pixel-coordinate invariants.
        result(unsafe { ffi::ghostty_terminal_resize(self.raw, cols, rows, 1, 1) })
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll_delta(-isize::try_from(lines).unwrap_or(isize::MAX));
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll_delta(isize::try_from(lines).unwrap_or(isize::MAX));
    }

    pub fn scroll_to_bottom(&mut self) {
        let viewport = ffi::GhosttyTerminalScrollViewport {
            tag: ffi::GhosttyTerminalScrollViewportTag_GHOSTTY_SCROLL_VIEWPORT_BOTTOM,
            value: ffi::GhosttyTerminalScrollViewportValue::default(),
        };
        // SAFETY: the union value matches the bottom tag.
        unsafe { ffi::ghostty_terminal_scroll_viewport(self.raw, viewport) };
    }

    pub fn set_scroll_offset_from_bottom(&mut self, lines: usize) {
        self.scroll_to_bottom();
        self.scroll_up(lines);
    }

    pub fn set_scroll_row(&mut self, row: usize) {
        let viewport = ffi::GhosttyTerminalScrollViewport {
            tag: ffi::GhosttyTerminalScrollViewportTag_GHOSTTY_SCROLL_VIEWPORT_ROW,
            value: ffi::GhosttyTerminalScrollViewportValue { row },
        };
        // SAFETY: the union value matches the absolute-row tag.
        unsafe { ffi::ghostty_terminal_scroll_viewport(self.raw, viewport) };
    }

    fn scroll_delta(&mut self, delta: isize) {
        let viewport = ffi::GhosttyTerminalScrollViewport {
            tag: ffi::GhosttyTerminalScrollViewportTag_GHOSTTY_SCROLL_VIEWPORT_DELTA,
            value: ffi::GhosttyTerminalScrollViewportValue { delta },
        };
        // SAFETY: the union value matches the delta tag.
        unsafe { ffi::ghostty_terminal_scroll_viewport(self.raw, viewport) };
    }

    pub fn scroll_metrics(&self) -> Result<ScrollMetrics, TerminalError> {
        let mut scrollbar = ffi::GhosttyTerminalScrollbar::default();
        result(unsafe {
            ffi::ghostty_terminal_get(
                self.raw,
                ffi::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_SCROLLBAR,
                (&mut scrollbar as *mut ffi::GhosttyTerminalScrollbar).cast(),
            )
        })?;
        let total = usize::try_from(scrollbar.total).unwrap_or(usize::MAX);
        let offset = usize::try_from(scrollbar.offset).unwrap_or(usize::MAX);
        let len = usize::try_from(scrollbar.len).unwrap_or(usize::MAX);
        Ok(ScrollMetrics {
            offset_from_bottom: total.saturating_sub(offset.saturating_add(len)),
            max_offset_from_bottom: total.saturating_sub(len),
            viewport_rows: len,
        })
    }

    pub fn render(&mut self) -> Result<RenderedTerminal, TerminalError> {
        result(unsafe { ffi::ghostty_render_state_update(self.render, self.raw) })?;
        let cols = self.render_u16(ffi::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_COLS)?;
        let rows = self.render_u16(ffi::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_ROWS)?;
        let default_fg = self
            .render_color(ffi::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_COLOR_FOREGROUND)?;
        let default_bg = self
            .render_color(ffi::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_COLOR_BACKGROUND)?;
        let area = Rect::new(0, 0, cols, rows);
        let mut buffer = Buffer::empty(area);

        let mut row_iterator = ptr::null_mut();
        result(unsafe {
            ffi::ghostty_render_state_row_iterator_new(ptr::null(), &mut row_iterator)
        })?;
        let mut row_cells = ptr::null_mut();
        if let Err(error) =
            result(unsafe { ffi::ghostty_render_state_row_cells_new(ptr::null(), &mut row_cells) })
        {
            unsafe { ffi::ghostty_render_state_row_iterator_free(row_iterator) };
            return Err(error);
        }
        let render_result =
            self.render_cells(&mut buffer, row_iterator, row_cells, default_fg, default_bg);
        // SAFETY: both iterators were created above and are no longer borrowed.
        unsafe {
            ffi::ghostty_render_state_row_cells_free(row_cells);
            ffi::ghostty_render_state_row_iterator_free(row_iterator);
        }
        render_result?;

        let cursor = self.cursor()?;
        Ok(RenderedTerminal {
            buffer,
            cursor,
            mouse_reporting: self.mouse_reporting()?,
            scroll: self.scroll_metrics()?,
        })
    }

    fn render_cells(
        &self,
        buffer: &mut Buffer,
        row_iterator: ffi::GhosttyRenderStateRowIterator,
        row_cells: ffi::GhosttyRenderStateRowCells,
        default_fg: Color,
        default_bg: Color,
    ) -> Result<(), TerminalError> {
        result(unsafe {
            ffi::ghostty_render_state_get(
                self.render,
                ffi::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                (&row_iterator as *const ffi::GhosttyRenderStateRowIterator)
                    .cast_mut()
                    .cast(),
            )
        })?;

        let mut y = 0u16;
        while y < buffer.area.height
            && unsafe { ffi::ghostty_render_state_row_iterator_next(row_iterator) }
        {
            result(unsafe {
                ffi::ghostty_render_state_row_get(
                    row_iterator,
                    ffi::GhosttyRenderStateRowData_GHOSTTY_RENDER_STATE_ROW_DATA_CELLS,
                    (&row_cells as *const ffi::GhosttyRenderStateRowCells)
                        .cast_mut()
                        .cast(),
                )
            })?;
            let mut x = 0u16;
            while x < buffer.area.width
                && unsafe { ffi::ghostty_render_state_row_cells_next(row_cells) }
            {
                let (symbol, style, wide) = cell(row_cells, default_fg, default_bg)?;
                let target = &mut buffer[(x, y)];
                target.reset();
                target.set_symbol(&symbol);
                target.set_style(style);
                x = x.saturating_add(if wide { 2 } else { 1 });
            }
            while x < buffer.area.width {
                buffer[(x, y)]
                    .set_symbol(" ")
                    .set_fg(default_fg)
                    .set_bg(default_bg);
                x += 1;
            }
            y += 1;
        }
        Ok(())
    }

    fn cursor(&self) -> Result<Option<Cursor>, TerminalError> {
        let visible =
            self.render_bool(ffi::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_CURSOR_VISIBLE)?;
        let in_view = self.render_bool(
            ffi::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_HAS_VALUE,
        )?;
        if !visible || !in_view || self.scroll_metrics()?.offset_from_bottom != 0 {
            return Ok(None);
        }
        Ok(Some(Cursor {
            x: self.render_u16(
                ffi::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_X,
            )?,
            y: self.render_u16(
                ffi::GhosttyRenderStateData_GHOSTTY_RENDER_STATE_DATA_CURSOR_VIEWPORT_Y,
            )?,
            visible: true,
        }))
    }

    fn render_u16(&self, data: ffi::GhosttyRenderStateData) -> Result<u16, TerminalError> {
        let mut value = 0u16;
        result(unsafe {
            ffi::ghostty_render_state_get(self.render, data, (&mut value as *mut u16).cast())
        })?;
        Ok(value)
    }

    fn render_bool(&self, data: ffi::GhosttyRenderStateData) -> Result<bool, TerminalError> {
        let mut value = false;
        result(unsafe {
            ffi::ghostty_render_state_get(self.render, data, (&mut value as *mut bool).cast())
        })?;
        Ok(value)
    }

    fn render_color(&self, data: ffi::GhosttyRenderStateData) -> Result<Color, TerminalError> {
        let mut color = ffi::GhosttyColorRgb::default();
        result(unsafe {
            ffi::ghostty_render_state_get(
                self.render,
                data,
                (&mut color as *mut ffi::GhosttyColorRgb).cast(),
            )
        })?;
        Ok(rgb(color))
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // SAFETY: handles were created together and remain live until drop.
        unsafe {
            ffi::ghostty_render_state_free(self.render);
            ffi::ghostty_terminal_free(self.raw);
        }
    }
}

fn cell(
    cells: ffi::GhosttyRenderStateRowCells,
    default_fg: Color,
    default_bg: Color,
) -> Result<(String, Style, bool), TerminalError> {
    let mut raw = ffi::GhosttyCell::default();
    result(unsafe {
        ffi::ghostty_render_state_row_cells_get(
            cells,
            ffi::GhosttyRenderStateRowCellsData_GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_RAW,
            (&mut raw as *mut ffi::GhosttyCell).cast(),
        )
    })?;
    let mut wide = ffi::GhosttyCellWide_GHOSTTY_CELL_WIDE_NARROW;
    result(unsafe {
        ffi::ghostty_cell_get(
            raw,
            ffi::GhosttyCellData_GHOSTTY_CELL_DATA_WIDE,
            (&mut wide as *mut ffi::GhosttyCellWide).cast(),
        )
    })?;

    let symbol = match wide {
        ffi::GhosttyCellWide_GHOSTTY_CELL_WIDE_SPACER_TAIL => String::new(),
        ffi::GhosttyCellWide_GHOSTTY_CELL_WIDE_SPACER_HEAD => " ".to_owned(),
        _ => grapheme(cells)?,
    };

    let mut style_data = ffi::GhosttyStyle {
        size: mem::size_of::<ffi::GhosttyStyle>(),
        ..Default::default()
    };
    result(unsafe {
        ffi::ghostty_render_state_row_cells_get(
            cells,
            ffi::GhosttyRenderStateRowCellsData_GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_STYLE,
            (&mut style_data as *mut ffi::GhosttyStyle).cast(),
        )
    })?;

    let mut fg = resolved_cell_color(
        cells,
        ffi::GhosttyRenderStateRowCellsData_GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_FG_COLOR,
    )
    .unwrap_or(default_fg);
    let mut bg = resolved_cell_color(
        cells,
        ffi::GhosttyRenderStateRowCellsData_GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_BG_COLOR,
    )
    .unwrap_or(default_bg);
    if style_data.invisible {
        fg = bg;
    }
    if style_data.inverse {
        mem::swap(&mut fg, &mut bg);
    }
    let mut modifiers = Modifier::empty();
    if style_data.bold {
        modifiers |= Modifier::BOLD;
    }
    if style_data.italic {
        modifiers |= Modifier::ITALIC;
    }
    if style_data.faint {
        modifiers |= Modifier::DIM;
    }
    if style_data.blink {
        modifiers |= Modifier::SLOW_BLINK;
    }
    if style_data.underline != 0 {
        modifiers |= Modifier::UNDERLINED;
    }
    if style_data.strikethrough {
        modifiers |= Modifier::CROSSED_OUT;
    }
    Ok((
        if symbol.is_empty() && wide != ffi::GhosttyCellWide_GHOSTTY_CELL_WIDE_SPACER_TAIL {
            " ".to_owned()
        } else {
            symbol
        },
        Style::default().fg(fg).bg(bg).add_modifier(modifiers),
        wide == ffi::GhosttyCellWide_GHOSTTY_CELL_WIDE_WIDE,
    ))
}

fn grapheme(cells: ffi::GhosttyRenderStateRowCells) -> Result<String, TerminalError> {
    let mut data = vec![0u8; 32];
    loop {
        let mut buffer = ffi::GhosttyBuffer {
            ptr: data.as_mut_ptr(),
            len: 0,
            cap: data.len(),
        };
        let code = unsafe {
            ffi::ghostty_render_state_row_cells_get(
                cells,
                ffi::GhosttyRenderStateRowCellsData_GHOSTTY_RENDER_STATE_ROW_CELLS_DATA_GRAPHEMES_UTF8,
                (&mut buffer as *mut ffi::GhosttyBuffer).cast(),
            )
        };
        if code == ffi::GhosttyResult_GHOSTTY_OUT_OF_SPACE {
            data.resize(buffer.len, 0);
            continue;
        }
        result(code)?;
        return Ok(String::from_utf8_lossy(&data[..buffer.len]).into_owned());
    }
}

fn resolved_cell_color(
    cells: ffi::GhosttyRenderStateRowCells,
    data: ffi::GhosttyRenderStateRowCellsData,
) -> Option<Color> {
    let mut color = ffi::GhosttyColorRgb::default();
    (unsafe {
        ffi::ghostty_render_state_row_cells_get(
            cells,
            data,
            (&mut color as *mut ffi::GhosttyColorRgb).cast(),
        )
    } == SUCCESS)
        .then(|| rgb(color))
}

fn rgb(color: ffi::GhosttyColorRgb) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn row_text(rendered: &RenderedTerminal, y: u16) -> String {
        (0..rendered.buffer.area.width)
            .map(|x| rendered.buffer[(x, y)].symbol())
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    #[test]
    fn renders_live_cells_and_cursor() {
        let mut terminal = Terminal::new(8, 3, 100, |_| {}).expect("terminal");
        terminal.feed(b"hello");
        let rendered = terminal.render().expect("render");

        assert_eq!(row_text(&rendered, 0), "hello");
        assert_eq!(
            rendered.cursor,
            Some(Cursor {
                x: 5,
                y: 0,
                visible: true
            })
        );
    }

    #[test]
    fn scrolls_the_emulator_viewport_and_hides_cursor() {
        let mut terminal = Terminal::new(8, 3, 100, |_| {}).expect("terminal");
        terminal.feed(b"zero\r\none\r\ntwo\r\nthree\r\nfour");
        let live = terminal.render().expect("live render");
        assert_eq!(row_text(&live, 2), "four");
        assert!(live.scroll.max_offset_from_bottom > 0);

        terminal.scroll_up(1);
        let history = terminal.render().expect("history render");
        assert_eq!(history.scroll.offset_from_bottom, 1);
        assert!(history.cursor.is_none());
        assert_eq!(row_text(&history, 2), "three");
    }

    #[test]
    fn forwards_terminal_query_replies() {
        let replies = Arc::new(Mutex::new(Vec::<u8>::new()));
        let captured = Arc::clone(&replies);
        let mut terminal = Terminal::new(8, 3, 100, move |bytes| {
            captured
                .lock()
                .expect("reply lock")
                .extend_from_slice(bytes);
        })
        .expect("terminal");

        terminal.feed(b"\x1b[6n");

        assert!(!replies.lock().expect("reply lock").is_empty());
    }

    #[test]
    fn exposes_synchronized_output_mode_for_frame_suppression() {
        let mut terminal = Terminal::new(8, 3, 100, |_| {}).expect("terminal");

        terminal.feed(b"\x1b[?2026hpartial");
        assert!(terminal
            .synchronized_output_active()
            .expect("sync mode query"));

        terminal.feed(b" complete\x1b[?2026l");
        assert!(!terminal
            .synchronized_output_active()
            .expect("sync mode query"));
    }

    #[test]
    fn paste_encoding_tracks_the_childs_bracketed_paste_mode() {
        let mut terminal = Terminal::new(20, 4, 100, |_| {}).expect("terminal");
        assert_eq!(terminal.encode_paste(b"one\ntwo").unwrap(), b"one\rtwo");
        terminal.feed(b"\x1b[?2004h");
        assert_eq!(
            terminal.encode_paste(b"one\ntwo").unwrap(),
            b"\x1b[200~one\ntwo\x1b[201~"
        );
    }

    #[test]
    fn mouse_encoder_tracks_child_reporting_modes() {
        let mut terminal = Terminal::new(20, 4, 100, |_| {}).expect("terminal");
        assert!(!terminal.mouse_reporting().unwrap());
        assert!(terminal
            .encode_mouse(MouseAction::Press, Some(1), 0, 1, 2, false)
            .unwrap()
            .is_empty());

        terminal.feed(b"\x1b[?1000h\x1b[?1006h");
        assert!(terminal.mouse_reporting().unwrap());
        assert_eq!(
            terminal
                .encode_mouse(MouseAction::Press, Some(1), 0, 1, 2, false)
                .unwrap(),
            b"\x1b[<0;2;3M"
        );
        assert_eq!(
            terminal
                .encode_mouse(MouseAction::Release, Some(1), 0, 1, 2, true)
                .unwrap(),
            b"\x1b[<0;2;3m"
        );
    }

    #[test]
    fn extracts_plain_text_with_ghostty_selection_formatting() {
        let mut terminal = Terminal::new(20, 4, 100, |_| {}).expect("terminal");
        terminal.feed(b"alpha beta\r\nsecond line");
        assert_eq!(
            terminal
                .read_text_screen((6, 0), (9, 0))
                .expect("selection"),
            "beta"
        );
        assert_eq!(
            terminal
                .read_text_screen((0, 0), (5, 1))
                .expect("selection"),
            "alpha beta\nsecond"
        );
    }

    #[test]
    fn alternate_screen_restores_the_primary_screen() {
        let mut terminal = Terminal::new(12, 3, 100, |_| {}).expect("terminal");
        terminal.feed(b"primary");
        terminal.feed(b"\x1b[?1049h\x1b[Halternate");
        assert_eq!(row_text(&terminal.render().unwrap(), 0), "alternate");
        terminal.feed(b"\x1b[?1049l");
        assert_eq!(row_text(&terminal.render().unwrap(), 0), "primary");
    }

    #[test]
    fn preserves_wide_and_combining_graphemes_as_cells() {
        let mut terminal = Terminal::new(12, 3, 100, |_| {}).expect("terminal");
        terminal.feed("A界e\u{301}".as_bytes());
        let rendered = terminal.render().unwrap();
        assert_eq!(rendered.buffer[(0, 0)].symbol(), "A");
        assert_eq!(rendered.buffer[(1, 0)].symbol(), "界");
        assert!(
            (0..rendered.buffer.area.width).any(|x| rendered.buffer[(x, 0)].symbol() == "e\u{301}")
        );
    }

    #[test]
    fn resize_reflows_text_without_losing_content() {
        let mut terminal = Terminal::new(6, 3, 100, |_| {}).expect("terminal");
        terminal.feed(b"abcdefghij");
        terminal.resize(10, 3).unwrap();
        let rendered = terminal.render().unwrap();
        let text = (0..rendered.buffer.area.height)
            .map(|row| row_text(&rendered, row))
            .collect::<String>();
        assert!(text.contains("abcdefghij"));
    }

    #[test]
    fn absolute_row_anchor_stays_fixed_when_new_output_arrives() {
        let mut terminal = Terminal::new(12, 3, 100, |_| {}).expect("terminal");
        terminal.feed(b"zero\r\none\r\ntwo\r\nthree\r\nfour");
        terminal.set_scroll_row(0);
        let before = row_text(&terminal.render().unwrap(), 0);
        terminal.scroll_to_bottom();
        terminal.feed(b"\r\nfive");
        terminal.set_scroll_row(0);
        let after = row_text(&terminal.render().unwrap(), 0);
        assert_eq!(before, after);
    }
}
