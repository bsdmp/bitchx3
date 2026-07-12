use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame, Terminal,
};

/// Holds all mutable application state.
struct App {
    output: Vec<String>,
    /// Input text stored as chars (not a String) so cursor movement and
    /// insertion at arbitrary positions don't have to worry about UTF-8
    /// byte-boundary slicing.
    input: Vec<char>,
    /// Cursor position as a char index into `input`, in `0..=input.len()`.
    input_cursor: usize,
    /// Number of chars scrolled off the left edge of the input line.
    input_scroll: usize,
    /// Number of lines scrolled down from the top of `output`.
    scroll: usize,
    /// When true, the view auto-follows new output (sticks to the bottom).
    /// This is what makes resizing feel natural: if you were at the bottom,
    /// you stay at the bottom after a resize; if you'd scrolled up, your
    /// position is preserved (and only clamped if it would go out of range).
    follow_bottom: bool,
    /// Height (in lines) of the output area, refreshed every draw call.
    last_output_height: usize,
}

impl App {
    fn new() -> Self {
        let output = vec![
            "Welcome! Type a message and press Enter to submit.".to_string(),
            "Use PageUp / PageDown to scroll this output area.".to_string(),
            "Resize the terminal at any time -- scroll position is preserved.".to_string(),
        ];
        Self {
            output,
            input: Vec::new(),
            input_cursor: 0,
            input_scroll: 0,
            scroll: 0,
            follow_bottom: true,
            last_output_height: 0,
        }
    }

    fn max_scroll(&self) -> usize {
        self.output.len().saturating_sub(self.last_output_height.max(1))
    }

    /// Re-validate `scroll` against the current output height. Call this
    /// every frame (after the output area's height is known) so a resize
    /// is honored immediately instead of only on the next keypress.
    fn clamp_scroll(&mut self) {
        let max = self.max_scroll();
        if self.follow_bottom {
            self.scroll = max;
        } else if self.scroll > max {
            self.scroll = max;
        }
    }

    fn push_output(&mut self, line: String) {
        self.output.push(line);
        if self.follow_bottom {
            self.scroll = self.max_scroll();
        }
    }

    fn page_up(&mut self) {
        let page = self.last_output_height.max(1);
        self.scroll = self.scroll.saturating_sub(page);
        self.follow_bottom = self.scroll >= self.max_scroll();
    }

    fn page_down(&mut self) {
        let page = self.last_output_height.max(1);
        let max = self.max_scroll();
        self.scroll = (self.scroll + page).min(max);
        self.follow_bottom = self.scroll >= max;
    }

    fn insert_char(&mut self, c: char) {
        self.input.insert(self.input_cursor, c);
        self.input_cursor += 1;
    }

    fn backspace(&mut self) {
        if self.input_cursor > 0 {
            self.input_cursor -= 1;
            self.input.remove(self.input_cursor);
        }
    }

    fn delete_forward(&mut self) {
        if self.input_cursor < self.input.len() {
            self.input.remove(self.input_cursor);
        }
    }

    fn move_left(&mut self) {
        self.input_cursor = self.input_cursor.saturating_sub(1);
    }

    fn move_right(&mut self) {
        self.input_cursor = (self.input_cursor + 1).min(self.input.len());
    }

    fn move_home(&mut self) {
        self.input_cursor = 0;
    }

    fn move_end(&mut self) {
        self.input_cursor = self.input.len();
    }

    fn submit_input(&mut self) -> Option<String> {
        if self.input.is_empty() {
            return None;
        }
        let msg: String = std::mem::take(&mut self.input).into_iter().collect();
        self.input_cursor = 0;
        self.input_scroll = 0;
        Some(msg)
    }

    /// Keep the cursor within the visible window of the input line, scrolling
    /// horizontally like a text field when the content is wider than `width`.
    /// Called every draw with the current line width, so it self-corrects on
    /// resize just like the output area's vertical scroll does.
    fn clamp_input_scroll(&mut self, width: usize) {
        let width = width.max(1);
        if self.input_cursor < self.input_scroll {
            // Cursor moved left past the visible window -- scroll left to it.
            self.input_scroll = self.input_cursor;
        } else if self.input_cursor >= self.input_scroll + width {
            // Cursor moved right past the visible window -- scroll right so
            // the cursor lands on the last visible column.
            self.input_scroll = self.input_cursor + 1 - width;
        }
        // If the line got shorter (deletion) or the window got wider
        // (resize), don't leave a scroll position with unnecessary blank
        // space trailing past the end of the text.
        let max_scroll = self.input.len().saturating_sub(width);
        if self.input_scroll > max_scroll {
            self.input_scroll = max_scroll;
        }
    }
}

