//! Desktop dashboard, built with egui. Compiled only with the `gui` feature.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{Color32, RichText};

use crate::advice;
use crate::score::Tier;
use crate::store::Store;
use crate::view::{self, ago_text, human_bytes, now_epoch, score_text, temperature_text, DiskView};

/// How often the window redraws while a refresh is running.
const BUSY_TICK: Duration = Duration::from_millis(100);
/// Redraw rate when idle, so "updated 12s ago" stays right.
const IDLE_TICK: Duration = Duration::from_secs(1);
/// Drives skipped by PageUp and PageDown.
const PAGE_STEP: usize = 5;
/// Time between automatic refreshes when "Auto" is on.
const AUTO_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, PartialEq, Debug)]
enum Movement {
    Up,
    Down,
    PageUp,
    PageDown,
    First,
    Last,
}

/// The selection after a key press, clamped to the list.
fn moved(selected: usize, count: usize, movement: Movement) -> usize {
    if count == 0 {
        return 0;
    }
    let last = count - 1;
    match movement {
        Movement::Up => selected.saturating_sub(1),
        Movement::Down => (selected + 1).min(last),
        Movement::PageUp => selected.saturating_sub(PAGE_STEP),
        Movement::PageDown => (selected + PAGE_STEP).min(last),
        Movement::First => 0,
        Movement::Last => last,
    }
}

/// True when every word of the query appears in one of the fields, ignoring
/// case. An empty query matches everything.
fn matches_query(query: &str, fields: &[&str]) -> bool {
    let fields: Vec<String> = fields.iter().map(|field| field.to_lowercase()).collect();
    query
        .to_lowercase()
        .split_whitespace()
        .all(|word| fields.iter().any(|field| field.contains(word)))
}

/// Drives per tier, most severe first, skipping tiers with no drives.
fn tier_counts(tiers: impl Iterator<Item = Tier>) -> Vec<(Tier, usize)> {
    let all = [
        Tier::Critical,
        Tier::Warning,
        Tier::Watch,
        Tier::Unknown,
        Tier::Healthy,
    ];
    let tiers: Vec<Tier> = tiers.collect();
    all.into_iter()
        .map(|tier| (tier, tiers.iter().filter(|t| **t == tier).count()))
        .filter(|(_, count)| *count > 0)
        .collect()
}

enum Command {
    Refresh,
    Quit,
}

enum Update {
    Done(Vec<DiskView>),
    Failed(String),
}

pub fn run(path: PathBuf) -> Result<(), String> {
    // Without this the window, taskbar and Dock fall back to egui's default
    // icon, so the app would not match the icon in the README and the bundle.
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../icons/icon.png"))
        .map_err(|e| format!("cannot load the app icon: {e}"))?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 760.0])
            .with_min_inner_size([760.0, 500.0])
            .with_icon(icon),
        ..Default::default()
    };
    eframe::run_native(
        "Lastwrite",
        options,
        Box::new(move |cc| {
            apply_style(&cc.egui_ctx);
            Ok(Box::new(App::new(path)))
        }),
    )
    .map_err(|e| e.to_string())
}

/// Larger type, rounder widgets and roomier spacing than egui's defaults.
fn apply_style(ctx: &egui::Context) {
    use egui::{FontFamily::Proportional, FontId, TextStyle};
    ctx.global_style_mut(|style| {
        style.text_styles = [
            (TextStyle::Heading, FontId::new(22.0, Proportional)),
            (TextStyle::Body, FontId::new(14.5, Proportional)),
            (TextStyle::Button, FontId::new(14.0, Proportional)),
            (TextStyle::Small, FontId::new(11.5, Proportional)),
            (
                TextStyle::Monospace,
                FontId::new(13.0, egui::FontFamily::Monospace),
            ),
        ]
        .into();
        style.spacing.item_spacing = egui::vec2(10.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 5.0);
        style.spacing.interact_size.y = 28.0;
        let radius = egui::CornerRadius::same(7);
        style.visuals.widgets.noninteractive.corner_radius = radius;
        style.visuals.widgets.inactive.corner_radius = radius;
        style.visuals.widgets.hovered.corner_radius = radius;
        style.visuals.widgets.active.corner_radius = radius;
        style.visuals.window_corner_radius = egui::CornerRadius::same(10);
    });
}

