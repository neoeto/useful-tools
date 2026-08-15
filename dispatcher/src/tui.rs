use std::{
    collections::VecDeque,
    io::{self, IsTerminal},
    process::Stdio,
    time::Duration,
};

use crossterm::{
    cursor::Show,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, Command},
    sync::mpsc,
};

const MAX_OUTPUT_LINES: usize = 1_000;

pub async fn run() -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "an interactive terminal is required (use `ut --help` for CLI usage)",
        ));
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run_loop(&mut terminal).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, Show)?;
    terminal.show_cursor()?;
    result
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Tools,
    Parameters,
}

#[derive(Clone)]
enum FieldKind {
    Text,
    Toggle,
    Choice(&'static [&'static str]),
}

#[derive(Clone)]
struct Field {
    label: &'static str,
    hint: &'static str,
    flag: Option<&'static str>,
    value: String,
    kind: FieldKind,
    positional: bool,
    mode: Option<&'static str>,
}

impl Field {
    fn text(
        label: &'static str,
        hint: &'static str,
        flag: Option<&'static str>,
        value: &str,
    ) -> Self {
        Self {
            label,
            hint,
            flag,
            value: value.to_string(),
            kind: FieldKind::Text,
            positional: flag.is_none(),
            mode: None,
        }
    }

    fn toggle(label: &'static str, hint: &'static str, flag: &'static str) -> Self {
        Self {
            label,
            hint,
            flag: Some(flag),
            value: "false".to_string(),
            kind: FieldKind::Toggle,
            positional: false,
            mode: None,
        }
    }

    fn choice(
        label: &'static str,
        hint: &'static str,
        flag: &'static str,
        choices: &'static [&'static str],
        selected: &str,
    ) -> Self {
        Self {
            label,
            hint,
            flag: Some(flag),
            value: selected.to_string(),
            kind: FieldKind::Choice(choices),
            positional: false,
            mode: None,
        }
    }

    fn positional_choice(
        label: &'static str,
        hint: &'static str,
        choices: &'static [&'static str],
        selected: &str,
    ) -> Self {
        Self {
            label,
            hint,
            flag: None,
            value: selected.to_string(),
            kind: FieldKind::Choice(choices),
            positional: true,
            mode: None,
        }
    }

    fn for_mode(mut self, mode: &'static str) -> Self {
        self.mode = Some(mode);
        self
    }

    fn display_value(&self) -> String {
        match self.kind {
            FieldKind::Toggle => {
                if self.value == "true" {
                    "[x]".to_string()
                } else {
                    "[ ]".to_string()
                }
            }
            FieldKind::Choice(_) => format!("< {} >", self.value),
            FieldKind::Text => {
                if self.value.is_empty() {
                    format!("({})", self.hint)
                } else {
                    self.value.clone()
                }
            }
        }
    }

    fn toggle_or_cycle(&mut self, backwards: bool) {
        match self.kind {
            FieldKind::Toggle => {
                self.value = (self.value != "true").to_string();
            }
            FieldKind::Choice(choices) => {
                let current = choices
                    .iter()
                    .position(|choice| *choice == self.value)
                    .unwrap_or(0);
                let next = if backwards {
                    current.checked_sub(1).unwrap_or(choices.len() - 1)
                } else {
                    (current + 1) % choices.len()
                };
                self.value = choices[next].to_string();
            }
            FieldKind::Text => {}
        }
    }
}

struct Tool {
    name: &'static str,
    description: &'static str,
    fields: Vec<Field>,
}

impl Tool {
    fn mode(&self) -> Option<&str> {
        self.fields
            .iter()
            .find(|field| field.label == "Mode")
            .map(|field| field.value.as_str())
    }

    fn active_field_indices(&self) -> Vec<usize> {
        let mode = self.mode();
        self.fields
            .iter()
            .enumerate()
            .filter(|(_, field)| field.mode.is_none() || field.mode == mode)
            .map(|(index, _)| index)
            .collect()
    }

