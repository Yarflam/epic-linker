//! Epic Linker — GUI to relink Epic Games installs with the launcher.
//! English UI.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod core;

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};

use core::*;
use eframe::egui;
use egui_extras::{Column, TableBuilder};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Config persistence
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Default)]
struct Config {
    extra_dirs: Vec<String>,
}

fn config_path() -> PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        let dir = PathBuf::from(appdata).join("epic-linker-gui");
        let _ = std::fs::create_dir_all(&dir);
        dir.join("config.json")
    } else {
        PathBuf::from("config.json")
    }
}

fn load_config() -> Config {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_config(cfg: &Config) {
    if let Ok(s) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(config_path(), s);
    }
}

// ---------------------------------------------------------------------------
// Directory list
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct DirSpec {
    path: String,
    is_default: bool,
}

/// Builds the directory list: default install dir (locked) + configured dirs,
/// plus any extra roots discovered from LauncherInstalled.dat.
fn detect_dirs(cfg: &mut Config) -> Vec<DirSpec> {
    let mut dirs: Vec<DirSpec> = vec![DirSpec {
        path: DEFAULT_GAMES_DIR.to_string(),
        is_default: true,
    }];

    for d in &cfg.extra_dirs {
        if !dirs.iter().any(|x| x.path.eq_ignore_ascii_case(d)) {
            dirs.push(DirSpec {
                path: d.clone(),
                is_default: false,
            });
        }
    }

    // Auto-discover game library roots from LauncherInstalled.dat
    if let Some((dat, _)) = load_installed_dat(&PathBuf::from(DEFAULT_INSTALLED_DAT)) {
        if let Some(list) = dat.get("InstallationList").and_then(|v| v.as_array()) {
            for e in list {
                if let Some(loc) = e.get("InstallLocation").and_then(|v| v.as_str()) {
                    if let Some(parent) = PathBuf::from(loc).parent() {
                        let p = parent.to_string_lossy().into_owned();
                        if !p.is_empty()
                            && !dirs.iter().any(|x| x.path.eq_ignore_ascii_case(&p))
                        {
                            cfg.extra_dirs.push(p.clone());
                            dirs.push(DirSpec {
                                path: p,
                                is_default: false,
                            });
                        }
                    }
                }
            }
        }
    }
    save_config(cfg);
    dirs
}

// ---------------------------------------------------------------------------
// Background scanning
// ---------------------------------------------------------------------------

enum ScanMsg {
    Log(String),
    Game(GameInfo),
    Done(ScanContext),
}

// ---------------------------------------------------------------------------
// Application
// ---------------------------------------------------------------------------

struct EpicLinkerApp {
    dirs: Vec<DirSpec>,
    games: Vec<GameInfo>,
    ctx: Option<ScanContext>,
    rx: Option<Receiver<ScanMsg>>,
    scanning: bool,
    busy: bool,
    log: Vec<String>,
    cfg: Config,
}

impl EpicLinkerApp {
    fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let mut cfg = load_config();
        let dirs = detect_dirs(&mut cfg);
        let mut app = EpicLinkerApp {
            dirs,
            games: Vec::new(),
            ctx: None,
            rx: None,
            scanning: false,
            busy: false,
            log: Vec::new(),
            cfg,
        };
        app.start_scan();
        app
    }

    fn start_scan(&mut self) {
        self.games.clear();
        self.ctx = None;
        self.scanning = true;
        self.log.clear();
        self.log.push("Scanning…".to_string());
        let dirs: Vec<String> = self.dirs.iter().map(|d| d.path.clone()).collect();
        let paths = CtxPaths::default();
        let (tx, rx) = channel();
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let mut ctx = ScanContext::load(&paths);
            for d in &dirs {
                let _ = tx.send(ScanMsg::Log(format!("Scanning {}…", d)));
                let Ok(rd) = std::fs::read_dir(d) else {
                    let _ = tx.send(ScanMsg::Log(format!("  directory not found: {}", d)));
                    continue;
                };
                let mut entries: Vec<_> = rd.flatten().collect();
                entries.sort_by_key(|e| e.file_name());
                for e in entries {
                    if !e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        continue;
                    }
                    if let Some(mut info) = scan_game(&e.path()) {
                        finalize_game(&mut ctx, &mut info);
                        let _ = tx.send(ScanMsg::Game(info));
                    }
                }
            }
            save_db(&ctx);
            let _ = tx.send(ScanMsg::Done(ctx));
        });
    }

    fn poll_scan(&mut self) {
        if self.rx.is_none() {
            return;
        }
        let mut finished: Option<ScanContext> = None;
        {
            let rx = self.rx.as_ref().unwrap();
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    ScanMsg::Log(l) => self.log.push(l),
                    ScanMsg::Game(g) => self.games.push(g),
                    ScanMsg::Done(ctx) => {
                        finished = Some(ctx);
                        break;
                    }
                }
            }
        }
        if let Some(ctx) = finished {
            self.ctx = Some(ctx);
            self.scanning = false;
            self.rx = None;
        }
        if self.rx.is_none() && !self.scanning {
            self.games.sort_by(|a, b| {
                a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase())
            });
            let linked = self.games.iter().filter(|g| g.status == LinkStatus::Linked).count();
            let broken = self
                .games
                .iter()
                .filter(|g| g.status != LinkStatus::Linked)
                .count();
            self.log.push(format!(
                "Scan complete: {} game(s) — {} linked, {} to relink",
                self.games.len(),
                linked,
                broken
            ));
        }
    }

    fn do_relink(&mut self, indices: Vec<usize>) {
        let Some(ctx) = &mut self.ctx else { return };
        if indices.is_empty() {
            return;
        }
        self.busy = true;
        let games: Vec<GameInfo> = indices.iter().map(|&i| self.games[i].clone()).collect();
        let refs: Vec<&GameInfo> = games.iter().collect();
        let logs = relink(ctx, &refs);
        self.log.extend(logs);
        ctx.reload_items();
        for &i in &indices {
            let status = compute_status(ctx, &self.games[i]);
            self.games[i].status = status;
        }
        self.log
            .push("Restart the Epic Games Launcher to take effect.".to_string());
        self.busy = false;
    }
}

