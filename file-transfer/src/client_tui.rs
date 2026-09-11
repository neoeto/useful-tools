use crate::{
    client::{
        download_plan, expand_paths, format_bytes, format_eta, preflight_collisions, Api,
        ClientArgs, DownloadSummary, Progress, ProgressCallback, ProgressState,
    },
    pathing::safe_local_path,
    protocol::{ListEntry, SortBy},
};
use chrono::{DateTime, Local};
use crossterm::{
    cursor::Show,
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table, TableState},
    Frame, Terminal,
};
use std::{
    collections::HashSet,
    io,
    sync::Arc,
    time::{Duration, Instant, UNIX_EPOCH},
};
use tokio::{sync::mpsc, task::JoinHandle};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const MAX_FILENAME_WIDTH: usize = 40;
const MIN_FILENAME_WIDTH: usize = 8;
const MARQUEE_PAUSE: Duration = Duration::from_millis(700);
const MARQUEE_STEP: Duration = Duration::from_millis(160);

enum DownloadEvent {
    Progress(Progress),
    Done(DownloadSummary),
    Error(String),
}

struct App {
    api: Api,
    args: ClientArgs,
    current: String,
    entries: Vec<ListEntry>,
    selected: usize,
    marked: HashSet<String>,
    next_cursor: Option<usize>,
    sort: SortBy,
    search: String,
    editing_search: bool,
    selected_since: Instant,
    status: String,
    progress: Option<Progress>,
    download_task: Option<JoinHandle<()>>,
    tx: mpsc::UnboundedSender<DownloadEvent>,
    rx: mpsc::UnboundedReceiver<DownloadEvent>,
}

pub(crate) async fn run(api: Api, args: ClientArgs) -> io::Result<()> {
    let page = api.list_page("", 0, SortBy::Name).await?;
    let (tx, rx) = mpsc::unbounded_channel();
    let mut app = App {
        api,
        args,
        current: String::new(),
        entries: page.entries,
        selected: 0,
        marked: HashSet::new(),
        next_cursor: page.next_cursor,
        sort: SortBy::Name,
        search: String::new(),
        editing_search: false,
        selected_since: Instant::now(),
        status: "Ready".to_string(),
        progress: None,
        download_task: None,
        tx,
        rx,
    };

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    let result = run_loop(&mut terminal, &mut app).await;
    if let Some(task) = app.download_task.take() {
        task.abort();
    }
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, Show)?;
    terminal.show_cursor()?;
    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> io::Result<()> {
    loop {
        app.collect_download_events();
        terminal.draw(|frame| draw(frame, app))?;
        if !event::poll(Duration::from_millis(75))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        if app.editing_search {
            match key.code {
                KeyCode::Enter | KeyCode::Esc => app.editing_search = false,
                KeyCode::Backspace => {
                    app.search.pop();
                    app.selected = 0;
                    app.reset_name_scroll();
                }
                KeyCode::Char(character) => {
                    app.search.push(character);
                    app.selected = 0;
                    app.reset_name_scroll();
                }
                _ => {}
            }
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                if app.download_task.is_some() {
                    app.status = "Download active; press c to cancel before leaving".to_string();
                } else {
                    break;
                }
            }
            KeyCode::Up | KeyCode::Char('k') => app.move_selection(false),
            KeyCode::Down | KeyCode::Char('j') => app.move_selection(true),
            KeyCode::Char(' ') => app.toggle_selected(),
            KeyCode::Enter => app.open_selected().await?,
            KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => app.go_parent().await?,
            KeyCode::Char('r') => app.reload().await?,
            KeyCode::Char('n') => app.load_more().await?,
            KeyCode::Char('/') => app.editing_search = true,
            KeyCode::Char('s') => app.change_sort().await?,
            KeyCode::Char('o') | KeyCode::Char('O') => {
                app.args.overwrite = !app.args.overwrite;
                app.status = format!(
                    "Overwrite {}",
                    if app.args.overwrite {
                        "enabled"
                    } else {
                        "disabled"
                    }
                );
            }
            KeyCode::Char('d') => app.start_download(),
            KeyCode::Char('c') => app.cancel_download(),
            _ => {}
        }
    }
    Ok(())
}

