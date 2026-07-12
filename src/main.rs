use std::io::{self, Stdout};
use std::sync::{Arc, OnceLock};

use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame, Terminal,
};
use tokio::{
    io::{split, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::mpsc,
};
use tokio_rustls::{
    rustls::{pki_types::ServerName, ClientConfig, RootCertStore},
    TlsConnector,
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
    /// Number of *wrapped* lines the output currently renders to, refreshed
    /// every draw call. Scroll math is done against this, not `output.len()`,
    /// since word-wrapping can turn one raw line into several rendered ones.
    last_wrapped_len: usize,
}

impl App {
    fn new() -> Self {
        let output = vec![
            "Welcome! Type a message and press Enter to submit.".to_string(),
            "Use PageUp / PageDown to scroll this output area.".to_string(),
            "Try /connect irc.libera.chat:6667, or /connect --tls irc.libera.chat:6697.".to_string(),
        ];
        Self {
            output,
            input: Vec::new(),
            input_cursor: 0,
            input_scroll: 0,
            scroll: 0,
            follow_bottom: true,
            last_output_height: 0,
            last_wrapped_len: 0,
        }
    }

    fn max_scroll(&self) -> usize {
        self.last_wrapped_len.saturating_sub(self.last_output_height.max(1))
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
        // Scroll position is re-derived from the fresh wrapped-line count on
        // the next draw call (see clamp_scroll), so nothing to do here.
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
        // space trailing past the end of the text. When the cursor sits
        // right after the last character (append position), one extra
        // column must stay reserved for it -- otherwise this clamp pulls
        // the scroll back and the cursor gets rendered one column past the
        // visible area, which terminals clip back onto the last character.
        let effective_len = if self.input_cursor >= self.input.len() {
            self.input.len() + 1
        } else {
            self.input.len()
        };
        let max_scroll = effective_len.saturating_sub(width);
        if self.input_scroll > max_scroll {
            self.input_scroll = max_scroll;
        }
    }
}

/// Wraps a single line of text to `width` columns, breaking on word (space)
/// boundaries where possible. A single word longer than `width` is hard-broken
/// mid-word since there's no boundary to break on. Always returns at least one
/// (possibly empty) line.
fn wrap_line(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    if line.is_empty() {
        return vec![String::new()];
    }

    let mut result = Vec::new();
    let mut current = String::new();

    for word in line.split(' ') {
        wrap_word(&mut result, &mut current, word, width);
    }
    result.push(current);
    result
}

fn wrap_word(result: &mut Vec<String>, current: &mut String, word: &str, width: usize) {
    let word_len = word.chars().count();

    // Word alone doesn't fit on any line -- hard-break it by characters.
    if word_len > width {
        if !current.is_empty() {
            result.push(std::mem::take(current));
        }
        let mut chunk = String::new();
        for ch in word.chars() {
            if chunk.chars().count() == width {
                result.push(std::mem::take(&mut chunk));
            }
            chunk.push(ch);
        }
        *current = chunk; // leftover partial chunk continues accumulating
        return;
    }

    let needs_space = !current.is_empty();
    let extra = if needs_space { 1 } else { 0 };
    if current.chars().count() + extra + word_len > width {
        result.push(std::mem::take(current));
    } else if needs_space {
        current.push(' ');
    }
    current.push_str(word);
}

// ---------------------------------------------------------------------------
// IRC support
// ---------------------------------------------------------------------------

/// A minimally-parsed IRC line: `[:prefix] COMMAND [params...] [:trailing]`.
/// `params` holds middle params followed by the trailing param (if any) as a
/// single last element, matching how most IRC messages are consumed.
struct IrcMessage {
    prefix: Option<String>,
    command: String,
    params: Vec<String>,
}

/// Parses one raw IRC protocol line (no trailing CR/LF) into its parts.
/// This is deliberately minimal -- just enough structure to strip numerics
/// and to spot PING -- not a full IRCv3 parser.
fn parse_irc_line(line: &str) -> IrcMessage {
    let mut rest = line;
    let mut prefix = None;

    if let Some(stripped) = rest.strip_prefix(':') {
        match stripped.find(' ') {
            Some(idx) => {
                prefix = Some(stripped[..idx].to_string());
                rest = &stripped[idx + 1..];
            }
            None => {
                prefix = Some(stripped.to_string());
                rest = "";
            }
        }
    }

    let mut params = Vec::new();
    let command;
    if let Some(idx) = rest.find(" :") {
        let (head, trailing) = rest.split_at(idx);
        let trailing = &trailing[2..]; // skip over " :"
        let mut parts = head.split_whitespace();
        command = parts.next().unwrap_or("").to_string();
        params.extend(parts.map(str::to_string));
        params.push(trailing.to_string());
    } else {
        let mut parts = rest.split_whitespace();
        command = parts.next().unwrap_or("").to_string();
        params.extend(parts.map(str::to_string));
    }

    IrcMessage { prefix, command, params }
}

/// Extension point for future per-numeric handling (e.g. tracking the
/// nickname the server confirmed in 001, populating a channel list from
/// 353, etc). Currently a no-op -- wire up a registry here (for example a
/// `HashMap<u16, Vec<Box<dyn Fn(&IrcMessage)>>>` on `App`) when that's needed.
fn dispatch_numeric(_code: u16, _msg: &IrcMessage) {}

/// Formats a raw IRC line for display in the output area. Numeric replies
/// (e.g. `001`, `353`, `376`) are routed through `dispatch_numeric` and then
/// stripped from the visible text -- the sender and message body still show,
/// just without the noisy 3-digit code.
fn format_irc_line(raw: &str) -> String {
    let msg = parse_irc_line(raw);

    let is_numeric = msg.command.len() == 3 && msg.command.bytes().all(|b| b.is_ascii_digit());
    if is_numeric {
        if let Ok(code) = msg.command.parse::<u16>() {
            dispatch_numeric(code, &msg);
        }
    }

    let mut out = String::new();
    if let Some(prefix) = &msg.prefix {
        out.push_str(prefix);
        out.push(' ');
    }
    if !is_numeric {
        out.push_str(&msg.command);
        out.push(' ');
    }
    out.push_str(&msg.params.join(" "));
    out.trim().to_string()
}

/// Any duplex byte stream we can speak the IRC protocol over -- lets
/// `spawn_connection` treat a plain `TcpStream` and a TLS-wrapped one
/// identically after the handshake.
trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> AsyncStream for T {}

/// Lazily-built, shared TLS client config using Mozilla's bundled root
/// store (via `webpki-roots`) rather than the OS trust store, so this works
/// the same in a bare container as on a full desktop. Built once and reused
/// across connections.
fn tls_client_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
}

