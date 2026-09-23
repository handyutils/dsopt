//! The `dsopt` terminal user interface: scan, select, and reclaim.

use crate::{
    Candidate, ProgressSnapshot, ScanState, THREAD_RANGE, cache_path, format_size,
    is_safe_candidate, load_scan_cache, remove_candidate, save_scan_cache, scan_with_state,
};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Cell, Clear, Gauge, List, ListItem, Paragraph, Row, Table,
        TableState, Wrap,
    },
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::Duration,
};

/// How often the scanning thread publishes progress while a walk is running.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);
/// Event-loop tick; also bounds how long a key press can wait behind a redraw.
const TICK: Duration = Duration::from_millis(120);

/// Messages sent from the background scan thread.
enum ScanEvent {
    Progress(Box<ProgressSnapshot>),
    Finished(Vec<Candidate>),
    Failed(String),
}

/// Messages sent from the background removal thread.
enum RemovalEvent {
    Step {
        index: usize,
        total: usize,
        candidate: Candidate,
        error: Option<String>,
    },
    Finished {
        deleted: usize,
        reclaimed: u64,
        errors: usize,
    },
}

/// The single overlay that can be on screen at a time.
enum Modal {
    Confirm {
        count: usize,
        bytes: u64,
    },
    Removal {
        lines: Vec<Line<'static>>,
        done: usize,
        total: usize,
        reclaimed: u64,
        errors: usize,
        finished: bool,
    },
    Help,
}

/// Which widget currently receives plain character keys.
#[derive(PartialEq, Eq)]
enum Focus {
    Table,
    ThreadsInput,
}

/// Application state for the whole session.
pub struct App {
    roots: Vec<PathBuf>,
    threads: usize,
    threads_input: String,
    candidates: Vec<Candidate>,
    selected: HashSet<PathBuf>,
    table: TableState,
    table_height: usize,
    status: String,
    scanning: bool,
    scan_rx: Option<Receiver<ScanEvent>>,
    cancel: Arc<AtomicBool>,
    show_settings: bool,
    focus: Focus,
    progress_done: u64,
    progress_total: u64,
    stats: Option<ProgressSnapshot>,
    modal: Option<Modal>,
    removal_rx: Option<Receiver<RemovalEvent>>,
    should_quit: bool,
    cache: PathBuf,
}

impl App {
    pub fn new(roots: Vec<PathBuf>, threads: usize) -> Self {
        Self {
            roots,
            threads,
            threads_input: threads.to_string(),
            candidates: Vec::new(),
            selected: HashSet::new(),
            table: TableState::default(),
            table_height: 10,
            status: "Starting scan...".to_owned(),
            scanning: false,
            scan_rx: None,
            cancel: Arc::new(AtomicBool::new(false)),
            show_settings: false,
            focus: Focus::Table,
            progress_done: 0,
            progress_total: 1,
            stats: None,
            modal: None,
            removal_rx: None,
            should_quit: false,
            cache: cache_path(),
        }
    }

    /// Blocking event loop: draw, drain background channels, handle one key.
    pub fn run(&mut self, terminal: &mut ratatui::DefaultTerminal) -> std::io::Result<()> {
        self.restore_or_start_scan();
        while !self.should_quit {
            terminal.draw(|frame| self.draw(frame))?;
            self.drain_background();
            if self.should_quit {
                break;
            }
            if crossterm::event::poll(TICK)? {
                match crossterm::event::read()? {
                    crossterm::event::Event::Key(key) if key.kind == KeyEventKind::Press => {
                        self.on_key(key);
                    }
                    _ => {}
                }
            }
        }
        self.cancel.store(true, Ordering::Relaxed);
        Ok(())
    }

