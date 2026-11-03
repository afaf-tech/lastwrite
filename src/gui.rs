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
use crate::view::{
    self, ago_text, human_bytes, now_epoch, score_text, temperature_text, DiskView,
};

/// How often the window redraws while a refresh is running.
const BUSY_TICK: Duration = Duration::from_millis(100);
/// Redraw rate when idle, so "updated 12s ago" stays right.
const IDLE_TICK: Duration = Duration::from_secs(1);

enum Command {
    Refresh,
    Quit,
}

enum Update {
    Done(Vec<DiskView>),
    Failed(String),
}

pub fn run(path: PathBuf) -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 700.0])
            .with_min_inner_size([720.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native("Lastwrite", options, Box::new(move |_| Ok(Box::new(App::new(path)))))
        .map_err(|e| e.to_string())
}

struct App {
    commands: Sender<Command>,
    updates: Receiver<Update>,
    disks: Vec<DiskView>,
    selected: usize,
    status: String,
    refreshing: bool,
    last_refresh: Option<Instant>,
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
            status: String::new(),
            refreshing: false,
            last_refresh: None,
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

    fn status_text(&self) -> String {
        if self.refreshing {
            return "refreshing...".to_string();
        }
        match self.last_refresh {
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

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("Lastwrite");
                ui.separator();
                if ui.button("Refresh").clicked() {
                    self.refresh();
                }
                if self.refreshing {
                    ui.spinner();
                }
                ui.label(RichText::new(self.status_text()).weak());
                if !self.status.is_empty() {
                    ui.label(RichText::new(&self.status).color(ui.visuals().error_fg_color));
                }
            });
            ui.add_space(6.0);
        });
    }

    fn drive_list(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("drives")
            .resizable(true)
            .default_size(340.0)
            .min_size(260.0)
            .max_size(520.0)
            .show(ui, |ui| {
                ui.add_space(6.0);
                ui.label(RichText::new(format!("{} drive(s)", self.disks.len())).strong());
                ui.separator();
                if self.disks.is_empty() {
                    let text = if self.refreshing {
                        "Reading drives..."
                    } else {
                        "No readable drives. Press Refresh."
                    };
                    ui.label(text);
                    return;
                }
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    for index in 0..self.disks.len() {
                        if self.card(ui, index) {
                            self.selected = index;
                        }
                    }
                });
            });
    }

    /// One clickable row in the drive list. Returns true when clicked.
    fn card(&self, ui: &mut egui::Ui, index: usize) -> bool {
        let view = &self.disks[index];
        let disk = &view.reading.disk;
        let assessment = &view.reading.assessment;
        let selected = index == self.selected;
        // A filled selection colour put the small grey metadata on a bright
        // background, so selection is shown with a thicker accent border and
        // the card keeps the same subtle fill in both themes.
        let fill = ui.visuals().faint_bg_color;
        let stroke = if selected {
            egui::Stroke::new(2.0, ui.visuals().selection.bg_fill)
        } else {
            egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color)
        };
        let color = tier_color(ui, assessment.tier);
        let response = egui::Frame::new()
            .fill(fill)
            .stroke(stroke)
            .corner_radius(egui::CornerRadius::same(6))
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&disk.model).strong());
                        ui.label(
                            RichText::new(format!("{}  ·  {}", disk.kind.label(), disk.device))
                                .small()
                                .weak(),
                        );
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(assessment.tier.label()).small().color(color));
                        ui.label(
                            RichText::new(score_text(assessment.score)).strong().color(color),
                        );
                    });
                });
            })
            .response;
        ui.add_space(6.0);
        response.interact(egui::Sense::click()).clicked()
    }

    fn details(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default().show(ui, |ui| {
            let Some(view) = self.disks.get(self.selected) else {
                ui.centered_and_justified(|ui| {
                    ui.label("No drive selected.");
                });
                return;
            };
            let disk = &view.reading.disk;
            let assessment = &view.reading.assessment;
            let color = tier_color(ui, assessment.tier);

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.heading(&disk.model);
                ui.label(RichText::new(disk.kind.label()).weak());
            });
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(score_text(assessment.score))
                        .size(36.0)
                        .strong()
                        .color(color),
                );
                ui.label(RichText::new(assessment.tier.label()).size(18.0).color(color));
                ui.add_space(12.0);
                ui.label(RichText::new(temperature_text(disk.temperature_c)).weak());
            });
            ui.add_space(8.0);

            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                egui::Grid::new("metadata").num_columns(2).spacing([18.0, 4.0]).show(ui, |ui| {
                    row(ui, "Serial", &disk.serial);
                    row(ui, "Device", &disk.device);
                    if let Some(firmware) = &disk.firmware {
                        row(ui, "Firmware", firmware);
                    }
                    if let Some(capacity) = disk.capacity_bytes {
                        row(ui, "Capacity", &human_bytes(capacity));
                    }
                    if let Some(last) = view.history.last() {
                        let age = now_epoch().saturating_sub(last.timestamp);
                        row(ui, "Last sample", &ago_text(age));
                    }
                    let worsening = view.history.iter().filter(|s| s.worsening).count();
                    if worsening > 0 {
                        row(ui, "Worsening samples", &worsening.to_string());
                    }
                });

                ui.add_space(14.0);
                ui.label(RichText::new("Score history").strong());
                trend_chart(ui, &view.score_samples());

                ui.add_space(14.0);
                ui.label(RichText::new("Why this score").strong());
                if assessment.reasons.is_empty() {
                    ui.label(RichText::new("No damage found on the tracked attributes.").weak());
                }
                for reason in &assessment.reasons {
                    ui.label(format!("• {reason}"));
                }

                if let Some(advice) = advice(disk.kind, assessment.tier) {
                    ui.add_space(14.0);
                    ui.label(RichText::new(advice).color(color));
                }
                ui.add_space(8.0);
            });
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        while let Ok(update) = self.updates.try_recv() {
            self.apply(update);
        }
        ui.ctx().request_repaint_after(if self.refreshing { BUSY_TICK } else { IDLE_TICK });
        self.top_bar(ui);
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