fn main() -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    let result = run_app(&mut terminal, &mut app);

    // Always restore the terminal, even if run_app returned an error.
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(e) = result {
        eprintln!("Error: {e}");
    }
    Ok(())
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> io::Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;

        // Poll with a timeout so the UI stays responsive even with no input,
        // and so resize events (which crossterm delivers between reads) get
        // picked up promptly.
        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Key(key) => {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    match key.code {
                        KeyCode::Esc => return Ok(()),
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            return Ok(())
                        }
                        KeyCode::Enter => {
                            if let Some(msg) = app.submit_input() {
                                app.push_output(format!("> {msg}"));
                            }
                        }
                        KeyCode::Backspace => app.backspace(),
                        KeyCode::Delete => app.delete_forward(),
                        KeyCode::Left => app.move_left(),
                        KeyCode::Right => app.move_right(),
                        KeyCode::Home => app.move_home(),
                        KeyCode::End => app.move_end(),
                        KeyCode::Char(c) => app.insert_char(c),
                        KeyCode::PageUp => app.page_up(),
                        KeyCode::PageDown => app.page_down(),
                        _ => {}
                    }
                }
                // No special handling needed here: the next terminal.draw()
                // call naturally re-measures the output area and clamp_scroll()
                // adjusts the scroll position to fit.
                Event::Resize(_, _) => {}
                _ => {}
            }
        }
    }
}

fn ui(f: &mut Frame<'_>, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),    // output area -- takes remaining space
            Constraint::Length(1), // status bar -- fixed 1 line
            Constraint::Length(1), // input area -- fixed 1 line
        ])
        .split(f.area());

    draw_output(f, app, chunks[0]);
    draw_status_bar(f, chunks[1]);
    draw_input(f, app, chunks[2]);
}

fn draw_output(f: &mut Frame<'_>, app: &mut App, area: Rect) {
    // No border now, so the full area height is the visible line count.
    let inner_height = area.height as usize;

    // Update the known height *before* clamping, so a resize is honored
    // on this very frame rather than lagging a keypress behind.
    app.last_output_height = inner_height;
    app.clamp_scroll();

    let lines: Vec<Line> = app.output.iter().map(|s| Line::from(Span::raw(s.clone()))).collect();

    let paragraph = Paragraph::new(lines).scroll((app.scroll as u16, 0));

    f.render_widget(paragraph, area);
}

fn draw_status_bar(f: &mut Frame<'_>, area: Rect) {
    // Fixed example text for now -- replace with real status later.
    let status = Paragraph::new(Line::from(vec![Span::styled(
        " STATUS: connected | mode: normal | PgUp/PgDn scroll | Esc quit ",
        Style::default()
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )]));
    f.render_widget(status, area);
}

fn draw_input(f: &mut Frame<'_>, app: &mut App, area: Rect) {
    // Prefix so it's visually distinguishable from the output area.
    const PREFIX: &str = "> ";
    let prefix_len = PREFIX.len() as u16;
    let avail_width = area.width.saturating_sub(prefix_len) as usize;

    // Re-clamp every draw (not just on keypress) so a terminal resize
    // immediately reflows which slice of the input is visible.
    app.clamp_input_scroll(avail_width);

    let visible: String = app
        .input
        .iter()
        .skip(app.input_scroll)
        .take(avail_width)
        .collect();
    let text = format!("{PREFIX}{visible}");

    let paragraph = Paragraph::new(text);
    f.render_widget(paragraph, area);

    // Cursor column is relative to the scrolled window, not the full input.
    let cursor_col = (app.input_cursor - app.input_scroll) as u16;
    let cursor_x = area.x + prefix_len + cursor_col;
    let cursor_y = area.y;
    f.set_cursor_position((cursor_x, cursor_y));
}