impl App {
    fn filtered_indices(&self) -> Vec<usize> {
        let needle = self.search.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| needle.is_empty() || entry.name.to_lowercase().contains(&needle))
            .map(|(index, _)| index)
            .collect()
    }

    fn selected_entry(&self) -> Option<&ListEntry> {
        let indices = self.filtered_indices();
        indices
            .get(self.selected)
            .and_then(|index| self.entries.get(*index))
    }

    fn move_selection(&mut self, down: bool) {
        let len = self.filtered_indices().len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        self.selected = if down {
            (self.selected + 1) % len
        } else {
            self.selected.checked_sub(1).unwrap_or(len - 1)
        };
        self.reset_name_scroll();
    }

    fn reset_name_scroll(&mut self) {
        self.selected_since = Instant::now();
    }

    fn toggle_selected(&mut self) {
        let Some(path) = self.selected_entry().map(|entry| entry.path.clone()) else {
            return;
        };
        if !self.marked.insert(path.clone()) {
            self.marked.remove(&path);
        }
        self.status = format!("{} item(s) selected", self.marked.len());
    }

    async fn open_selected(&mut self) -> io::Result<()> {
        let Some(entry) = self.selected_entry().cloned() else {
            return Ok(());
        };
        if entry.is_dir {
            self.current = entry.path;
            self.search.clear();
            self.reload().await?;
        } else {
            self.toggle_selected();
        }
        Ok(())
    }

    async fn go_parent(&mut self) -> io::Result<()> {
        if self.current.is_empty() {
            return Ok(());
        }
        self.current = self
            .current
            .rsplit_once('/')
            .map(|(parent, _)| parent.to_string())
            .unwrap_or_default();
        self.search.clear();
        self.reload().await
    }

    async fn reload(&mut self) -> io::Result<()> {
        let page = self.api.list_page(&self.current, 0, self.sort).await?;
        self.entries = page.entries;
        self.next_cursor = page.next_cursor;
        self.selected = 0;
        self.reset_name_scroll();
        self.status = format!("Loaded {} item(s)", self.entries.len());
        Ok(())
    }

    async fn load_more(&mut self) -> io::Result<()> {
        let Some(cursor) = self.next_cursor else {
            self.status = "No more items".to_string();
            return Ok(());
        };
        let page = self.api.list_page(&self.current, cursor, self.sort).await?;
        self.entries.extend(page.entries);
        self.next_cursor = page.next_cursor;
        self.status = format!("Loaded {} item(s)", self.entries.len());
        Ok(())
    }

    async fn change_sort(&mut self) -> io::Result<()> {
        self.sort = match self.sort {
            SortBy::Name => SortBy::Size,
            SortBy::Size => SortBy::Modified,
            SortBy::Modified => SortBy::Name,
        };
        self.reload().await
    }

    fn start_download(&mut self) {
        if self.download_task.is_some() {
            self.status = "A download is already active".to_string();
            return;
        }
        let roots = if self.marked.is_empty() {
            self.selected_entry()
                .map(|entry| vec![entry.path.clone()])
                .unwrap_or_default()
        } else {
            self.marked.iter().cloned().collect()
        };
        if roots.is_empty() {
            self.status = "Select at least one file or directory".to_string();
            return;
        }
        let api = self.api.clone();
        let args = self.args.clone();
        let tx = self.tx.clone();
        self.status = "Preparing download plan…".to_string();
        self.download_task = Some(tokio::spawn(async move {
            let result = async {
                let plan = expand_paths(&api, &roots).await?;
                preflight_collisions(&args.output, &plan)?;
                let progress_tx = tx.clone();
                let callback: ProgressCallback = Arc::new(move |progress| {
                    let _ = progress_tx.send(DownloadEvent::Progress(progress));
                });
                Ok::<_, io::Error>(download_plan(&api, &args, plan, callback).await)
            }
            .await;
            match result {
                Ok(summary) => {
                    let _ = tx.send(DownloadEvent::Done(summary));
                }
                Err(error) => {
                    let _ = tx.send(DownloadEvent::Error(error.to_string()));
                }
            }
        }));
    }

    fn cancel_download(&mut self) {
        if let Some(task) = self.download_task.take() {
            task.abort();
            self.status = "Download cancelled; verified partial data was kept".to_string();
            self.progress = None;
        }
    }

    fn collect_download_events(&mut self) {
        while let Ok(event) = self.rx.try_recv() {
            match event {
                DownloadEvent::Progress(progress) => {
                    self.status = format!("{:?}: {}", progress.state, progress.path);
                    self.progress = Some(progress);
                }
                DownloadEvent::Done(summary) => {
                    self.status = format!(
                        "Done: {} completed, {} skipped, {} failed",
                        summary.completed, summary.skipped, summary.failed
                    );
                    self.download_task = None;
                    self.progress = None;
                    self.marked.clear();
                }
                DownloadEvent::Error(error) => {
                    self.status = format!("Download failed: {error}");
                    self.download_task = None;
                    self.progress = None;
                }
            }
        }
    }
}