    /// Show the cached scan if there is one, otherwise begin scanning immediately.
    fn restore_or_start_scan(&mut self) {
        let Some(cache) = load_scan_cache(&self.cache) else {
            self.start_scan();
            return;
        };
        self.candidates = cache.candidates;
        self.selected.clear();
        self.progress_done = 1;
        self.progress_total = 1;
        let total = format_size(self.candidates.iter().map(|c| c.size_bytes).sum());
        self.status = format!(
            "Loaded {} candidates totaling {total} from the last scan ({}) · press R to rescan",
            self.candidates.len(),
            cache.scanned_at
        );
        self.sync_table();
    }

    fn sync_table(&mut self) {
        if self.candidates.is_empty() {
            self.table.select(None);
        } else {
            let current = self.table.selected().unwrap_or(0);
            self.table
                .select(Some(current.min(self.candidates.len() - 1)));
        }
        self.clamp_offset();
    }

    // ---------------------------------------------------------------- scanning

    fn start_scan(&mut self) {
        if self.scanning {
            return;
        }
        if self.show_settings {
            match self.threads_input.trim().parse::<usize>() {
                Ok(value) if THREAD_RANGE.contains(&value) => self.threads = value,
                Ok(_) => {
                    self.status = format!(
                        "Thread count must be between {} and {}.",
                        THREAD_RANGE.start(),
                        THREAD_RANGE.end()
                    );
                    return;
                }
                Err(_) => {
                    self.status = "Thread count must be a number from 1 through 8.".to_owned();
                    return;
                }
            }
        }
        self.scanning = true;
        self.cancel.store(false, Ordering::Relaxed);
        self.selected.clear();
        self.candidates.clear();
        self.stats = None;
        self.progress_done = 0;
        self.progress_total = 1;
        self.sync_table();
        self.status = format!("Scanning with {} threads...", self.threads);

        let (tx, rx) = mpsc::channel();
        self.scan_rx = Some(rx);
        let roots = self.roots.clone();
        let threads = self.threads;
        let cancel = Arc::clone(&self.cancel);
        thread::spawn(move || run_scan(roots, threads, cancel, tx));
    }

    fn drain_background(&mut self) {
        let scan_events: Vec<ScanEvent> = self
            .scan_rx
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default();
        let mut finished_scan = false;
        for event in scan_events {
            match event {
                ScanEvent::Progress(snapshot) => self.stats = Some(*snapshot),
                ScanEvent::Finished(candidates) => {
                    self.candidates = candidates;
                    finished_scan = true;
                }
                ScanEvent::Failed(error) => {
                    self.scanning = false;
                    self.status = format!("Scan failed: {error}");
                    finished_scan = true;
                }
            }
        }
        if finished_scan {
            self.scan_rx = None;
            if self.scanning {
                self.scanning = false;
                self.sync_table();
                let total = format_size(self.candidates.iter().map(|c| c.size_bytes).sum());
                self.status = format!(
                    "Found {} candidates totaling {total}. Saved as last scan.",
                    self.candidates.len()
                );
                let _ = save_scan_cache(&self.cache, &self.candidates, &self.roots, self.threads);
            }
            self.progress_done = 1;
            self.progress_total = 1;
        }

        let removal_events: Vec<RemovalEvent> = self
            .removal_rx
            .as_ref()
            .map(|rx| rx.try_iter().collect())
            .unwrap_or_default();
        let mut removal_finished = false;
        for event in removal_events {
            match event {
                RemovalEvent::Step {
                    index,
                    total,
                    candidate,
                    error,
                } => {
                    if let Some(Modal::Removal {
                        lines,
                        done,
                        total: modal_total,
                        ..
                    }) = self.modal.as_mut()
                    {
                        *done = index;
                        *modal_total = total;
                        lines.push(removal_line(index, total, &candidate, error.as_deref()));
                    }
                }
                RemovalEvent::Finished {
                    deleted,
                    reclaimed,
                    errors,
                } => {
                    self.candidates.retain(|candidate| candidate.path.exists());
                    self.selected.clear();
                    self.sync_table();
                    let _ =
                        save_scan_cache(&self.cache, &self.candidates, &self.roots, self.threads);
                    self.status = format!(
                        "Removed {deleted} directories and reclaimed {}.{}",
                        format_size(reclaimed),
                        if errors == 0 {
                            String::new()
                        } else {
                            format!(" Errors: {errors}")
                        }
                    );
                    if let Some(Modal::Removal {
                        reclaimed: modal_reclaimed,
                        errors: modal_errors,
                        finished,
                        ..
                    }) = self.modal.as_mut()
                    {
                        *modal_reclaimed = reclaimed;
                        *modal_errors = errors;
                        *finished = true;
                    }
                    removal_finished = true;
                }
            }
        }
        if removal_finished {
            self.removal_rx = None;
        }
    }