struct App {
    commands: Sender<Command>,
    updates: Receiver<Update>,
    disks: Vec<DiskView>,
    selected: usize,
    /// Scroll the selected card into view after a keyboard move.
    scroll_to_selected: bool,
    status: String,
    refreshing: bool,
    last_refresh: Option<Instant>,
    filter: String,
    auto_refresh: bool,
    search_id: egui::Id,
}

impl App {
    fn new(path: PathBuf) -> App {
        let (commands, command_rx) = mpsc::channel();
        let (update_tx, updates) = mpsc::channel();
        thread::spawn(move || worker(path, command_rx, update_tx));
        let mut app = App {
            commands,
            updates,
            disks: Vec::new(),
            selected: 0,
            scroll_to_selected: false,
            status: String::new(),
            refreshing: false,
            last_refresh: None,
            filter: String::new(),
            auto_refresh: false,
            search_id: egui::Id::unique("search"),
        };
        app.refresh();
        app
    }

    fn refresh(&mut self) {
        if self.refreshing {
            return;
        }
        if self.commands.send(Command::Refresh).is_err() {
            self.status = "refresh worker stopped".to_string();
            return;
        }
        self.refreshing = true;
        self.status.clear();
    }

    fn apply(&mut self, update: Update) {
        match update {
            Update::Done(disks) => {
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

    /// Indexes into `disks` that pass the search box, in list order.
    fn visible(&self) -> Vec<usize> {
        (0..self.disks.len())
            .filter(|&index| {
                let disk = &self.disks[index].reading.disk;
                matches_query(&self.filter, &[&disk.model, &disk.serial, &disk.device])
            })
            .collect()
    }

    fn auto_refresh_due(&self) -> bool {
        self.auto_refresh
            && !self.refreshing
            && self
                .last_refresh
                .is_some_and(|at| at.elapsed() >= AUTO_INTERVAL)
    }

    /// Keyboard shortcuts mirror the terminal dashboard: arrows or j/k move,
    /// Home/g and End/G jump, r refreshes, / searches and Esc closes. Letter
    /// keys are ignored while the search box has focus.
    fn handle_keys(&mut self, ctx: &egui::Context) {
        let typing = ctx.egui_wants_keyboard_input();
        let (movement, refresh, close, search) = ctx.input(|input| {
            let movement = if input.key_pressed(egui::Key::ArrowUp)
                || (!typing && input.key_pressed(egui::Key::K))
            {
                Some(Movement::Up)
            } else if input.key_pressed(egui::Key::ArrowDown)
                || (!typing && input.key_pressed(egui::Key::J))
            {
                Some(Movement::Down)
            } else if input.key_pressed(egui::Key::PageUp) {
                Some(Movement::PageUp)
            } else if input.key_pressed(egui::Key::PageDown) {
                Some(Movement::PageDown)
            } else if input.key_pressed(egui::Key::Home) {
                Some(Movement::First)
            } else if input.key_pressed(egui::Key::End) {
                Some(Movement::Last)
            } else if !typing && input.key_pressed(egui::Key::G) {
                Some(if input.modifiers.shift {
                    Movement::Last
                } else {
                    Movement::First
                })
            } else {
                None
            };
            (
                movement,
                !typing && input.key_pressed(egui::Key::R),
                input.key_pressed(egui::Key::Escape),
                !typing && input.key_pressed(egui::Key::Slash),
            )
        });
        if close {
            if typing {
                // Esc leaves the search box first and clears it.
                self.filter.clear();
                ctx.memory_mut(|memory| memory.stop_text_input());
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            return;
        }
        if search {
            ctx.memory_mut(|memory| memory.request_focus(self.search_id));
        }
        if refresh {
            self.refresh();
        }
        if let Some(movement) = movement {
            let visible = self.visible();
            let position = visible
                .iter()
                .position(|&index| index == self.selected)
                .unwrap_or(0);
            let next = moved(position, visible.len(), movement);
            if let Some(&index) = visible.get(next) {
                if index != self.selected {
                    self.selected = index;
                    self.scroll_to_selected = true;
                }
            }
        }
    }

    fn status_text(&self) -> String {
        if self.refreshing {
            return "Refreshing…".to_string();
        }
        match self.last_refresh {
            Some(at) => {
                let seconds = at.elapsed().as_secs();
                if seconds < 2 {
                    "Updated just now".to_string()
                } else {
                    format!("Updated {seconds}s ago")
                }
            }
            None => "Waiting for first reading".to_string(),
        }
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("◉").size(24.0).color(ACCENT));
                ui.heading("Lastwrite");
                ui.add_space(8.0);
                let counts = tier_counts(self.disks.iter().map(|d| d.reading.assessment.tier));
                if counts.is_empty() {
                    ui.label(RichText::new("no drives").weak());
                }
                for (tier, count) in counts {
                    pill(
                        ui,
                        &format!("{count} {}", tier.label().to_lowercase()),
                        tier_color(ui, tier),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let label = if self.refreshing {
                        "Refreshing"
                    } else {
                        "Refresh"
                    };
                    if ui
                        .add_enabled(!self.refreshing, egui::Button::new(label))
                        .clicked()
                    {
                        self.refresh();
                    }
                    ui.checkbox(&mut self.auto_refresh, "Auto (60s)");
                    if self.refreshing {
                        ui.spinner();
                    }
                });
            });
            ui.add_space(8.0);
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.add_space(3.0);
            ui.horizontal(|ui| {
                ui.label(RichText::new(self.status_text()).small().weak());
                if !self.status.is_empty() {
                    ui.label(
                        RichText::new(&self.status)
                            .small()
                            .color(ui.visuals().error_fg_color),
                    );
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new("↑↓ select  ·  / search  ·  R refresh  ·  Esc close")
                            .small()
                            .weak(),
                    );
                });
            });
            ui.add_space(3.0);
        });
    }

    fn drive_list(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("drives")
            .resizable(true)
            .default_size(340.0)
            .min_size(270.0)
            .max_size(520.0)
            .show(ui, |ui| {
                ui.add_space(8.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.filter)
                        .id(self.search_id)
                        .hint_text("Search model, serial or device")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(2.0);
                let visible = self.visible();
                let total = self.disks.len();
                let heading = if visible.len() == total {
                    format!("{total} drive(s)")
                } else {
                    format!("{} of {total} drive(s)", visible.len())
                };
                ui.label(RichText::new(heading).small().weak());
                if visible.is_empty() {
                    let text = if self.refreshing {
                        "Reading drives…"
                    } else if total == 0 {
                        "No readable drives. Is smartmontools installed? Press Refresh."
                    } else {
                        "No drive matches the search."
                    };
                    ui.label(text);
                    return;
                }
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let mut selected_rect = None;
                        for index in visible {
                            let response = self.card(ui, index);
                            if response.clicked() {
                                self.selected = index;
                            }
                            if index == self.selected {
                                selected_rect = Some(response.rect);
                            }
                        }
                        if self.scroll_to_selected {
                            if let Some(rect) = selected_rect {
                                ui.scroll_to_rect(rect, Some(egui::Align::Center));
                            }
                            self.scroll_to_selected = false;
                        }
                    });
            });
    }

    /// One clickable row in the drive list: a tier-coloured stripe, the name,
    /// the score and a thin score bar.
    fn card(&self, ui: &mut egui::Ui, index: usize) -> egui::Response {
        let view = &self.disks[index];
        let disk = &view.reading.disk;
        let assessment = &view.reading.assessment;
        let selected = index == self.selected;
        let color = tier_color(ui, assessment.tier);
        let fill = if selected {
            color.gamma_multiply(0.14)
        } else {
            ui.visuals().faint_bg_color
        };
        let stroke = if selected {
            egui::Stroke::new(1.5, color)
        } else {
            egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color)
        };
        let frame = egui::Frame::new()
            .fill(fill)
            .stroke(stroke)
            .corner_radius(egui::CornerRadius::same(8))
            .inner_margin(egui::Margin {
                left: 16,
                right: 12,
                top: 9,
                bottom: 9,
            });
        let inner = frame.show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(&disk.model).strong());
                    let capacity = disk.capacity_bytes.map(human_bytes).unwrap_or_default();
                    ui.label(
                        RichText::new(format!(
                            "{}  ·  {}  ·  {}",
                            disk.kind.label(),
                            disk.device,
                            capacity
                        ))
                        .small()
                        .weak(),
                    );
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.vertical(|ui| {
                        ui.with_layout(egui::Layout::top_down(egui::Align::Max), |ui| {
                            ui.label(
                                RichText::new(score_text(assessment.score))
                                    .size(18.0)
                                    .strong()
                                    .color(color),
                            );
                            ui.label(RichText::new(assessment.tier.label()).small().color(color));
                        });
                    });
                });
            });
            ui.add_space(2.0);
            score_bar(ui, assessment.score, color);
        });
        // The stripe sits on the left edge, over the frame's rounded border.
        let rect = inner.response.rect;
        let stripe = egui::Rect::from_min_max(
            rect.left_top() + egui::vec2(0.0, 6.0),
            egui::pos2(rect.left() + 4.0, rect.bottom() - 6.0),
        );
        ui.painter()
            .rect_filled(stripe, egui::CornerRadius::same(2), color);
        ui.add_space(4.0);
        inner
            .response
            .interact(egui::Sense::click())
            .on_hover_cursor(egui::CursorIcon::PointingHand)
    }

    fn details(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            let Some(view) = self.disks.get(self.selected) else {
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new("No drive selected.").weak());
                });
                return;
            };
            let disk = &view.reading.disk;
            let assessment = &view.reading.assessment;
            let color = tier_color(ui, assessment.tier);

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.add_space(6.0);
                    ui.heading(&disk.model);
                    ui.label(
                        RichText::new(format!(
                            "{}  ·  {}  ·  {}",
                            disk.kind.label(),
                            disk.device,
                            disk.serial
                        ))
                        .weak(),
                    );
                    ui.add_space(6.0);

                    // Verdict: the gauge, the tier and the advice side by side.
                    card_frame(ui).show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        ui.horizontal(|ui| {
                            gauge(ui, assessment.score, color);
                            ui.add_space(8.0);
                            ui.vertical(|ui| {
                                pill(ui, assessment.tier.label(), color);
                                match advice(disk.kind, assessment.tier) {
                                    Some(text) => {
                                        ui.add(
                                            egui::Label::new(RichText::new(text).size(15.0)).wrap(),
                                        );
                                    }
                                    None => {
                                        ui.label(
                                            RichText::new("No action needed right now.").weak(),
                                        );
                                    }
                                }
                            });
                        });
                    });
                    ui.add_space(4.0);

                    ui.horizontal_wrapped(|ui| {
                        tile(
                            ui,
                            "Temperature",
                            &temperature_text(disk.temperature_c),
                            None,
                        );
                        let capacity = disk.capacity_bytes.map(human_bytes);
                        tile(ui, "Capacity", capacity.as_deref().unwrap_or("-"), None);
                        let smart = match disk.smart_passed {
                            Some(true) => ("Passed", Some(tier_color(ui, Tier::Healthy))),
                            Some(false) => ("Failed", Some(tier_color(ui, Tier::Critical))),
                            None => ("-", None),
                        };
                        tile(ui, "SMART self-assessment", smart.0, smart.1);
                        if let Some(last) = view.history.last() {
                            let age = now_epoch().saturating_sub(last.timestamp);
                            tile(ui, "Last sample", &ago_text(age), None);
                        }
                        let worsening = view.history.iter().filter(|s| s.worsening).count();
                        if worsening > 0 {
                            let warning = Some(tier_color(ui, Tier::Warning));
                            tile(ui, "Worsening samples", &worsening.to_string(), warning);
                        }
                    });
                    ui.add_space(6.0);

                    section(ui, "Score history");
                    let scores = view.score_samples();
                    line_chart(ui, &scores, 0.0, 100.0, color, |v| score_text(Some(v)));

                    let temps: Vec<(f64, i64)> = view
                        .history
                        .iter()
                        .filter_map(|s| s.temperature_c.map(|t| (t as f64, s.timestamp)))
                        .collect();
                    if temps.len() >= 2 {
                        ui.add_space(8.0);
                        section(ui, "Temperature history");
                        let low = temps.iter().map(|t| t.0).fold(f64::MAX, f64::min);
                        let high = temps.iter().map(|t| t.0).fold(f64::MIN, f64::max);
                        let (low, high) = ((low - 3.0).floor().max(0.0), (high + 3.0).ceil());
                        line_chart(ui, &temps, low, high, ACCENT, |v| {
                            temperature_text(Some(v as i64))
                        });
                    }

                    ui.add_space(8.0);
                    section(ui, "Why this score");
                    card_frame(ui).show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        if assessment.reasons.is_empty() {
                            ui.label(
                                RichText::new("No damage found on the tracked attributes.").weak(),
                            );
                        }
                        for reason in &assessment.reasons {
                            ui.horizontal_top(|ui| {
                                ui.label(RichText::new("●").small().color(color));
                                ui.add(egui::Label::new(reason).wrap());
                            });
                        }
                    });

                    ui.add_space(8.0);
                    section(ui, "Identity");
                    card_frame(ui).show(ui, |ui| {
                        ui.set_min_width(ui.available_width());
                        egui::Grid::new("metadata")
                            .num_columns(2)
                            .spacing([24.0, 6.0])
                            .show(ui, |ui| {
                                row(ui, "Model", &disk.model);
                                row(ui, "Serial", &disk.serial);
                                row(ui, "Device", &disk.device);
                                if let Some(firmware) = &disk.firmware {
                                    row(ui, "Firmware", firmware);
                                }
                            });
                    });
                    ui.add_space(12.0);
                });
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        while let Ok(update) = self.updates.try_recv() {
            self.apply(update);
        }
        if self.auto_refresh_due() {
            self.refresh();
        }
        ui.ctx().request_repaint_after(if self.refreshing {
            BUSY_TICK
        } else {
            IDLE_TICK
        });
        self.handle_keys(ui.ctx());
        self.top_bar(ui);
        self.status_bar(ui);
        self.drive_list(ui);
        self.details(ui);
    }
}