fn draw(frame: &mut Frame<'_>, app: &App) {
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(4),
            Constraint::Length(3),
        ])
        .split(frame.area());
    let title = Line::from(vec![
        Span::styled(
            " FILE TRANSFER ",
            Style::default()
                .bg(Color::Cyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  /{}", app.current)),
        Span::styled(
            format!(
                "  search: {}",
                if app.search.is_empty() {
                    "—"
                } else {
                    &app.search
                }
            ),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(title).block(Block::default().borders(Borders::ALL)),
        sections[0],
    );
    draw_entries(frame, sections[1], app);
    draw_progress(frame, sections[2], app);
    let help = if app.editing_search {
        "Type to search · Enter/Esc finish"
    } else if app.download_task.is_some() {
        "c cancel · q waits for cancellation"
    } else {
        "↑↓ move · Enter open · Space select · d download · / search · s sort · n more · O overwrite · q quit"
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(" {} ", app.status),
                Style::default().fg(Color::Green),
            ),
            Span::styled(help, Style::default().fg(Color::DarkGray)),
        ]))
        .block(Block::default().borders(Borders::ALL)),
        sections[3],
    );
    if app.editing_search {
        frame.set_cursor_position((
            sections[0].x + 28 + app.search.chars().count() as u16,
            sections[0].y + 1,
        ));
    }
}

fn draw_entries(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let indices = app.filtered_indices();
    let filename_width =
        usize::from(area.width.saturating_sub(53)).clamp(MIN_FILENAME_WIDTH, MAX_FILENAME_WIDTH);
    let rows = indices
        .iter()
        .enumerate()
        .map(|(position, index)| {
            let entry = &app.entries[*index];
            let marked = if app.marked.contains(&entry.path) {
                "[x]"
            } else {
                "[ ]"
            };
            let kind = if entry.is_dir { "DIR " } else { "FILE" };
            let partial = safe_local_path(&app.args.output, &entry.path)
                .map(|path| {
                    let mut value = path.as_os_str().to_os_string();
                    value.push(".utpart");
                    std::path::PathBuf::from(value).exists()
                })
                .unwrap_or(false);
            let displayed_name = if position == app.selected {
                scrolling_name(&entry.name, filename_width, app.selected_since.elapsed())
            } else {
                truncate_name(&entry.name, filename_width)
            };
            Row::new(vec![
                Cell::new(marked).style(Style::default().fg(Color::Cyan)),
                Cell::new(kind).style(Style::default().fg(Color::Cyan)),
                Cell::new(displayed_name).style(Style::default().add_modifier(Modifier::BOLD)),
                Cell::new(format!(
                    "{:>10}",
                    if entry.is_dir {
                        "—".to_string()
                    } else {
                        format_bytes(entry.size)
                    }
                )),
                Cell::new(format_modified(entry.modified_ms))
                    .style(Style::default().fg(Color::DarkGray)),
                Cell::new(if partial { "resumable" } else { "" }).style(Style::default().fg(
                    if partial {
                        Color::Yellow
                    } else {
                        Color::DarkGray
                    },
                )),
            ])
        })
        .collect::<Vec<_>>();
    let title = format!(
        " Files · sort:{:?} · selected:{}{} ",
        app.sort,
        app.marked.len(),
        if app.next_cursor.is_some() {
            " · more available (n)"
        } else {
            ""
        }
    );
    let header = Row::new(vec![
        Cell::new("SEL"),
        Cell::new("TYPE"),
        Cell::new("NAME"),
        Cell::new(format!("{:>10}", "SIZE")),
        Cell::new("MODIFIED"),
        Cell::new("STATUS"),
    ])
    .style(
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    );
    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Length(4),
            Constraint::Length(filename_width as u16),
            Constraint::Length(10),
            Constraint::Length(16),
            Constraint::Length(9),
        ],
    )
    .header(header)
    .column_spacing(1)
    .block(Block::default().title(title).borders(Borders::ALL))
    .highlight_symbol("▸ ")
    .row_highlight_style(Style::default().bg(Color::DarkGray).fg(Color::White));
    let mut state =
        TableState::default().with_selected((!indices.is_empty()).then_some(app.selected));
    frame.render_stateful_widget(table, area, &mut state);
}

