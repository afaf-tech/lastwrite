//! Interactive dashboard. Runs only when stdout is a terminal; the plain
//! report stays available for scripts through `lastwrite report`.

use std::io;
use std::io::IsTerminal;
use std::panic;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{Frame, Terminal};

use crate::score::Tier;
use crate::store::{Snapshot, Store};
use crate::{advice, poll, Poll, Reading};

const HISTORY_POINTS: i64 = 32;
const TREND_POINTS: usize = 12;
const REFRESH_TICK: Duration = Duration::from_millis(250);

enum Command {
    Refresh,
    Quit,
}

enum Update {
    Done(Vec<DiskView>),
    Failed(String),
}

struct DiskView {
    reading: Reading,
    history: Vec<Snapshot>,
}

impl DiskView {
    fn trend(&self) -> String {
        let scores: Vec<f64> = self.history.iter().filter_map(|snapshot| snapshot.score).collect();
        sparkline(&scores)
    }
}

struct App {
    disks: Vec<DiskView>,
    selected: usize,
    status: String,
    refreshing: bool,
    last_refresh: Option<Instant>,
    quit: bool,
}

impl App {
    fn new() -> App {
        App {
            disks: Vec::new(),
            selected: 0,
            status: String::new(),
            refreshing: false,
            last_refresh: None,
            quit: false,
        }
    }

    fn refresh(&mut self, commands: &Sender<Command>) {
        if self.refreshing {
            return;
        }
        if commands.send(Command::Refresh).is_err() {
            self.status = "refresh worker stopped".to_string();
            return;
        }
        self.refreshing = true;
        self.status.clear();
    }

    fn apply(&mut self, update: Update) {
        match update {
            Update::Done(mut disks) => {
                sort_views(&mut disks);
                self.disks = disks;
                self.selected = self.selected.min(self.disks.len().saturating_sub(1));
                self.refreshing = false;
                self.status.clear();
                self.last_refresh = Some(Instant::now());
            }
            Update::Failed(message) => {
                self.refreshing = false;
                self.status = message;
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent, commands: &Sender<Command>) {
        let len = self.disks.len();
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.quit = true
            }
            KeyCode::Char('r') => self.refresh(commands),
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = move_selection(self.selected, len, -1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = move_selection(self.selected, len, 1)
            }
            KeyCode::Home | KeyCode::Char('g') => self.selected = 0,
            KeyCode::End | KeyCode::Char('G') => self.selected = len.saturating_sub(1),
            _ => {}
        }
    }
}

pub fn run(path: PathBuf) -> Result<(), String> {
    if !io::stdout().is_terminal() {
        return Err("tui needs an interactive terminal".to_string());
    }

    let (command_tx, command_rx) = mpsc::channel();
    let (update_tx, update_rx) = mpsc::channel();
    thread::spawn(move || worker(path, command_rx, update_tx));

    install_panic_hook();
    enable_raw_mode().map_err(|e| e.to_string())?;
    let _restore = Restore;
    execute!(io::stdout(), EnterAlternateScreen, Hide).map_err(|e| e.to_string())?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))
        .map_err(|e| e.to_string())?;

    let mut app = App::new();
    app.refresh(&command_tx);
    while !app.quit {
        while let Ok(update) = update_rx.try_recv() {
            app.apply(update);
        }
        terminal.draw(|frame| render(frame, &app)).map_err(|e| e.to_string())?;
        if event::poll(REFRESH_TICK).map_err(|e| e.to_string())? {
            if let Event::Key(key) = event::read().map_err(|e| e.to_string())? {
                if key.kind == KeyEventKind::Press {
                    app.handle_key(key, &command_tx);
                }
            }
        }
    }
    let _ = command_tx.send(Command::Quit);
    Ok(())
}

fn worker(path: PathBuf, commands: Receiver<Command>, updates: Sender<Update>) {
    let mut store = match Store::open(&path) {
        Ok(store) => store,
        Err(message) => {
            let _ = updates.send(Update::Failed(message));
            return;
        }
    };
    while let Ok(command) = commands.recv() {
        match command {
            Command::Quit => break,
            Command::Refresh => {
                if updates.send(refresh(&mut store)).is_err() {
                    break;
                }
            }
        }
    }
}