    fn arguments(&self) -> Result<Vec<String>, String> {
        let mut args = vec![self.name.to_string()];
        for field in &self.fields {
            if field.mode.is_some() && field.mode != self.mode() {
                continue;
            }
            match field.kind {
                FieldKind::Toggle => {
                    if field.value == "true" {
                        args.push(field.flag.expect("toggle field has a flag").to_string());
                    }
                }
                FieldKind::Text if field.value.is_empty() => {}
                FieldKind::Text if field.positional => {
                    let values = shell_words::split(&field.value)
                        .map_err(|error| format!("{}: {error}", field.label))?;
                    args.extend(values);
                }
                FieldKind::Choice(_) if field.positional => args.push(field.value.clone()),
                FieldKind::Text | FieldKind::Choice(_) => {
                    args.push(field.flag.expect("option field has a flag").to_string());
                    args.push(field.value.clone());
                }
            }
        }
        Ok(args)
    }
}

struct RunningCommand {
    child: Child,
    command: String,
    control: Option<ChildStdin>,
    graceful: bool,
}

enum OutputEvent {
    Line(&'static str, String),
}

struct App {
    tools: Vec<Tool>,
    selected_tool: usize,
    selected_field: usize,
    focus: Focus,
    editing: bool,
    cursor: usize,
    output: VecDeque<String>,
    output_scroll: u16,
    status: String,
    running: Option<RunningCommand>,
    output_tx: mpsc::UnboundedSender<OutputEvent>,
    output_rx: mpsc::UnboundedReceiver<OutputEvent>,
}

impl App {
    fn new() -> Self {
        let (output_tx, output_rx) = mpsc::unbounded_channel();
        Self {
            tools: tools(),
            selected_tool: 0,
            selected_field: 0,
            focus: Focus::Tools,
            editing: false,
            cursor: 0,
            output: VecDeque::from([
                "Ready. Select a tool, edit its parameters, then press r to run.".to_string(),
            ]),
            output_scroll: 0,
            status: "Ready".to_string(),
            running: None,
            output_tx,
            output_rx,
        }
    }

    fn selected_tool(&self) -> &Tool {
        &self.tools[self.selected_tool]
    }

    fn selected_tool_mut(&mut self) -> &mut Tool {
        &mut self.tools[self.selected_tool]
    }

    fn selected_field_index(&self) -> usize {
        self.selected_tool()
            .active_field_indices()
            .get(self.selected_field)
            .copied()
            .unwrap_or(0)
    }

    fn selected_is_interactive(&self) -> bool {
        self.selected_tool().name == "file-transfer"
            && self.selected_tool().mode() == Some("client")
    }

    fn push_output(&mut self, line: impl Into<String>) {
        if self.output.len() == MAX_OUTPUT_LINES {
            self.output.pop_front();
        }
        self.output.push_back(line.into());
        self.output_scroll = 0;
    }

    fn preview(&self) -> String {
        match self.selected_tool().arguments() {
            Ok(args) => format_command(&args),
            Err(error) => format!("Invalid parameters: {error}"),
        }
    }

    fn move_selection(&mut self, down: bool) {
        match self.focus {
            Focus::Tools => {
                let len = self.tools.len();
                self.selected_tool = if down {
                    (self.selected_tool + 1) % len
                } else {
                    self.selected_tool.checked_sub(1).unwrap_or(len - 1)
                };
                self.selected_field = 0;
            }
            Focus::Parameters => {
                let len = self.selected_tool().active_field_indices().len();
                self.selected_field = if down {
                    (self.selected_field + 1) % len
                } else {
                    self.selected_field.checked_sub(1).unwrap_or(len - 1)
                };
            }
        }
    }

    fn begin_or_change_field(&mut self, backwards: bool) {
        let field_index = self.selected_field_index();
        let kind = self.selected_tool().fields[field_index].kind.clone();
        match kind {
            FieldKind::Text => {
                let field = &self.selected_tool().fields[field_index];
                let cursor = field.value.len();
                let label = field.label;
                self.editing = true;
                self.cursor = cursor;
                self.status = format!("Editing {label} — Enter or Esc to finish");
            }
            FieldKind::Toggle | FieldKind::Choice(_) => {
                self.selected_tool_mut().fields[field_index].toggle_or_cycle(backwards);
            }
        }
    }