fn truncate_name(name: &str, width: usize) -> String {
    if UnicodeWidthStr::width(name) <= width {
        return name.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let content_width = width.saturating_sub(1);
    let mut used = 0;
    let mut output = String::new();
    for character in name.chars() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > content_width {
            break;
        }
        output.push(character);
        used += character_width;
    }
    output.push('…');
    output
}

fn scrolling_name(name: &str, width: usize, elapsed: Duration) -> String {
    if UnicodeWidthStr::width(name) <= width || elapsed < MARQUEE_PAUSE {
        return truncate_name(name, width);
    }
    let characters = format!("{name}   ").chars().collect::<Vec<_>>();
    let offset = ((elapsed - MARQUEE_PAUSE).as_millis() / MARQUEE_STEP.as_millis()) as usize
        % characters.len();
    let mut output = String::new();
    let mut used = 0;
    for character in characters.iter().cycle().skip(offset) {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > width {
            break;
        }
        output.push(*character);
        used += character_width;
        if used == width {
            break;
        }
    }
    output
}

fn format_modified(modified_ms: u64) -> String {
    UNIX_EPOCH
        .checked_add(Duration::from_millis(modified_ms))
        .map(DateTime::<Local>::from)
        .map(|timestamp| timestamp.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "unknown time".to_string())
}

fn draw_progress(frame: &mut Frame<'_>, area: Rect, app: &App) {
    let (ratio, label) = match &app.progress {
        Some(progress) => {
            let ratio = if progress.total == 0 {
                0.0
            } else {
                progress.transferred as f64 / progress.total as f64
            };
            let eta = format_eta(progress.eta);
            (
                ratio,
                format!(
                    "[{}/{}] {} · {} / {} · {}/s · {} · overall {} / {} · {:?}",
                    progress.file_index,
                    progress.file_count,
                    progress.path,
                    format_bytes(progress.transferred),
                    format_bytes(progress.total),
                    format_bytes(progress.bytes_per_second),
                    eta,
                    format_bytes(progress.batch_transferred),
                    format_bytes(progress.batch_total),
                    progress.state
                ),
            )
        }
        None => (
            0.0,
            format!("Download directory: {}", app.args.output.display()),
        ),
    };
    frame.render_widget(
        Gauge::default()
            .block(
                Block::default()
                    .title(" Transfer status ")
                    .borders(Borders::ALL),
            )
            .gauge_style(
                Style::default().fg(match app.progress.as_ref().map(|p| p.state) {
                    Some(ProgressState::Failed) => Color::Red,
                    Some(ProgressState::Complete) => Color::Green,
                    _ => Color::Cyan,
                }),
            )
            .ratio(ratio.clamp(0.0, 1.0))
            .label(label),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_names_to_terminal_width() {
        let name = truncate_name("abcdefghijkl", 8);
        assert_eq!(name, "abcdefg…");
        assert!(UnicodeWidthStr::width(name.as_str()) <= 8);

        let wide_name = truncate_name("一二三四五", 6);
        assert!(wide_name.ends_with('…'));
        assert!(UnicodeWidthStr::width(wide_name.as_str()) <= 6);
    }

    #[test]
    fn selected_long_name_scrolls_after_pause() {
        let initial = scrolling_name("abcdefghijkl", 8, Duration::ZERO);
        let scrolled = scrolling_name("abcdefghijkl", 8, MARQUEE_PAUSE + MARQUEE_STEP * 2);
        assert_ne!(initial, scrolled);
        assert!(UnicodeWidthStr::width(scrolled.as_str()) <= 8);
    }
}