/// Performs a TLS handshake over an already-connected TCP stream, verifying
/// the server's certificate against `host` (used as the SNI / hostname to
/// validate -- not just for the initial DNS lookup).
async fn upgrade_to_tls(host: &str, tcp: TcpStream) -> io::Result<impl AsyncStream> {
    let connector = TlsConnector::from(tls_client_config());
    let server_name = ServerName::try_from(host.to_string())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("invalid hostname {host:?}: {e}")))?;
    connector.connect(server_name, tcp).await
}

/// Turns a user-typed `/connect` argument into (host, port, use_tls).
/// Accepts an optional leading `--tls` (or `-tls`) flag, e.g.
/// `--tls irc.libera.chat:6697` or plain `irc.libera.chat`. Defaults the
/// port to IRC's traditional plaintext port (6667), or 6697 when TLS is
/// requested and no port is given.
fn parse_connect_args(arg: &str) -> Option<(String, u16, bool)> {
    let mut rest = arg.trim();
    let mut tls = false;
    for flag in ["--tls", "-tls"] {
        if let Some(stripped) = rest.strip_prefix(flag) {
            tls = true;
            rest = stripped.trim();
            break;
        }
    }
    if rest.is_empty() {
        return None;
    }

    let default_port = if tls { 6697 } else { 6667 };
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) => match p.parse::<u16>() {
            Ok(port) => (h.to_string(), port),
            Err(_) => (rest.to_string(), default_port), // not "host:port" (e.g. bare IPv6) -- use as-is
        },
        None => (rest.to_string(), default_port),
    };
    Some((host, port, tls))
}