impl eframe::App for EpicLinkerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_scan();
        // Keep polling while the background scan runs
        if self.scanning {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        let mut relink_idx: Option<usize> = None;

        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.heading("🎮 Epic Linker");
                ui.separator();
                if ui
                    .add_enabled(!self.scanning, egui::Button::new("🔄 Refresh"))
                    .clicked()
                {
                    self.start_scan();
                }
                let to_relink = self
                    .games
                    .iter()
                    .enumerate()
                    .filter(|(_, g)| g.status != LinkStatus::Linked)
                    .map(|(i, _)| i)
                    .collect::<Vec<_>>();
                if ui
                    .add_enabled(
                        !self.scanning && !self.busy && !to_relink.is_empty() && self.ctx.is_some(),
                        egui::Button::new(format!("🔗 Relink All ({})", to_relink.len())),
                    )
                    .clicked()
                {
                    self.do_relink(to_relink);
                }
                if self.scanning {
                    ui.spinner();
                    ui.label("Scanning…");
                }
            });
            ui.add_space(4.0);
        });

        egui::TopBottomPanel::bottom("log_panel")
            .resizable(true)
            .default_height(140.0)
            .min_height(60.0)
            .max_height(220.0)
            .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Log").strong(),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Clear").clicked() {
                        self.log.clear();
                    }
                });
            });
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in &self.log {
                        ui.monospace(line);
                    }
                });
            ui.add_space(2.0);
        });

        egui::SidePanel::left("dirs_panel")
            .resizable(true)
            .default_width(300.0)
            .show(ctx, |ui| {
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Library Directories").strong());
                ui.add_space(2.0);
                let mut remove: Option<usize> = None;
                for (i, d) in self.dirs.iter().enumerate() {
                    let available = std::path::Path::new(&d.path).is_dir();
                    ui.horizontal(|ui| {
                        if d.is_default {
                            ui.label("🔒");
                            ui.add_enabled(
                                false,
                                egui::Label::new(egui::RichText::new(&d.path).weak()),
                            )
                            .on_hover_text("Default Epic Games install directory");
                        } else if available {
                            ui.label("📁");
                            ui.label(&d.path);
                            if ui.small_button("✖").on_hover_text("Remove").clicked() {
                                remove = Some(i);
                            }
                        } else {
                            ui.label(
                                egui::RichText::new("⚠️").color(egui::Color32::YELLOW),
                            )
                            .on_hover_text(
                                "Directory not available (drive disconnected?) — kept for later",
                            );
                            ui.label(egui::RichText::new(&d.path).weak());
                            if ui.small_button("✖").on_hover_text("Remove").clicked() {
                                remove = Some(i);
                            }
                        }
                    });
                }
                if let Some(i) = remove {
                    let p = self.dirs[i].path.clone();
                    self.dirs.remove(i);
                    self.cfg.extra_dirs.retain(|d| !d.eq_ignore_ascii_case(&p));
                    save_config(&self.cfg);
                }
                ui.add_space(4.0);
                if ui.button("➕ Add directory…").clicked() && !self.scanning {
                    if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                        let p = folder.to_string_lossy().into_owned();
                        if !self.dirs.iter().any(|d| d.path.eq_ignore_ascii_case(&p)) {
                            self.dirs.push(DirSpec {
                                path: p.clone(),
                                is_default: false,
                            });
                            self.cfg.extra_dirs.push(p);
                            save_config(&self.cfg);
                        }
                    }
                }
                ui.add_space(4.0);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            let linked = self.games.iter().filter(|g| g.status == LinkStatus::Linked).count();
            let needs = self
                .games
                .iter()
                .filter(|g| g.status == LinkStatus::NeedsRelink)
                .count();
            let missing = self
                .games
                .iter()
                .filter(|g| g.status == LinkStatus::NotLinked)
                .count();
            ui.label(
                egui::RichText::new(format!(
                    "{} game(s)   ✅ {} linked   ⚠️ {} to relink   ❌ {} not linked",
                    self.games.len(),
                    linked,
                    needs,
                    missing
                ))
                .strong(),
            );
            ui.add_space(4.0);

            // Snapshot rows to avoid borrow conflicts with the relink button
            let rows: Vec<(LinkStatus, String, String, String, String, String, String, bool)> =
                self.games
                    .iter()
                    .map(|g| {
                        (
                            g.status.clone(),
                            g.display_name.clone(),
                            g.game_dir.to_string_lossy().into_owned(),
                            g.version.clone(),
                            g.app_name.clone(),
                            g.catalog_item.clone(),
                            human_size(g.install_size),
                            g.catalog_item.is_empty(),
                        )
                    })
                    .collect();

            let text_height = egui::TextStyle::Body.resolve(ui.style()).size + 6.0;
            TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
                .column(Column::auto()) // status
                .column(Column::remainder().at_least(140.0)) // name
                .column(Column::remainder().at_least(220.0)) // path
                .column(Column::remainder().at_least(90.0)) // version
                .column(Column::remainder().at_least(120.0)) // app name
                .column(Column::remainder().at_least(120.0)) // item id
                .column(Column::remainder().at_least(70.0)) // size
                .column(Column::auto()) // action
                .header(20.0, |mut header| {
                    for label in [
                        "Status", "Name", "Path", "Version", "App Name", "Item ID", "Size", "",
                    ] {
                        header.col(|ui| {
                            ui.label(egui::RichText::new(label).strong());
                        });
                    }
                })
                .body(|body| {
                    body.rows(text_height, rows.len(), |mut row| {
                        let i = row.index();
                        let (status, name, path, version, app, item_id, size, no_id) =
                            &rows[i];
                        row.col(|ui| {
                            let (emoji, color, tooltip) = match status {
                                LinkStatus::Linked => ("✅", egui::Color32::GREEN, "Linked to Epic Games"),
                                LinkStatus::NeedsRelink => ("⚠️", egui::Color32::GOLD, "Outdated manifest — needs relink"),
                                LinkStatus::NotLinked => ("❌", egui::Color32::RED, "Not linked to Epic Games"),
                            };
                            ui.label(
                                egui::RichText::new(emoji).color(color),
                            )
                            .on_hover_text(tooltip);
                        });
                        row.col(|ui| {
                            ui.label(name).on_hover_text(path.clone());
                        });
                        row.col(|ui| {
                            let short = if path.len() > 60 {
                                format!("…{}", &path[path.len() - 59..])
                            } else {
                                path.clone()
                            };
                            ui.label(egui::RichText::new(short).weak())
                                .on_hover_text(path.clone());
                        });
                        row.col(|ui| {
                            ui.label(version);
                        });
                        row.col(|ui| {
                            ui.label(app);
                        });
                        row.col(|ui| {
                            let text = if item_id.is_empty() {
                                "unknown".to_string()
                            } else {
                                item_id.clone()
                            };
                            let mut rt = egui::RichText::new(text);
                            if *no_id {
                                rt = rt.color(egui::Color32::RED);
                            }
                            ui.label(rt);
                        });
                        row.col(|ui| {
                            ui.label(size);
                        });
                        row.col(|ui| {
                            if ui
                                .add_enabled(
                                    *status != LinkStatus::Linked && !self.busy && !self.scanning,
                                    egui::Button::new("Relink"),
                                )
                                .clicked()
                            {
                                relink_idx = Some(i);
                            }
                        });
                    });
                });
        });

        if let Some(i) = relink_idx {
            self.do_relink(vec![i]);
            ctx.request_repaint();
        }
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1180.0, 720.0])
            .with_min_inner_size([900.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Epic Linker",
        options,
        Box::new(|cc| Ok(Box::new(EpicLinkerApp::new(cc)))),
    )
}