    fn edit_key(&mut self, key: KeyEvent) {
        let field_index = self.selected_field_index();
        let cursor = self.cursor;
        let field = &mut self.selected_tool_mut().fields[field_index];
        match key.code {
            KeyCode::Enter => {
                self.editing = false;
                self.status = "Parameter updated".to_string();
            }
            KeyCode::Esc => {
                self.editing = false;
                self.status = "Editing finished".to_string();
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                field.value.clear();
                self.cursor = 0;
            }
            KeyCode::Char(character) => {
                field.value.insert(cursor, character);
                self.cursor += character.len_utf8();
            }
            KeyCode::Backspace if cursor > 0 => {
                let previous = previous_char_boundary(&field.value, cursor);
                field.value.drain(previous..cursor);
                self.cursor = previous;
            }
            KeyCode::Delete if cursor < field.value.len() => {
                let next = next_char_boundary(&field.value, cursor);
                field.value.drain(cursor..next);
            }
            KeyCode::Left if cursor > 0 => {
                self.cursor = previous_char_boundary(&field.value, cursor);
            }
            KeyCode::Right if cursor < field.value.len() => {
                self.cursor = next_char_boundary(&field.value, cursor);
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = field.value.len(),
            _ => {}
        }
    }

    async fn launch(&mut self) {
        if self.running.is_some() {
            self.status = "A tool is already running; press x to stop it first".to_string();
            return;
        }

        let args = match self.selected_tool().arguments() {
            Ok(args) => args,
            Err(error) => {
                self.status = format!("Invalid parameters: {error}");
                return;
            }
        };
        let graceful = args.first().is_some_and(|arg| arg == "file-transfer")
            && args.get(1).is_some_and(|arg| arg == "server");
        let display = format_command(&args);
        let executable = match std::env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                self.status = format!("Cannot locate ut executable: {error}");
                return;
            }
        };