impl Drop for App {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Quit);
    }
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
                let update = match view::refresh(&mut store) {
                    Ok(disks) => Update::Done(disks),
                    Err(message) => Update::Failed(message),
                };
                if updates.send(update).is_err() {
                    break;
                }
            }
        }
    }
}

/// Brand colour, used for the logo and for charts that are not about a tier.
const ACCENT: Color32 = Color32::from_rgb(98, 142, 235);

fn row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.label(RichText::new(label).weak());
    ui.label(value);
    ui.end_row();
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.label(RichText::new(title).size(16.0).strong());
}

fn card_frame(ui: &egui::Ui) -> egui::Frame {
    egui::Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .stroke(egui::Stroke::new(
            1.0,
            ui.visuals().widgets.noninteractive.bg_stroke.color,
        ))
        .corner_radius(egui::CornerRadius::same(10))
        .inner_margin(egui::Margin::same(14))
}

/// A small rounded label tinted with `color`.
fn pill(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.18))
        .corner_radius(egui::CornerRadius::same(12))
        .inner_margin(egui::Margin::symmetric(10, 3))
        .show(ui, |ui| {
            ui.label(RichText::new(text).strong().color(color));
        });
}

/// A labelled value box. `color` tints the value.
fn tile(ui: &mut egui::Ui, label: &str, value: &str, color: Option<Color32>) {
    card_frame(ui)
        .inner_margin(egui::Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_min_width(110.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(label).small().weak());
                let mut text = RichText::new(value).size(20.0).strong();
                if let Some(color) = color {
                    text = text.color(color);
                }
                ui.label(text);
            });
        });
}