fn row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.label(RichText::new(label).weak());
    ui.label(value);
    ui.end_row();
}

fn tier_color(ui: &egui::Ui, tier: Tier) -> Color32 {
    match tier {
        Tier::Critical => Color32::from_rgb(220, 80, 72),
        Tier::Warning => Color32::from_rgb(214, 160, 40),
        Tier::Watch => Color32::from_rgb(70, 160, 210),
        Tier::Healthy => Color32::from_rgb(70, 170, 100),
        Tier::Unknown => ui.visuals().weak_text_color(),
    }
}

fn score_color(ui: &egui::Ui, score: f64) -> Color32 {
    if score < 20.0 {
        tier_color(ui, Tier::Warning)
    } else if score < 100.0 {
        tier_color(ui, Tier::Watch)
    } else {
        tier_color(ui, Tier::Healthy)
    }
}

/// Bars, oldest on the left, each one 0-100. The bars are capped in width and
/// right-aligned so a short history does not stretch across the whole panel,
/// and hovering one shows its score and age.
fn trend_chart(ui: &mut egui::Ui, samples: &[(f64, i64)]) {
    let (response, painter) =
        ui.allocate_painter(egui::vec2(ui.available_width(), 96.0), egui::Sense::hover());
    let rect = response.rect;
    painter.rect_filled(rect, egui::CornerRadius::same(4), ui.visuals().extreme_bg_color);
    if samples.is_empty() {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "No history yet",
            egui::FontId::proportional(13.0),
            ui.visuals().weak_text_color(),
        );
        return;
    }

    let weak = ui.visuals().weak_text_color();
    let plot = egui::Rect::from_min_max(
        egui::pos2(rect.left() + 26.0, rect.top() + 4.0),
        egui::pos2(rect.right() - 4.0, rect.bottom() - 4.0),
    );
    painter.text(
        egui::pos2(rect.left() + 3.0, plot.top()),
        egui::Align2::LEFT_TOP,
        "100",
        egui::FontId::proportional(10.0),
        weak,
    );
    painter.text(
        egui::pos2(rect.left() + 3.0, plot.bottom()),
        egui::Align2::LEFT_BOTTOM,
        "0",
        egui::FontId::proportional(10.0),
        weak,
    );
    painter.line_segment(
        [egui::pos2(plot.left(), plot.bottom()), egui::pos2(plot.right(), plot.bottom())],
        egui::Stroke::new(1.0, ui.visuals().widgets.noninteractive.bg_stroke.color),
    );

    let gap = 3.0;
    let count = samples.len() as f32;
    let bar_width = ((plot.width() - gap * (count - 1.0)) / count).clamp(2.0, 16.0);
    let used = count * bar_width + (count - 1.0) * gap;
    let start = (plot.right() - used).max(plot.left());
    let hovered = response.hover_pos().and_then(|pos| {
        if !plot.contains(pos) {
            return None;
        }
        let index = ((pos.x - start) / (bar_width + gap)).floor();
        (index >= 0.0 && (index as usize) < samples.len()).then_some(index as usize)
    });
    for (index, (score, _)) in samples.iter().enumerate() {
        let ratio = (score.clamp(0.0, 100.0) / 100.0) as f32;
        let height = (ratio * plot.height()).max(2.0);
        let left = start + index as f32 * (bar_width + gap);
        let bar = egui::Rect::from_min_max(
            egui::pos2(left, plot.bottom() - height),
            egui::pos2(left + bar_width, plot.bottom()),
        );
        painter.rect_filled(bar, egui::CornerRadius::same(2), score_color(ui, *score));
    }
    if let Some(index) = hovered {
        let (score, timestamp) = samples[index];
        let age = now_epoch().saturating_sub(timestamp);
        response.on_hover_text(format!("{} · {}", score_text(Some(score)), ago_text(age)));
    }
}