        let mut command = Command::new(executable);
        if graceful {
            command.env("UT_FILE_TRANSFER_CONTROL_STDIN", "1");
        }
        command
            .args(&args)
            .stdin(if graceful {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        match command.spawn() {
            Ok(mut child) => {
                let control = child.stdin.take();
                self.push_output(format!("$ {display}"));
                if let Some(stdout) = child.stdout.take() {
                    spawn_output_reader(stdout, "out", self.output_tx.clone());
                }
                if let Some(stderr) = child.stderr.take() {
                    spawn_output_reader(stderr, "err", self.output_tx.clone());
                }
                self.running = Some(RunningCommand {
                    child,
                    command: display.clone(),
                    control,
                    graceful,
                });
                self.status = format!("Running: {display} (press x to stop)");
            }
            Err(error) => {
                self.status = format!("Failed to run {display}: {error}");
            }
        }
    }

    async fn stop(&mut self) {
        let Some(mut running) = self.running.take() else {
            self.status = "No tool is running".to_string();
            return;
        };
        let command = running.command.clone();
        if running.graceful {
            if let Some(mut control) = running.control.take() {
                if control.write_all(b"shutdown\n").await.is_ok() {
                    let waited =
                        tokio::time::timeout(Duration::from_secs(10), running.child.wait()).await;
                    if matches!(waited, Ok(Ok(_))) {
                        self.push_output(format!("[stopped gracefully] {command}"));
                        self.status = "Tool stopped gracefully".to_string();
                        return;
                    }
                }
            }
        }
        match running.child.kill().await {
            Ok(()) => {
                self.push_output(format!("[stopped] {command}"));
                self.status = "Tool stopped".to_string();
            }
            Err(error) => {
                self.push_output(format!("[error] failed to stop {command}: {error}"));
                self.status = "Failed to stop tool".to_string();
            }
        }
    }

    fn collect_output(&mut self) {
        while let Ok(event) = self.output_rx.try_recv() {
            match event {
                OutputEvent::Line(stream, line) => {
                    let prefix = if stream == "err" { "! " } else { "  " };
                    self.push_output(format!("{prefix}{line}"));
                }
            }
        }
    }

    fn check_child(&mut self) -> io::Result<()> {
        let status = match self.running.as_mut() {
            Some(running) => running.child.try_wait()?,
            None => None,
        };
        if let Some(exit_status) = status {
            let command = self.running.take().expect("running command exists").command;
            self.push_output(format!("[finished: {exit_status}] {command}"));
            self.status = format!("Finished: {exit_status}");
        }
        Ok(())
    }
}

async fn run_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
    let mut app = App::new();
    loop {
        app.collect_output();
        app.check_child()?;
        terminal.draw(|frame| draw(frame, &app))?;

        if !event::poll(Duration::from_millis(75))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if app.editing {
            app.edit_key(key);
            continue;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                if app.running.is_some() {
                    app.stop().await;
                }
                break;
            }
            KeyCode::Tab => {
                app.focus = match app.focus {
                    Focus::Tools => Focus::Parameters,
                    Focus::Parameters => Focus::Tools,
                };
            }
            KeyCode::Up | KeyCode::Char('k') => app.move_selection(false),
            KeyCode::Down | KeyCode::Char('j') => app.move_selection(true),
            KeyCode::Left | KeyCode::Char('h') if app.focus == Focus::Parameters => {
                app.begin_or_change_field(true);
            }
            KeyCode::Right | KeyCode::Char('l') if app.focus == Focus::Parameters => {
                app.begin_or_change_field(false);
            }
            KeyCode::Enter | KeyCode::Char(' ') if app.focus == Focus::Parameters => {
                app.begin_or_change_field(false);
            }
            KeyCode::Char('r') if app.selected_is_interactive() => {
                if app.running.is_some() {
                    app.status = "A tool is already running; press x to stop it first".to_string();
                } else {
                    match app.selected_tool().arguments() {
                        Ok(args) => {
                            let display = format_command(&args);
                            let status = launch_interactive(terminal, &args).await;
                            match status {
                                Ok(status) => {
                                    app.push_output(format!("[interactive: {status}] {display}"));
                                    app.status = format!("Client exited: {status}");
                                }
                                Err(error) => app.status = format!("Failed to run client: {error}"),
                            }
                        }
                        Err(error) => app.status = format!("Invalid parameters: {error}"),
                    }
                }
            }
            KeyCode::Char('r') => app.launch().await,
            KeyCode::Char('x') => app.stop().await,
            KeyCode::PageUp => app.output_scroll = app.output_scroll.saturating_add(5),
            KeyCode::PageDown => app.output_scroll = app.output_scroll.saturating_sub(5),
            _ => {}
        }
    }
    Ok(())
}

fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(12),
            Constraint::Length(3),
            Constraint::Length(3),
        ])
        .split(area);

    draw_header(frame, sections[0]);
    draw_body(frame, sections[1], app);
    draw_command(frame, sections[2], app);
    draw_status(frame, sections[3], app);
}