    // ---------------------------------------------------------------- removal

    fn begin_removal(&mut self) {
        let targets: Vec<Candidate> = self
            .candidates
            .iter()
            .filter(|candidate| self.selected.contains(&candidate.path))
            .cloned()
            .collect();
        if targets.is_empty() {
            self.status = "Nothing selected. Use space or A to select directories.".to_owned();
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.removal_rx = Some(rx);
        self.modal = Some(Modal::Removal {
            lines: Vec::new(),
            done: 0,
            total: targets.len(),
            reclaimed: 0,
            errors: 0,
            finished: false,
        });
        self.status = format!("Removing {} directories...", targets.len());
        thread::spawn(move || run_removal(targets, tx));
    }

    // ------------------------------------------------------------------ input

    fn on_key(&mut self, key: KeyEvent) {
        if self.modal.is_some() {
            self.on_modal_key(key);
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') if ctrl => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Char('r') if ctrl => self.start_scan(),
            KeyCode::Char('t') if ctrl => self.toggle_settings(),
            KeyCode::Char('a') if ctrl => self.select_all(),
            KeyCode::Char('n') if ctrl => self.select_none(),
            KeyCode::Char('d') if ctrl => self.confirm_removal(),
            KeyCode::Char('?') => self.modal = Some(Modal::Help),
            KeyCode::Esc if self.show_settings => self.toggle_settings(),
            _ if self.focus == Focus::ThreadsInput => self.on_input_key(key),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-(self.table_height as isize)),
            KeyCode::PageDown => self.move_selection(self.table_height as isize),
            KeyCode::Home => self.select_row(0),
            KeyCode::End => self.select_row(self.candidates.len().saturating_sub(1)),
            KeyCode::Char(' ') => self.toggle_selected(),
            KeyCode::Char('a') | KeyCode::Char('A') => self.select_all(),
            KeyCode::Char('n') | KeyCode::Char('N') => self.select_none(),
            KeyCode::Char('r') | KeyCode::Char('R') => self.start_scan(),
            KeyCode::Char('t') | KeyCode::Char('T') => self.toggle_settings(),
            KeyCode::Char('d') | KeyCode::Char('D') => self.confirm_removal(),
            _ => {}
        }
    }