fn refresh(store: &mut Store) -> Update {
    let Poll { readings, .. } = match poll(store) {
        Ok(poll) => poll,
        Err(message) => return Update::Failed(message),
    };
    let mut views = Vec::with_capacity(readings.len());
    for reading in readings {
        match store.history(reading.disk_id, HISTORY_POINTS) {
            Ok(history) => views.push(DiskView { reading, history }),
            Err(error) => return Update::Failed(error.to_string()),
        }
    }
    Update::Done(views)
}

fn install_panic_hook() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
}

struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
}

fn render(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(frame.area());
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
        .split(chunks[1]);

    render_header(frame, app, chunks[0]);
    render_drives(frame, app, body[0]);
    render_details(frame, app, body[1]);
    render_footer(frame, app, chunks[2]);
}

fn render_header(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let line = Line::from(vec![
        Span::styled(
            " Lastwrite ",
            Style::default().fg(Color::Black).bg(Color::Green).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" {} drive(s)   ", app.disks.len())),
        Span::styled(state_text(app), Style::default().fg(Color::DarkGray)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_drives(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let header = Row::new(vec!["Model", "Type", "Score", "Tier", "Temp", "Trend"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let rows = app.disks.iter().map(|view| {
        let disk = &view.reading.disk;
        let assessment = &view.reading.assessment;
        Row::new(vec![
            Cell::from(disk.model.clone()),
            Cell::from(disk.kind.label()),
            Cell::from(score_text(assessment.score)),
            Cell::from(Span::styled(assessment.tier.label(), tier_style(assessment.tier))),
            Cell::from(temperature_text(disk.temperature_c)),
            Cell::from(view.trend()),
        ])
    });
    let widths = [
        Constraint::Min(16),
        Constraint::Length(8),
        Constraint::Length(6),
        Constraint::Length(9),
        Constraint::Length(5),
        Constraint::Length(TREND_POINTS as u16),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" Drives "))
        .row_highlight_style(Style::default().add_modifier(Modifier::BOLD).bg(Color::DarkGray))
        .highlight_symbol("> ");
    let mut state = TableState::default();
    if !app.disks.is_empty() {
        state.select(Some(app.selected));
    }
    frame.render_stateful_widget(table, area, &mut state);
}

fn render_details(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let block = Block::default().borders(Borders::ALL).title(" Details ");
    let Some(view) = app.disks.get(app.selected) else {
        let text = if app.disks.is_empty() {
            "No readable drives. Press r to refresh."
        } else {
            "No drive selected."
        };
        frame.render_widget(Paragraph::new(text).block(block).wrap(Wrap { trim: true }), area);
        return;
    };

    let disk = &view.reading.disk;
    let assessment = &view.reading.assessment;
    let mut lines = vec![
        Line::from(Span::styled(
            format!("{} ({})", disk.model, disk.kind.label()),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            Span::raw("Score: "),
            Span::styled(score_text(assessment.score), tier_style(assessment.tier)),
            Span::raw("   "),
            Span::styled(assessment.tier.label(), tier_style(assessment.tier)),
        ]),
        Line::from(format!("Serial: {}", disk.serial)),
        Line::from(format!("Device: {}", disk.device)),
    ];
    if let Some(firmware) = &disk.firmware {
        lines.push(Line::from(format!("Firmware: {firmware}")));
    }
    if let Some(capacity) = disk.capacity_bytes {
        lines.push(Line::from(format!("Capacity: {}", human_bytes(capacity))));
    }
    if let Some(temperature) = disk.temperature_c {
        lines.push(Line::from(format!("Temperature: {temperature}C")));
    }
    let trend = view.trend();
    if !trend.is_empty() {
        lines.push(Line::from(format!("Trend: {trend}")));
    }
    if let Some(last) = view.history.last() {
        let age = now_epoch().saturating_sub(last.timestamp);
        lines.push(Line::from(format!("Last sample: {}", ago_text(age))));
    }
    let worsening = view.history.iter().filter(|snapshot| snapshot.worsening).count();
    if worsening > 0 {
        lines.push(Line::from(format!("Worsening samples: {worsening}")));
    }
    let temperatures: Vec<f64> = view
        .history
        .iter()
        .filter_map(|snapshot| snapshot.temperature_c)
        .map(|temperature| temperature as f64)
        .collect();
    if temperatures.len() > 1 {
        lines.push(Line::from(format!(
            "Temperature trend: {}",
            scaled_sparkline(&temperatures)
        )));
    }
    if !assessment.reasons.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Reasons",
            Style::default().add_modifier(Modifier::UNDERLINED),
        )));
        for reason in &assessment.reasons {
            lines.push(Line::from(format!("- {reason}")));
        }
    }
    if let Some(advice) = advice(disk.kind, assessment.tier) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(advice, Style::default().fg(Color::Yellow))));
    }
    let paragraph = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
    frame.render_widget(paragraph, area);
}