fn tier_color(ui: &egui::Ui, tier: Tier) -> Color32 {
    match tier {
        Tier::Critical => Color32::from_rgb(226, 84, 76),
        Tier::Warning => Color32::from_rgb(222, 160, 40),
        Tier::Watch => Color32::from_rgb(70, 160, 214),
        Tier::Healthy => Color32::from_rgb(64, 176, 104),
        Tier::Unknown => ui.visuals().weak_text_color(),
    }
}

/// A thin progress bar for a 0-100 score.
fn score_bar(ui: &mut egui::Ui, score: Option<f64>, color: Color32) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 4.0), egui::Sense::hover());
    let round = egui::CornerRadius::same(2);
    ui.painter()
        .rect_filled(rect, round, ui.visuals().extreme_bg_color);
    if let Some(score) = score {
        let ratio = (score.clamp(0.0, 100.0) / 100.0) as f32;
        let fill = egui::Rect::from_min_size(rect.min, egui::vec2(rect.width() * ratio, 4.0));
        ui.painter().rect_filled(fill, round, color);
    }
}

/// A ring that fills clockwise from the top in proportion to the score, with
/// the score in the middle.
fn gauge(ui: &mut egui::Ui, score: Option<f64>, color: Color32) {
    let size = 116.0;
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let center = rect.center();
    let thickness = 11.0;
    let radius = size / 2.0 - thickness;
    let track = ui.visuals().widgets.noninteractive.bg_stroke.color;
    painter.circle_stroke(center, radius, egui::Stroke::new(thickness, track));
    if let Some(score) = score {
        let ratio = (score.clamp(0.0, 100.0) / 100.0) as f32;
        let steps = ((ratio * 96.0).ceil() as usize).max(2);
        let start = -std::f32::consts::FRAC_PI_2;
        let points: Vec<egui::Pos2> = (0..=steps)
            .map(|step| {
                let angle = start + std::f32::consts::TAU * ratio * step as f32 / steps as f32;
                center + radius * egui::vec2(angle.cos(), angle.sin())
            })
            .collect();
        painter.add(egui::Shape::line(
            points,
            egui::Stroke::new(thickness, color),
        ));
    }
    painter.text(
        center,
        egui::Align2::CENTER_CENTER,
        score_text(score),
        egui::FontId::proportional(26.0),
        color,
    );
}