    fn on_modal_key(&mut self, key: KeyEvent) {
        match self.modal {
            Some(Modal::Confirm { .. }) => match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.modal = None;
                    self.begin_removal();
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') => {
                    self.modal = None;
                }
                _ => {}
            },
            Some(Modal::Removal { finished: true, .. })
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('q')
                ) =>
            {
                self.modal = None;
            }
            Some(Modal::Removal { .. }) => {}
            Some(Modal::Help) => self.modal = None,
            None => {}
        }
    }

    fn on_input_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(character) if character.is_ascii_digit() => {
                if self.threads_input.is_empty() {
                    self.threads_input.push(character);
                } else {
                    self.threads_input = character.to_string();
                }
            }
            KeyCode::Backspace => {
                self.threads_input.pop();
            }
            KeyCode::Enter | KeyCode::Tab => self.toggle_settings(),
            _ => {}
        }
    }

    fn toggle_settings(&mut self) {
        self.show_settings = !self.show_settings;
        self.focus = if self.show_settings {
            self.threads_input = self.threads.to_string();
            Focus::ThreadsInput
        } else {
            Focus::Table
        };
    }

    fn confirm_removal(&mut self) {
        let targets: Vec<&Candidate> = self
            .candidates
            .iter()
            .filter(|candidate| self.selected.contains(&candidate.path))
            .collect();
        if targets.is_empty() {
            self.status = "Nothing selected. Use space or A to select directories.".to_owned();
            return;
        }
        self.modal = Some(Modal::Confirm {
            count: targets.len(),
            bytes: targets.iter().map(|candidate| candidate.size_bytes).sum(),
        });
    }

    fn move_selection(&mut self, delta: isize) {
        if self.candidates.is_empty() {
            return;
        }
        let last = self.candidates.len() as isize - 1;
        let current = self.table.selected().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, last) as usize;
        self.table.select(Some(next));
        self.clamp_offset();
    }

    fn select_row(&mut self, index: usize) {
        if self.candidates.is_empty() {
            return;
        }
        self.table
            .select(Some(index.min(self.candidates.len() - 1)));
        self.clamp_offset();
    }

    /// Keep the highlighted row inside the rendered viewport.
    fn clamp_offset(&mut self) {
        let height = self.table_height.max(1);
        let selected = self.table.selected().unwrap_or(0);
        let offset = *self.table.offset_mut();
        let updated = if selected < offset {
            selected
        } else if selected >= offset + height {
            selected + 1 - height
        } else {
            offset
        };
        *self.table.offset_mut() = updated;
    }

    fn toggle_selected(&mut self) {
        let Some(index) = self.table.selected() else {
            return;
        };
        let Some(candidate) = self.candidates.get(index) else {
            return;
        };
        let path = candidate.path.clone();
        if !self.selected.remove(&path) {
            self.selected.insert(path);
        }
    }

    fn select_all(&mut self) {
        self.selected = self
            .candidates
            .iter()
            .map(|candidate| candidate.path.clone())
            .collect();
    }

    fn select_none(&mut self) {
        self.selected.clear();
    }

    // ----------------------------------------------------------------- render

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let settings_height = if self.show_settings { 3 } else { 0 };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(3),
                Constraint::Length(settings_height),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);

        self.draw_intro(frame, chunks[0]);
        self.draw_actions(frame, chunks[1]);
        if self.show_settings {
            self.draw_settings(frame, chunks[2]);
        }
        self.draw_status(frame, chunks[3]);
        self.draw_progress(frame, chunks[4]);
        self.draw_selection_bar(frame, chunks[5]);
        self.draw_table(frame, chunks[6]);
        self.draw_help(frame, chunks[7]);

        if let Some(modal) = self.modal.as_ref() {
            match modal {
                Modal::Confirm { count, bytes } => {
                    draw_confirm(frame, area, *count, *bytes);
                }
                Modal::Removal {
                    lines,
                    done,
                    total,
                    reclaimed,
                    errors,
                    finished,
                } => {
                    draw_removal(
                        frame, area, lines, *done, *total, *reclaimed, *errors, *finished,
                    );
                }
                Modal::Help => draw_help(frame, area),
            }
        }
    }

    fn draw_intro(&self, frame: &mut Frame, area: Rect) {
        let text = vec![
            Line::from(vec![
                Span::styled(
                    format!("dsopt v{}", env!("CARGO_PKG_VERSION")),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  ·  find and reclaim build artifacts"),
            ]),
            Line::from(Span::styled(
                "Scans for JavaScript node_modules and Rust target/ directories, then removes the ones you pick.",
                Style::default().fg(Color::DarkGray),
            )),
        ];
        frame.render_widget(
            Paragraph::new(text).block(Block::default().borders(Borders::ALL)),
            area,
        );
    }

    fn draw_actions(&self, frame: &mut Frame, area: Rect) {
        let action = |label: &'static str| {
            Span::styled(
                format!(" {label} "),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
        };
        let line = Line::from(vec![
            action("R  Rescan"),
            Span::raw("  "),
            action(if self.show_settings {
                "T  Hide settings"
            } else {
                "T  Scan settings"
            }),
            Span::raw("  "),
            Span::styled(
                if self.scanning { "scanning…" } else { "idle" },
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        frame.render_widget(
            Paragraph::new(line).block(Block::default().borders(Borders::ALL)),
            area,
        );
    }

    fn draw_settings(&self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::ThreadsInput;
        let line = Line::from(vec![
            Span::raw("Scanner threads (1-8):  "),
            Span::styled(
                format!(" {} ", self.threads_input),
                if focused {
                    Style::default().fg(Color::Black).bg(Color::Yellow)
                } else {
                    Style::default().fg(Color::Yellow)
                },
            ),
            Span::raw("   type a digit · Enter or Esc to close"),
        ]);
        frame.render_widget(
            Paragraph::new(line).block(Block::default().borders(Borders::ALL)),
            area,
        );
    }

    fn draw_status(&self, frame: &mut Frame, area: Rect) {
        let text = match (&self.stats, self.scanning) {
            (Some(stats), true) => format!(
                "{} threads | {} folders | {} files | {} | {}",
                self.threads,
                thousands(stats.directories_scanned),
                thousands(stats.files_scanned),
                format_size(stats.bytes_scanned),
                stats.current_path
            ),
            _ => self.status.clone(),
        };
        frame.render_widget(
            Paragraph::new(Line::from(text)).style(Style::default().fg(Color::Gray)),
            area,
        );
    }

    fn draw_progress(&self, frame: &mut Frame, area: Rect) {
        let ratio = if self.progress_total == 0 {
            0.0
        } else {
            (self.progress_done as f64 / self.progress_total as f64).clamp(0.0, 1.0)
        };
        let gauge = Gauge::default()
            .gauge_style(Style::default().fg(if self.scanning {
                Color::Cyan
            } else {
                Color::Green
            }))
            .ratio(ratio);
        frame.render_widget(gauge, area);
    }

    fn draw_selection_bar(&self, frame: &mut Frame, area: Rect) {
        let count = self.selected.len();
        let bytes: u64 = self
            .candidates
            .iter()
            .filter(|candidate| self.selected.contains(&candidate.path))
            .map(|candidate| candidate.size_bytes)
            .sum();
        let summary = if count == 0 {
            "Selected: none · Space toggle · A all · N none".to_owned()
        } else {
            format!(
                "Selected: {count} directories · {} reclaimable · D to remove",
                format_size(bytes)
            )
        };
        let hint = Span::styled(
            format!("  remove: {count} · {}", format_size(bytes)),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::raw(summary), hint]))
                .block(Block::default().borders(Borders::ALL))
                .alignment(Alignment::Left),
            area,
        );
    }

    fn draw_table(&mut self, frame: &mut Frame, area: Rect) {
        let rows = self.candidates.iter().map(|candidate| {
            let marker = if self.selected.contains(&candidate.path) {
                "✓"
            } else {
                " "
            };
            Row::new(vec![
                Cell::from(marker.to_owned()).style(Style::default().fg(Color::Green)),
                Cell::from(candidate.kind.label()),
                Cell::from(format_size(candidate.size_bytes)),
                Cell::from(candidate.path.display().to_string()),
            ])
        });
        let header = Row::new(vec!["", "Type", "Size", "Path"]).style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        );
        let table = Table::new(
            rows,
            [
                Constraint::Length(3),
                Constraint::Length(24),
                Constraint::Length(12),
                Constraint::Min(20),
            ],
        )
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Plain)
                .title(format!(" Candidates ({}) ", self.candidates.len())),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::Indexed(236))
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
        frame.render_stateful_widget(table, area, &mut self.table);
        self.table_height = area.height.saturating_sub(3) as usize;
        if self.table_height == 0 {
            self.table_height = 1;
        }
    }

    fn draw_help(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "Space select · A all · N none · D remove · R rescan · T settings · ? help · Q quit",
                Style::default().fg(Color::DarkGray),
            ))),
            area,
        );
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