fn render_footer(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let help = "q quit   r refresh   up/down select";
    let line = if app.status.is_empty() {
        Line::from(Span::styled(help, Style::default().fg(Color::DarkGray)))
    } else {
        Line::from(vec![
            Span::styled(help, Style::default().fg(Color::DarkGray)),
            Span::raw("   "),
            Span::styled(app.status.clone(), Style::default().fg(Color::Red)),
        ])
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn state_text(app: &App) -> String {
    if app.refreshing {
        return "refreshing...".to_string();
    }
    match app.last_refresh {
        Some(at) => {
            let seconds = at.elapsed().as_secs();
            if seconds < 2 {
                "updated just now".to_string()
            } else {
                format!("updated {seconds}s ago")
            }
        }
        None => "waiting for first reading".to_string(),
    }
}

fn score_text(score: Option<f64>) -> String {
    match score {
        Some(score) => format!("{:.0}%", score.floor()),
        None => "n/a".to_string(),
    }
}

fn temperature_text(temperature: Option<i64>) -> String {
    match temperature {
        Some(temperature) => format!("{temperature}C"),
        None => "-".to_string(),
    }
}

fn tier_style(tier: Tier) -> Style {
    let color = match tier {
        Tier::Critical => Color::Red,
        Tier::Warning => Color::Yellow,
        Tier::Watch => Color::Cyan,
        Tier::Healthy => Color::Green,
        Tier::Unknown => Color::DarkGray,
    };
    Style::default().fg(color)
}

fn sparkline(scores: &[f64]) -> String {
    const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let start = scores.len().saturating_sub(TREND_POINTS);
    scores[start..]
        .iter()
        .map(|score| {
            let scaled = (score.clamp(0.0, 100.0) / 100.0) * (BLOCKS.len() - 1) as f64;
            BLOCKS[scaled.round() as usize]
        })
        .collect()
}

/// Sparkline for values without a fixed range, such as temperature.
fn scaled_sparkline(values: &[f64]) -> String {
    const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let start = values.len().saturating_sub(TREND_POINTS);
    let values = &values[start..];
    let (low, high) = values
        .iter()
        .fold((f64::MAX, f64::MIN), |(low, high), value| {
            (low.min(*value), high.max(*value))
        });
    let span = (high - low).max(0.001);
    values
        .iter()
        .map(|value| {
            let scaled = ((value - low) / span) * (BLOCKS.len() - 1) as f64;
            BLOCKS[scaled.round() as usize]
        })
        .collect()
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

fn ago_text(seconds: i64) -> String {
    if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 60 * 60 {
        format!("{}m ago", seconds / 60)
    } else {
        format!("{}h ago", seconds / (60 * 60))
    }
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn move_selection(selected: usize, len: usize, delta: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let next = selected as isize + delta;
    next.clamp(0, len as isize - 1) as usize
}

fn sort_views(disks: &mut [DiskView]) {
    disks.sort_by(|a, b| {
        severity(b.reading.assessment.tier)
            .cmp(&severity(a.reading.assessment.tier))
            .then_with(|| {
                let left = a.reading.assessment.score.unwrap_or(f64::MAX);
                let right = b.reading.assessment.score.unwrap_or(f64::MAX);
                left.total_cmp(&right)
            })
    });
}

fn severity(tier: Tier) -> u8 {
    match tier {
        Tier::Critical => 4,
        Tier::Warning => 3,
        Tier::Watch => 2,
        Tier::Unknown => 1,
        Tier::Healthy => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::score::Assessment;
    use crate::smart::{Disk, Kind};
    use ratatui::backend::TestBackend;
    use std::collections::BTreeMap;

    fn sample(model: &str, tier: Tier, score: Option<f64>) -> DiskView {
        DiskView {
            reading: Reading {
                disk_id: 0,
                disk: Disk {
                    device: "/dev/test".to_string(),
                    serial: "SERIAL".to_string(),
                    model: model.to_string(),
                    firmware: None,
                    capacity_bytes: Some(1000204886016),
                    kind: Kind::Hdd,
                    temperature_c: Some(35),
                    smart_passed: Some(true),
                    attributes: BTreeMap::new(),
                    attribute_names: BTreeMap::new(),
                    attribute_values: BTreeMap::new(),
                    nvme: None,
                },
                assessment: Assessment {
                    score,
                    tier,
                    reasons: vec!["test reason".to_string()],
                },
            },
            history: Vec::new(),
        }
    }

    #[test]
    fn sparkline_covers_the_score_range() {
        assert_eq!(sparkline(&[]), "");
        assert_eq!(sparkline(&[0.0]), "▁");
        assert_eq!(sparkline(&[100.0]), "█");
        assert_eq!(sparkline(&[0.0, 50.0, 100.0]), "▁▅█");
    }

    #[test]
    fn sparkline_keeps_only_the_last_twelve_scores() {
        let scores: Vec<f64> = (0..20).map(|i| i as f64).collect();
        assert_eq!(sparkline(&scores).chars().count(), TREND_POINTS);
    }

    #[test]
    fn scaled_sparkline_spreads_the_temperature_range() {
        assert_eq!(scaled_sparkline(&[35.0, 40.0]), "▁█");
        assert_eq!(scaled_sparkline(&[40.0, 35.0]), "█▁");
    }

    #[test]
    fn selection_is_clamped() {
        assert_eq!(move_selection(0, 3, -1), 0);
        assert_eq!(move_selection(2, 3, 1), 2);
        assert_eq!(move_selection(0, 0, 1), 0);
    }

    #[test]
    fn human_bytes_uses_decimal_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(480103981056), "480.1 GB");
        assert_eq!(human_bytes(1000204886016), "1.0 TB");
    }

    #[test]
    fn views_sort_by_risk_then_score() {
        let mut disks = vec![
            sample("healthy", Tier::Healthy, Some(100.0)),
            sample("critical", Tier::Critical, Some(9.0)),
            sample("warning", Tier::Warning, Some(10.0)),
            sample("watch", Tier::Watch, Some(50.0)),
        ];
        sort_views(&mut disks);
        let order: Vec<&str> =
            disks.iter().map(|view| view.reading.disk.model.as_str()).collect();
        assert_eq!(order, ["critical", "warning", "watch", "healthy"]);
    }

    #[test]
    fn apply_sorts_and_clamps_the_selection() {
        let mut app = App::new();
        app.selected = 5;
        app.refreshing = true;
        app.apply(Update::Done(vec![
            sample("watch", Tier::Watch, Some(50.0)),
            sample("critical", Tier::Critical, Some(9.0)),
        ]));
        assert_eq!(app.disks[0].reading.disk.model, "critical");
        assert_eq!(app.selected, 1);
        assert!(!app.refreshing);
        assert!(app.last_refresh.is_some());
    }

    #[test]
    fn apply_failure_reports_the_error() {
        let mut app = App::new();
        app.refreshing = true;
        app.apply(Update::Failed("smartctl missing".to_string()));
        assert_eq!(app.status, "smartctl missing");
        assert!(!app.refreshing);
    }

    #[test]
    fn renders_rows_and_details() {
        let backend = TestBackend::new(120, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App {
            disks: vec![sample("SYNTHETIC DISK", Tier::Critical, Some(9.0))],
            selected: 0,
            status: String::new(),
            refreshing: false,
            last_refresh: None,
            quit: false,
        };
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let text: String =
            terminal.backend().buffer().content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("SYNTHETIC DISK"));
        assert!(text.contains("CRITICAL"));
        assert!(text.contains("test reason"));
    }
}