/// Connects to `host:port` (optionally over TLS) and relays every line the
/// server sends back through `out_tx`. Also takes `cmd_rx`, a channel the UI
/// can use to send raw lines out to the server (e.g. forwarding whatever the
/// user types once connected). Runs until the connection closes or errors.
fn spawn_connection(
    host: String,
    port: u16,
    tls: bool,
    out_tx: mpsc::UnboundedSender<String>,
    mut cmd_rx: mpsc::UnboundedReceiver<String>,
) {
    tokio::spawn(async move {
        let addr = format!("{host}:{port}");
        let tcp = match TcpStream::connect(&addr).await {
            Ok(s) => s,
            Err(e) => {
                let _ = out_tx.send(format!("* Connection to {addr} failed: {e}"));
                return;
            }
        };

        // Box the stream behind the shared trait so the rest of this
        // function doesn't care whether TLS is in play.
        let stream: Box<dyn AsyncStream> = if tls {
            match upgrade_to_tls(&host, tcp).await {
                Ok(s) => Box::new(s),
                Err(e) => {
                    let _ = out_tx.send(format!("* TLS handshake with {addr} failed: {e}"));
                    return;
                }
            }
        } else {
            Box::new(tcp)
        };

        let scheme = if tls { "TLS" } else { "plaintext" };
        let _ = out_tx.send(format!("* Connected to {addr} ({scheme})"));

        let (reader, mut writer) = split(stream);
        let mut lines = BufReader::new(reader).lines();

        // Minimal registration so the server actually talks back with
        // numerics instead of just waiting on us.
        let _ = writer.write_all(b"NICK tui_user\r\n").await;
        let _ = writer.write_all(b"USER tui_user 0 * :Rust TUI IRC client\r\n").await;

        loop {
            tokio::select! {
                line = lines.next_line() => {
                    match line {
                        Ok(Some(raw)) => {
                            // Keepalive: servers periodically send PING and
                            // will disconnect us if we don't PONG back.
                            let parsed = parse_irc_line(&raw);
                            if parsed.command.eq_ignore_ascii_case("PING") {
                                let token = parsed.params.last().cloned().unwrap_or_default();
                                let _ = writer.write_all(format!("PONG :{token}\r\n").as_bytes()).await;
                            }
                            if out_tx.send(raw).is_err() {
                                break; // UI side went away
                            }
                        }
                        Ok(None) => {
                            let _ = out_tx.send(format!("* Disconnected from {addr}"));
                            break;
                        }
                        Err(e) => {
                            let _ = out_tx.send(format!("* Read error from {addr}: {e}"));
                            break;
                        }
                    }
                }
                Some(cmd) = cmd_rx.recv() => {
                    let _ = writer.write_all(cmd.as_bytes()).await;
                    let _ = writer.write_all(b"\r\n").await;
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// App wiring
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    let result = run_app(&mut terminal, &mut app).await;

    // Always restore the terminal, even if run_app returned an error.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    if let Err(e) = result {
        eprintln!("Error: {e}");
    }
    Ok(())
}

async fn run_app(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> io::Result<()> {
    // Incoming lines from whichever IRC connection is currently active (and
    // any that came before it) all flow through this single channel.
    let (net_tx, mut net_rx) = mpsc::unbounded_channel::<String>();
    // Outgoing raw lines to the *currently* active connection, if any.
    let mut outgoing_tx: Option<mpsc::UnboundedSender<String>> = None;

    let mut events = EventStream::new();

    loop {
        terminal.draw(|f| ui(f, app))?;

        tokio::select! {
            maybe_event = events.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => {
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
                                    handle_submitted_line(app, msg, &net_tx, &mut outgoing_tx);
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
                    // Resize needs no explicit handling: the next terminal.draw()
                    // re-measures the output/input areas and the clamp_* methods
                    // adjust scroll positions to fit.
                    Some(Ok(_)) => {}
                    Some(Err(_)) => {}
                    None => return Ok(()), // event stream closed
                }
            }
            Some(line) = net_rx.recv() => {
                app.push_output(format_irc_line(&line));
            }
        }
    }
}

/// Handles one submitted input line: `/connect [--tls] <host[:port]>` starts
/// a new connection; anything else is sent raw to the active connection if
/// there is one, or just echoed locally as a harmless fallback.
fn handle_submitted_line(
    app: &mut App,
    msg: String,
    net_tx: &mpsc::UnboundedSender<String>,
    outgoing_tx: &mut Option<mpsc::UnboundedSender<String>>,
) {
    if let Some(rest) = msg.strip_prefix("/connect") {
        let Some((host, port, tls)) = parse_connect_args(rest) else {
            app.push_output("Usage: /connect [--tls] <host[:port]>".to_string());
            return;
        };
        let scheme = if tls { "TLS" } else { "plaintext" };
        app.push_output(format!("* Connecting to {host}:{port} ({scheme})..."));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<String>();
        *outgoing_tx = Some(cmd_tx);
        spawn_connection(host, port, tls, net_tx.clone(), cmd_rx);
        return;
    }

    match outgoing_tx {
        Some(tx) => {
            let _ = tx.send(msg);
        }
        None => app.push_output(format!("> {msg}")),
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
    // No border, so the full area is the visible line/column budget.
    let width = area.width as usize;
    let height = area.height as usize;

    // Re-wrap every draw against the current width -- this is what makes a
    // resize (which changes wrapping, not just visible line count) come out
    // correct: the wrapped total is recomputed fresh each frame.
    let wrapped: Vec<String> = app
        .output
        .iter()
        .flat_map(|line| wrap_line(line, width))
        .collect();

    app.last_output_height = height;
    app.last_wrapped_len = wrapped.len();
    app.clamp_scroll();

    let lines: Vec<Line> = wrapped.into_iter().map(|s| Line::from(Span::raw(s))).collect();

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