// ------------------------------------------------------------------ workers

fn run_scan(roots: Vec<PathBuf>, threads: usize, cancel: Arc<AtomicBool>, tx: Sender<ScanEvent>) {
    let state = Arc::new(ScanState::default());
    let reporter_state = Arc::clone(&state);
    let reporter_done = Arc::new(AtomicBool::new(false));
    let reporter_flag = Arc::clone(&reporter_done);
    let reporter_tx = tx.clone();
    let reporter = thread::spawn(move || {
        while !reporter_flag.load(Ordering::Relaxed) {
            let snapshot = reporter_state.snapshot();
            if reporter_tx
                .send(ScanEvent::Progress(Box::new(snapshot)))
                .is_err()
            {
                break;
            }
            thread::sleep(PROGRESS_INTERVAL);
        }
    });

    let mut collected = Vec::new();
    let mut failure = None;
    for root in &roots {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        match scan_with_state(root, threads, &state) {
            Ok(result) => collected.extend(result.candidates),
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }
    reporter_done.store(true, Ordering::Relaxed);
    let _ = reporter.join();

    if let Some(error) = failure {
        let _ = tx.send(ScanEvent::Failed(error));
        return;
    }
    collected.sort_by(|left, right| {
        right
            .size_bytes
            .cmp(&left.size_bytes)
            .then_with(|| left.path.cmp(&right.path))
    });
    collected.dedup_by(|left, right| left.path == right.path);
    let _ = tx.send(ScanEvent::Finished(collected));
}

fn run_removal(targets: Vec<Candidate>, tx: Sender<RemovalEvent>) {
    let total = targets.len();
    let mut deleted = 0;
    let mut reclaimed = 0;
    let mut errors = 0;
    for (offset, candidate) in targets.iter().enumerate() {
        let error = match remove_candidate(candidate) {
            Ok(()) => {
                deleted += 1;
                reclaimed += candidate.size_bytes;
                None
            }
            Err(message) => {
                errors += 1;
                Some(message)
            }
        };
        if tx
            .send(RemovalEvent::Step {
                index: offset + 1,
                total,
                candidate: candidate.clone(),
                error,
            })
            .is_err()
        {
            return;
        }
    }
    let _ = tx.send(RemovalEvent::Finished {
        deleted,
        reclaimed,
        errors,
    });
}

// ------------------------------------------------------------------ widgets

fn removal_line(
    index: usize,
    total: usize,
    candidate: &Candidate,
    error: Option<&str>,
) -> Line<'static> {
    let path = candidate.path.display().to_string();
    match error {
        Some(message) => Line::from(vec![
            Span::styled("✗  ", Style::default().fg(Color::Red)),
            Span::raw(format!("{index}/{total}  ")),
            Span::raw(path),
            Span::styled(format!("  {message}"), Style::default().fg(Color::Red)),
        ]),
        None => Line::from(vec![
            Span::styled("✓  ", Style::default().fg(Color::Green)),
            Span::raw(format!("{index}/{total}  ")),
            Span::styled(
                format!("{}  ", candidate.kind.label()),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw(path),
        ]),
    }
}

fn draw_confirm(frame: &mut Frame, area: Rect, count: usize, bytes: u64) {
    let dialog = centered_rect(74, 7, area);
    frame.render_widget(Clear, dialog);
    let body = vec![
        Line::from(Span::styled(
            format!(
                "Remove {count} selected directories and free {}?",
                format_size(bytes)
            ),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "This permanently deletes them. There is no Trash and no undo.",
            Style::default().fg(Color::Red),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled(
                " Y / Enter ",
                Style::default().fg(Color::Black).bg(Color::Red),
            ),
            Span::raw("  remove permanently      "),
            Span::styled(
                " N / Esc ",
                Style::default().fg(Color::Black).bg(Color::Gray),
            ),
            Span::raw("  cancel"),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(body)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Thick)
                    .border_style(Style::default().fg(Color::Red))
                    .title(" Confirm removal "),
            )
            .wrap(Wrap { trim: true }),
        dialog,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_removal(
    frame: &mut Frame,
    area: Rect,
    lines: &[Line<'static>],
    done: usize,
    total: usize,
    reclaimed: u64,
    errors: usize,
    finished: bool,
) {
    let dialog = centered_rect(100, 24, area);
    frame.render_widget(Clear, dialog);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(4),
            Constraint::Length(2),
        ])
        .split(dialog.inner(Margin {
            horizontal: 2,
            vertical: 1,
        }));

    let headline = if finished {
        format!(
            "Removal complete · {} reclaimed{}",
            format_size(reclaimed),
            if errors == 0 {
                String::new()
            } else {
                format!(" · {errors} errors")
            }
        )
    } else {
        format!(
            "Removing {total} directories · {} selected",
            format_size(reclaimed)
        )
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            headline,
            Style::default().add_modifier(Modifier::BOLD),
        ))),
        chunks[0],
    );
    let ratio = if total == 0 {
        0.0
    } else {
        (done as f64 / total as f64).clamp(0.0, 1.0)
    };
    frame.render_widget(
        Gauge::default()
            .gauge_style(Style::default().fg(Color::Red))
            .ratio(ratio),
        chunks[1],
    );

    let visible = chunks[2].height as usize;
    let tail: Vec<ListItem> = lines
        .iter()
        .rev()
        .take(visible.max(1))
        .rev()
        .map(|line| ListItem::new(line.clone()))
        .collect();
    frame.render_widget(
        List::new(tail).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded),
        ),
        chunks[2],
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            if finished {
                " Press Esc or Enter to close "
            } else {
                " working… "
            },
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[3],
    );

    let frame_block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Thick)
        .border_style(Style::default().fg(Color::Red))
        .title(" Removal progress ");
    frame.render_widget(frame_block, dialog);
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let dialog = centered_rect(72, 14, area);
    frame.render_widget(Clear, dialog);
    let body = vec![
        Line::from(Span::styled(
            "Keyboard shortcuts",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::raw("  ↑ / ↓, j / k, PgUp / PgDn, Home / End   move"),
        Line::raw("  Space                                    select or unselect"),
        Line::raw("  A / N                                    select all / none"),
        Line::raw("  D                                        remove selected"),
        Line::raw("  R                                        rescan the filesystem"),
        Line::raw("  T                                        scanner thread settings"),
        Line::raw("  Q or Ctrl+C                              quit"),
        Line::raw(""),
        Line::from(Span::styled(
            "Press any key to close",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(
        Paragraph::new(body).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Thick)
                .border_style(Style::default().fg(Color::Cyan))
                .title(" Help "),
        ),
        dialog,
    );
}

/// A `width` x `height` rect centred inside `area`, clamped to fit.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Group digits with commas, e.g. `1234567` -> `1,234,567`.
fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(character);
    }
    grouped
}

/// Check a path against the same safety rules the removal worker applies.
pub fn candidate_is_safe(path: &Path) -> bool {
    is_safe_candidate(path, Path::new("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_digits_with_commas() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn centres_a_dialog_inside_the_frame() {
        let area = Rect::new(0, 0, 100, 40);
        let dialog = centered_rect(74, 7, area);
        assert_eq!(dialog.width, 74);
        assert_eq!(dialog.x, 13);
        assert_eq!(dialog.y, 16);
    }

    #[test]
    fn never_centres_a_dialog_larger_than_the_frame() {
        let dialog = centered_rect(200, 100, Rect::new(0, 0, 40, 10));
        assert_eq!((dialog.width, dialog.height), (40, 10));
    }
}