fn draw_header(frame: &mut Frame<'_>, area: Rect) {
    let title = Line::from(vec![
        Span::styled(
            " UT ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  Useful Tools · interactive launcher"),
    ]);
    frame.render_widget(
        Paragraph::new(title).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn draw_body(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(34), Constraint::Percentage(66)])
        .split(area);

    let tool_items = app
        .tools
        .iter()
        .map(|tool| {
            ListItem::new(vec![
                Line::styled(tool.name, Style::default().add_modifier(Modifier::BOLD)),
                Line::styled(tool.description, Style::default().fg(Color::DarkGray)),
            ])
        })
        .collect::<Vec<_>>();
    let tool_border = if app.focus == Focus::Tools {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let tools = List::new(tool_items)
        .block(
            Block::default()
                .title(" Tools ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(tool_border)),
        )
        .highlight_symbol("▸ ")
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
    let mut tool_state = ListState::default().with_selected(Some(app.selected_tool));
    frame.render_stateful_widget(tools, columns[0], &mut tool_state);

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
        .split(columns[1]);
    draw_parameters(frame, right[0], app);
    draw_output(frame, right[1], app);
}

fn draw_parameters(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let active_indices = app.selected_tool().active_field_indices();
    let items = app
        .selected_tool()
        .fields
        .iter()
        .enumerate()
        .filter(|(index, _)| active_indices.contains(index))
        .map(|(_, field)| field)
        .map(|field| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:<13}", field.label),
                    Style::default().fg(Color::Gray),
                ),
                Span::raw(field.display_value()),
            ]))
        })
        .collect::<Vec<_>>();
    let border = if app.focus == Focus::Parameters {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let title = format!(" Parameters · {} ", app.selected_tool().name);
    let parameters = List::new(items)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(border)),
        )
        .highlight_symbol("▸ ")
        .highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White));
    let selected = (app.focus == Focus::Parameters).then_some(app.selected_field);
    let mut state = ListState::default().with_selected(selected);
    frame.render_stateful_widget(parameters, area, &mut state);

    if app.editing && area.width > 18 && area.height > app.selected_field as u16 + 2 {
        let label_width = 13usize;
        let field_index = app.selected_field_index();
        let visible_cursor = app.selected_tool().fields[field_index].value[..app.cursor]
            .chars()
            .count() as u16;
        let x = area
            .x
            .saturating_add(1 + 2 + label_width as u16 + visible_cursor)
            .min(area.right().saturating_sub(2));
        let y = area.y + 1 + app.selected_field as u16;
        frame.set_cursor_position((x, y));
    }
}

fn draw_output(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let lines = app
        .output
        .iter()
        .map(|line| Line::raw(line.as_str()))
        .collect::<Vec<_>>();
    let visible_height = area.height.saturating_sub(2) as usize;
    let bottom = app.output.len().saturating_sub(visible_height) as u16;
    let scroll = bottom.saturating_sub(app.output_scroll);
    let output = Paragraph::new(lines)
        .block(Block::default().title(" Output ").borders(Borders::ALL))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(output, area);
}

fn draw_command(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let preview = app.preview();
    frame.render_widget(
        Paragraph::new(preview)
            .style(Style::default().fg(Color::Yellow))
            .block(
                Block::default()
                    .title(" Command preview ")
                    .borders(Borders::ALL),
            ),
        area,
    );
}

fn draw_status(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let help = if app.editing {
        "Type to edit · Ctrl+U clear · Enter save · Esc finish"
    } else if app.running.is_some() {
        "Tab switch · ↑↓ navigate · PgUp/PgDn output · x stop · q quit"
    } else {
        "Tab switch · ↑↓ navigate · Enter edit/toggle · ←→ choose · r run · q quit"
    };
    let line = Line::from(vec![
        Span::styled(
            format!(" {} ", app.status),
            Style::default().fg(Color::Green),
        ),
        Span::styled(help, Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(
        Paragraph::new(line).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn spawn_output_reader<R>(reader: R, stream: &'static str, tx: mpsc::UnboundedSender<OutputEvent>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if tx.send(OutputEvent::Line(stream, line)).is_err() {
                break;
            }
        }
    });
}

async fn launch_interactive(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    args: &[String],
) -> io::Result<std::process::ExitStatus> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, Show)?;
    terminal.show_cursor()?;
    let executable = std::env::current_exe()?;
    let result = Command::new(executable).args(args).status().await;
    enable_raw_mode()?;
    execute!(terminal.backend_mut(), EnterAlternateScreen)?;
    terminal.clear()?;
    result
}

fn format_command(args: &[String]) -> String {
    std::iter::once("ut".to_string())
        .chain(args.iter().map(|arg| shell_words::quote(arg).into_owned()))
        .collect::<Vec<_>>()
        .join(" ")
}

fn previous_char_boundary(value: &str, cursor: usize) -> usize {
    value[..cursor]
        .char_indices()
        .next_back()
        .map(|(index, _)| index)
        .unwrap_or(0)
}