/// A line chart over evenly spaced samples, oldest on the left, with the area
/// under the line tinted. Hovering shows the nearest sample's value and age.
fn line_chart(
    ui: &mut egui::Ui,
    samples: &[(f64, i64)],
    low: f64,
    high: f64,
    color: Color32,
    format: impl Fn(f64) -> String,
) {
    let (response, painter) = ui.allocate_painter(
        egui::vec2(ui.available_width(), 150.0),
        egui::Sense::hover(),
    );
    let rect = response.rect;
    painter.rect_filled(
        rect,
        egui::CornerRadius::same(10),
        ui.visuals().extreme_bg_color,
    );
    let weak = ui.visuals().weak_text_color();
    if samples.is_empty() {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "No history yet",
            egui::FontId::proportional(13.0),
            weak,
        );
        return;
    }

    let plot = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 42.0, rect.top() + 12.0),
        egui::pos2(rect.right() - 16.0, rect.bottom() - 12.0),
    );
    let grid = ui
        .visuals()
        .widgets
        .noninteractive
        .bg_stroke
        .color
        .gamma_multiply(0.6);
    for step in 0..=2 {
        let t = step as f32 / 2.0;
        let y = plot.bottom() - t * plot.height();
        painter.line_segment(
            [egui::pos2(plot.left(), y), egui::pos2(plot.right(), y)],
            egui::Stroke::new(1.0, grid),
        );
        let value = low + (high - low) * t as f64;
        painter.text(
            egui::pos2(rect.left() + 8.0, y),
            egui::Align2::LEFT_CENTER,
            format!("{value:.0}"),
            egui::FontId::proportional(11.0),
            weak,
        );
    }

    let span = (high - low).max(f64::EPSILON);
    let count = samples.len();
    let point = |index: usize| {
        let x = if count == 1 {
            plot.center().x
        } else {
            plot.left() + plot.width() * index as f32 / (count - 1) as f32
        };
        let ratio = ((samples[index].0 - low) / span).clamp(0.0, 1.0) as f32;
        egui::pos2(x, plot.bottom() - ratio * plot.height())
    };
    let points: Vec<egui::Pos2> = (0..count).map(point).collect();

    // The area is drawn as one trapezoid per segment, since each is convex.
    for pair in points.windows(2) {
        let quad = vec![
            pair[0],
            pair[1],
            egui::pos2(pair[1].x, plot.bottom()),
            egui::pos2(pair[0].x, plot.bottom()),
        ];
        painter.add(egui::Shape::convex_polygon(
            quad,
            color.gamma_multiply(0.16),
            egui::Stroke::NONE,
        ));
    }
    painter.add(egui::Shape::line(
        points.clone(),
        egui::Stroke::new(2.0, color),
    ));
    for p in &points {
        painter.circle_filled(*p, 2.5, color);
    }

    if let Some(pos) = response
        .hover_pos()
        .filter(|pos| plot.expand2(egui::vec2(8.0, 12.0)).contains(*pos))
    {
        let nearest = (0..count)
            .min_by(|&a, &b| {
                (points[a].x - pos.x)
                    .abs()
                    .total_cmp(&(points[b].x - pos.x).abs())
            })
            .unwrap_or(0);
        let p = points[nearest];
        painter.line_segment(
            [egui::pos2(p.x, plot.top()), egui::pos2(p.x, plot.bottom())],
            egui::Stroke::new(1.0, weak),
        );
        painter.circle(
            p,
            5.0,
            color,
            egui::Stroke::new(2.0, ui.visuals().extreme_bg_color),
        );
        let (value, timestamp) = samples[nearest];
        let age = now_epoch().saturating_sub(timestamp);
        let tip = format!("{} · {}", format(value), ago_text(age));
        response.on_hover_text_at_pointer(tip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_icon_is_the_png_from_the_readme() {
        let icon = eframe::icon_data::from_png_bytes(include_bytes!("../icons/icon.png"))
            .expect("icons/icon.png must decode");
        assert_eq!((icon.width, icon.height), (1024, 1024));
        assert_eq!(icon.rgba.len(), 1024 * 1024 * 4);
    }

    #[test]
    fn movement_stops_at_the_ends() {
        assert_eq!(moved(0, 3, Movement::Up), 0);
        assert_eq!(moved(2, 3, Movement::Down), 2);
        assert_eq!(moved(1, 3, Movement::Up), 0);
        assert_eq!(moved(1, 3, Movement::Down), 2);
    }

    #[test]
    fn page_and_jumps_clamp_to_the_list() {
        assert_eq!(moved(0, 12, Movement::PageDown), 5);
        assert_eq!(moved(10, 12, Movement::PageDown), 11);
        assert_eq!(moved(3, 12, Movement::PageUp), 0);
        assert_eq!(moved(7, 12, Movement::First), 0);
        assert_eq!(moved(7, 12, Movement::Last), 11);
    }

    #[test]
    fn an_empty_list_has_no_selection() {
        assert_eq!(moved(4, 0, Movement::Down), 0);
    }

    #[test]
    fn search_needs_every_word_in_some_field() {
        let fields = ["Samsung SSD 860", "S3Z9NB0K", "/dev/disk0"];
        assert!(matches_query("", &fields));
        assert!(matches_query("samsung 860", &fields));
        assert!(matches_query("s3z9 DISK0", &fields));
        assert!(!matches_query("samsung seagate", &fields));
    }

    #[test]
    fn tier_counts_are_most_severe_first_and_skip_empty_tiers() {
        let tiers = [Tier::Healthy, Tier::Critical, Tier::Healthy, Tier::Watch];
        assert_eq!(
            tier_counts(tiers.into_iter()),
            vec![(Tier::Critical, 1), (Tier::Watch, 1), (Tier::Healthy, 2)]
        );
    }
}