fn next_char_boundary(value: &str, cursor: usize) -> usize {
    value[cursor..]
        .char_indices()
        .nth(1)
        .map(|(index, _)| cursor + index)
        .unwrap_or(value.len())
}

fn tools() -> Vec<Tool> {
    const ALGORITHMS: &[&str] = &[
        "md5", "sha1", "sha224", "sha256", "sha384", "sha512", "sha3-224", "sha3-256", "sha3-384",
        "sha3-512", "blake2b", "blake2s", "blake3",
    ];

    vec![
        Tool {
            name: "uuid-gen",
            description: "Batch generate UUIDs",
            fields: vec![
                Field::text("Count", "number of UUIDs", Some("--count"), "1"),
                Field::toggle("No hyphens", "strip hyphens", "--no-hyphens"),
            ],
        },
        Tool {
            name: "file-server",
            description: "Serve and upload files over HTTP",
            fields: vec![
                Field::text("Directory", "directory to serve", Some("--dir"), "."),
                Field::text("Port", "listen port", Some("--port"), "8080"),
                Field::text("Host", "bind address", Some("--host"), "0.0.0.0"),
            ],
        },
        Tool {
            name: "http-echo",
            description: "Inspect and echo HTTP requests",
            fields: vec![
                Field::text("Port", "listen port", Some("--port"), "8081"),
                Field::text("Host", "bind address", Some("--host"), "0.0.0.0"),
            ],
        },
        Tool {
            name: "file-hash",
            description: "Hash files or verify checksums",
            fields: vec![
                Field::choice(
                    "Algorithm",
                    "hash algorithm",
                    "--algorithm",
                    ALGORITHMS,
                    "sha256",
                ),
                Field::text("Files", "space-separated paths", None, ""),
                Field::toggle("Uppercase", "uppercase output", "--uppercase"),
                Field::toggle("Check", "verify checksum files", "--check"),
                Field::toggle("Quiet", "reduced output", "--quiet"),
            ],
        },
        Tool {
            name: "file-transfer",
            description: "Browse and transfer files over TCP",
            fields: vec![
                Field::positional_choice(
                    "Mode",
                    "server or client",
                    &["server", "client"],
                    "server",
                ),
                Field::text("Directory", "shared directory", Some("--dir"), ".").for_mode("server"),
                Field::text("Bind", "listen address", Some("--bind"), "0.0.0.0:9417")
                    .for_mode("server"),
                Field::toggle("Authentication", "require a token", "--auth").for_mode("server"),
                Field::text(
                    "Max clients",
                    "connection limit",
                    Some("--max-connections"),
                    "8",
                )
                .for_mode("server"),
                Field::text("Idle timeout", "seconds", Some("--idle-timeout"), "60")
                    .for_mode("server"),
                Field::text("Server", "host or host:port", None, "127.0.0.1:9417")
                    .for_mode("client"),
                Field::text("Output", "download directory", Some("--output"), ".")
                    .for_mode("client"),
                Field::toggle("Overwrite", "replace different files", "--overwrite")
                    .for_mode("client"),
                Field::toggle("Save token", "remember entered token", "--save-token")
                    .for_mode("client"),
                Field::text("Retries", "reconnection attempts", Some("--retries"), "3")
                    .for_mode("client"),
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_default_uuid_command() {
        let args = tools()[0].arguments().unwrap();
        assert_eq!(args, ["uuid-gen", "--count", "1"]);
    }

    #[test]
    fn parses_quoted_positional_paths() {
        let mut tools = tools();
        tools[3].fields[1].value = "'one file.txt' two.txt".to_string();
        let args = tools[3].arguments().unwrap();
        assert_eq!(
            args,
            [
                "file-hash",
                "--algorithm",
                "sha256",
                "one file.txt",
                "two.txt"
            ]
        );
    }

    #[test]
    fn character_boundaries_support_unicode() {
        let value = "a中b";
        assert_eq!(next_char_boundary(value, 1), 4);
        assert_eq!(previous_char_boundary(value, 4), 1);
    }
}
