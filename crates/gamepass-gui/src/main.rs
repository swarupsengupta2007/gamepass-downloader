//! Game Pass downloader: finish a paused Xbox app install and hand the files
//! back to the Xbox app. The `xodus-cli` commands are built in: run with a
//! command (`gamepass-downloader queue --json`) this exe is the CLI, and the
//! window runs its own copy that way for each job.
//!
//! 1. Queue a game in the Xbox app and pause it.
//! 2. It shows up here; Download fetches it with `streaming`.
//! 3. Send to Xbox app runs `adopt` as administrator.
//! 4. Resume installation in the Xbox app finishes the install without a download.

#![cfg_attr(not(test), windows_subsystem = "windows")]

mod cli;
mod icon;
mod markets;
mod settings;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, CornerRadius, Frame, Margin, RichText, Stroke};

const XBOX_GREEN: Color32 = Color32::from_rgb(0x10, 0x7C, 0x10);
const TEXT: Color32 = Color32::from_rgb(0xF2, 0xF3, 0xF5);
const MUTED: Color32 = Color32::from_rgb(0x9A, 0xA0, 0xA6);
const BACKGROUND: Color32 = Color32::from_rgb(0x17, 0x18, 0x1B);
const HEADER: Color32 = Color32::from_rgb(0x1E, 0x20, 0x23);
const CARD: Color32 = Color32::from_rgb(0x24, 0x26, 0x2A);
const CARD_EDGE: Color32 = Color32::from_rgb(0x33, 0x36, 0x3B);
const WARN: Color32 = Color32::from_rgb(0xE8, 0xA3, 0x3D);
const ERROR: Color32 = Color32::from_rgb(0xE8, 0x5D, 0x5D);

fn main() -> ExitCode {
    if std::env::args_os().len() > 1 {
        attach_console();
        return xodus_cli::run(std::env::args_os());
    }
    match window() {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}

/// A windows-subsystem exe started from a terminal has no stdout or stderr.
/// Write to that terminal, unless the window gave this copy pipes.
#[cfg(windows)]
fn attach_console() {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };

    let missing = |id| {
        let handle = unsafe { GetStdHandle(id) };
        handle.is_null() || handle == INVALID_HANDLE_VALUE
    };
    if !missing(STD_OUTPUT_HANDLE) && !missing(STD_ERROR_HANDLE) {
        return;
    }
    if unsafe { AttachConsole(ATTACH_PARENT_PROCESS) } == 0 {
        return;
    }
    let name: Vec<u16> = "CONOUT$".encode_utf16().chain(Some(0)).collect();
    let out = unsafe {
        CreateFileW(
            name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if out == INVALID_HANDLE_VALUE {
        return;
    }
    for id in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        if missing(id) {
            unsafe { SetStdHandle(id, out) };
        }
    }
}

#[cfg(not(windows))]
fn attach_console() {}

fn window() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Game Pass Downloader")
            .with_icon(icon())
            .with_inner_size([900.0, 640.0])
            .with_min_inner_size([680.0, 440.0]),
        ..Default::default()
    };
    eframe::run_native("Game Pass Downloader", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

/// State the background threads fill in.
#[derive(Default)]
struct Shared {
    queue: Option<Result<cli::Queue, String>>,
    account: Option<Result<Option<String>, String>>,
    /// `None` while it loads, and while signed out.
    profile: Option<Result<cli::Profile, String>>,
    signing_out: bool,
    sign_out_error: Option<String>,
}

/// Download speed over the last few seconds. `streaming` reports a 200 ms
/// rate, which jumps when a resumed run credits files already on disk.
#[derive(Default)]
struct RateMeter {
    samples: VecDeque<(Instant, u64)>,
}

impl RateMeter {
    const WINDOW: Duration = Duration::from_secs(10);

    fn update(&mut self, completed: u64) -> u64 {
        let now = Instant::now();
        if self.samples.back().is_some_and(|&(_, last)| completed < last) {
            self.samples.clear(); // a new run started counting again
        }
        if self.samples.back().is_none_or(|&(at, _)| now - at >= Duration::from_millis(250)) {
            self.samples.push_back((now, completed));
        }
        while self.samples.len() > 2 && now - self.samples[0].0 > Self::WINDOW {
            self.samples.pop_front();
        }
        match (self.samples.front(), self.samples.back()) {
            (Some(&(t0, b0)), Some(&(t1, b1))) if t1 > t0 => ((b1 - b0) as f64 / (t1 - t0).as_secs_f64()) as u64,
            _ => 0,
        }
    }
}

/// What this window started for one content id.
#[derive(Default)]
struct Game {
    download: Option<cli::Download>,
    adopt: Option<cli::Adopt>,
    rate: RateMeter,
    place_rate: RateMeter,
    /// Title and download folder, kept once the item leaves the queue.
    title: Option<String>,
    staging: Option<PathBuf>,
}

struct App {
    shared: Arc<Mutex<Shared>>,
    titles: Arc<Mutex<HashMap<String, Option<String>>>>,
    games: HashMap<String, Game>,
    market: String,
    /// Picked in the list (and saved); until then it follows the account's country.
    market_picked: bool,
    /// What `settings.json` holds now.
    saved: settings::Settings,
    /// The signed-in account choices are saved for; `None` while signed out.
    settings_for: Option<String>,
    /// The market list was open last frame (scroll to the choice on opening).
    market_list_open: bool,
    parallel: usize,
    /// Copy into the Xbox app's folder instead of moving; needs the space twice.
    keep_download: bool,
    login: Option<cli::Login>,
    /// Gamerpic texture and the gamertag it belongs to.
    avatar: Option<(String, egui::TextureHandle)>,
    account_open: bool,
    confirm_sign_out: bool,
    chip_rect: egui::Rect,
    notice: Option<(Color32, String)>,
    repaint: cli::Repaint,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        style(&cc.egui_ctx);
        let ctx = cc.egui_ctx.clone();
        let repaint: cli::Repaint = Arc::new(move || ctx.request_repaint());
        let shared = Arc::new(Mutex::new(Shared::default()));
        let poll = shared.clone();
        let poll_repaint = repaint.clone();
        std::thread::spawn(move || loop {
            let queue = cli::read_queue();
            poll.lock().unwrap().queue = Some(queue);
            poll_repaint();
            std::thread::sleep(Duration::from_secs(3));
        });
        // Applied once the stored account is known to be the one they belong to.
        let saved = settings::load();
        let app = Self {
            shared,
            titles: cli::titles_cache(),
            games: HashMap::new(),
            market: cli::default_market(),
            market_picked: false,
            market_list_open: false,
            parallel: 16,
            keep_download: false,
            saved,
            settings_for: None,
            login: None,
            avatar: None,
            account_open: false,
            confirm_sign_out: false,
            chip_rect: egui::Rect::NOTHING,
            notice: None,
            repaint,
        };
        app.refresh_account();
        app
    }

    /// Reads the stored account, then its Xbox profile.
    fn refresh_account(&self) {
        let shared = self.shared.clone();
        let repaint = self.repaint.clone();
        {
            let mut s = shared.lock().unwrap();
            s.account = None;
            s.profile = None;
        }
        std::thread::spawn(move || {
            let account = cli::account();
            let signed_in = matches!(account, Ok(Some(_)));
            shared.lock().unwrap().account = Some(account);
            repaint();
            if signed_in {
                let profile = cli::profile();
                shared.lock().unwrap().profile = Some(profile);
                repaint();
            }
        });
    }

    fn refresh_profile(&self) {
        let shared = self.shared.clone();
        let repaint = self.repaint.clone();
        shared.lock().unwrap().profile = None;
        std::thread::spawn(move || {
            let profile = cli::profile();
            shared.lock().unwrap().profile = Some(profile);
            repaint();
        });
    }

    fn start_login(&mut self) {
        self.notice = None;
        self.login = Some(cli::Login::start(self.repaint.clone()));
    }

    fn sign_out(&mut self) {
        self.account_open = false;
        self.confirm_sign_out = false;
        let shared = self.shared.clone();
        let repaint = self.repaint.clone();
        shared.lock().unwrap().signing_out = true;
        std::thread::spawn(move || {
            let result = cli::logout();
            let account = cli::account();
            let mut s = shared.lock().unwrap();
            s.signing_out = false;
            s.sign_out_error = result.err();
            s.account = Some(account);
            s.profile = None;
            drop(s);
            repaint();
        });
    }

    /// A download or hand-off this window started is still running.
    fn busy(&self) -> bool {
        self.games.values().any(|g| {
            g.download.as_ref().is_some_and(|d| d.state.lock().unwrap().finished.is_none())
                || g.adopt.as_ref().is_some_and(|a| a.state.lock().unwrap().finished.is_none())
        })
    }

    fn signing_in(&self) -> bool {
        self.login.as_ref().is_some_and(cli::Login::running)
    }

    fn title_for(&self, row: &cli::QueueRow) -> Option<String> {
        let mut titles = self.titles.lock().unwrap();
        if let Some(title) = titles.get(&row.content_id) {
            return title.clone();
        }
        titles.insert(row.content_id.clone(), None);
        let (cache, repaint, market) = (self.titles.clone(), self.repaint.clone(), self.market.clone());
        let (store, content) = (row.store_id.clone(), row.content_id.clone());
        std::thread::spawn(move || {
            let title = cli::title(&store, &content, &market);
            cache.lock().unwrap().insert(content, title);
            repaint();
        });
        None
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            logo(ui, 42.0);
            ui.add_space(6.0);
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.label(RichText::new("Game Pass Downloader").size(21.0).strong().color(TEXT));
                ui.label(RichText::new("Finish paused Xbox app installs").color(MUTED));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| self.account_chip(ui));
        });
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Store market").color(MUTED));
            let before = self.market.clone();
            let opening = !self.market_list_open;
            let country = self.account_country();
            let mut picked = false;
            let mut follow_account = false;
            let list = egui::ComboBox::from_id_salt("market")
                .width(190.0)
                .height(360.0)
                .selected_text(markets::name(&self.market).to_owned())
                .show_ui(ui, |ui| {
                    // Windows' region can be a market the list does not name.
                    if !markets::MARKETS.iter().any(|(code, _)| *code == self.market) {
                        let current = self.market.clone();
                        ui.selectable_value(&mut self.market, current.clone(), current);
                    }
                    for (code, name) in markets::MARKETS {
                        let row = ui.selectable_value(&mut self.market, (*code).to_owned(), *name);
                        picked |= row.clicked();
                        if opening && *code == before {
                            row.scroll_to_me(Some(egui::Align::Center));
                        }
                        ui.painter().text(
                            row.rect.right_center() - egui::vec2(8.0, 0.0),
                            egui::Align2::RIGHT_CENTER,
                            *code,
                            egui::FontId::proportional(13.0),
                            MUTED,
                        );
                    }
                });
            self.market_list_open = list.inner.is_some();
            list.response
                .on_hover_text("The Microsoft Store region your account buys from. It follows your account's country until you pick one; a pick is remembered.");
            // Only a signed-in account has a country to go back to.
            if self.market_picked && self.settings_for.is_some() {
                let hint = match &country {
                    Some(code) => format!("Go back to {} from your Xbox account", markets::name(code)),
                    None => "Go back to the country of your Xbox account".to_owned(),
                };
                follow_account = ui
                    .add(egui::Button::new(RichText::new("Use account's country").color(XBOX_GREEN).size(13.5)).frame(false))
                    .on_hover_text(hint)
                    .clicked();
            }
            // A pick is kept across restarts, even when it equals the account's country.
            if picked {
                self.market_picked = true;
            }
            if follow_account {
                self.market_picked = false;
                self.market = country.unwrap_or_else(cli::default_market);
            }
            ui.add_space(16.0);
            ui.label(RichText::new("Parallel downloads").color(MUTED));
            ui.add(egui::Slider::new(&mut self.parallel, 1..=32));
            ui.add_space(16.0);
            ui.checkbox(&mut self.keep_download, "Keep the download folder")
                .on_hover_text("Copy the files into the Xbox app's folder instead of moving them. Needs the disk space twice.");
        });
        self.save_settings(ui.ctx());
    }

    /// Follows sign-in and sign-out: restore the choices saved for the
    /// signed-in account, and forget them once nobody is signed in.
    fn sync_settings(&mut self) {
        let account = match &self.shared.lock().unwrap().account {
            Some(Ok(account)) => account.clone(),
            _ => return, // still reading, or unreadable: change nothing
        };
        match account {
            Some(user) if self.settings_for.as_deref() != Some(user.as_str()) => {
                if self.saved.account.as_deref() == Some(user.as_str()) {
                    let saved = self.saved.clone();
                    if let Some(market) = saved.market {
                        self.market = market;
                        self.market_picked = true;
                    }
                    self.parallel = saved.parallel.unwrap_or(self.parallel);
                    self.keep_download = saved.keep_download.unwrap_or(self.keep_download);
                } else {
                    // Downloads use the account's market: a pick made while
                    // signed out (or by another account) does not carry over.
                    self.market_picked = false;
                    self.market = cli::default_market();
                }
                self.settings_for = Some(user);
            }
            None if self.settings_for.is_some() || self.saved != settings::Settings::default() => {
                self.settings_for = None;
                self.saved = settings::Settings::default();
                settings::clear();
            }
            _ => {}
        }
    }

    /// Writes `settings.json` when a choice changed, while signed in, and not
    /// while the slider is still being dragged.
    fn save_settings(&mut self, ctx: &egui::Context) {
        let Some(account) = self.settings_for.clone() else { return };
        let now = settings::Settings {
            account: Some(account),
            market: self.market_picked.then(|| self.market.clone()),
            parallel: Some(self.parallel),
            keep_download: Some(self.keep_download),
        };
        if now == self.saved || ctx.input(|i| i.pointer.any_down()) {
            return;
        }
        if let Err(error) = settings::save(&now) {
            self.notice = Some((ERROR, error));
        }
        self.saved = now;
    }

    /// Top right: sign-in button, progress, or the signed-in account.
    fn account_chip(&mut self, ui: &mut egui::Ui) {
        let (account, profile, signing_out) = {
            let s = self.shared.lock().unwrap();
            (s.account.clone(), s.profile.clone(), s.signing_out)
        };
        if signing_out {
            ui.spinner();
            ui.label(RichText::new("Signing out").color(MUTED));
            return;
        }
        if self.signing_in() {
            if ui.button("Cancel").clicked()
                && let Some(login) = &self.login
            {
                login.cancel();
            }
            ui.spinner();
            ui.label(RichText::new("Finish signing in in the browser").color(MUTED));
            return;
        }
        match account {
            None => {
                ui.spinner();
            }
            Some(Err(error)) => {
                ui.label(RichText::new("Account unavailable").color(ERROR)).on_hover_text(error);
            }
            Some(Ok(None)) => {
                if ui.add(accent_button("Sign in")).clicked() {
                    self.start_login();
                }
            }
            Some(Ok(Some(user))) => {
                let profile = profile.as_ref().and_then(|p| p.as_ref().ok());
                let name = profile.map(|p| p.gamertag.as_str()).filter(|g| !g.is_empty()).unwrap_or(&user);
                let detail = match profile {
                    Some(p) => p.plan.as_deref().unwrap_or(&user).to_owned(),
                    None => user.clone(),
                };
                let ring = profile.and_then(|p| p.plan.as_ref()).map(|_| XBOX_GREEN);
                let texture = self.avatar.as_ref().map(|(_, t)| t.clone());
                let response = chip(ui, name, &detail, texture.as_ref(), ring, self.account_open);
                self.chip_rect = response.rect;
                if response.clicked() {
                    self.account_open = !self.account_open;
                    self.confirm_sign_out = false;
                }
            }
        }
    }

    /// The panel under the account chip: profile, plan, sign out.
    fn account_popup(&mut self, ctx: &egui::Context) {
        let (account, profile) = {
            let s = self.shared.lock().unwrap();
            (s.account.clone(), s.profile.clone())
        };
        let Some(Ok(Some(user))) = account else {
            self.account_open = false;
            return;
        };
        const WIDTH: f32 = 390.0;
        let busy = self.busy();
        let texture = self.avatar.as_ref().map(|(_, t)| t.clone());
        let pos = egui::pos2(self.chip_rect.right() - WIDTH, self.chip_rect.bottom() + 8.0);
        let area = egui::Area::new(egui::Id::new("account-popup"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .show(ctx, |ui| {
                Frame::popup(ui.style())
                    .fill(CARD)
                    .stroke(Stroke::new(1.0_f32, CARD_EDGE))
                    .corner_radius(CornerRadius::same(12))
                    .inner_margin(Margin::same(18))
                    .show(ui, |ui| {
                        ui.set_width(WIDTH - 36.0);
                        let ready = profile.as_ref().and_then(|p| p.as_ref().ok());
                        ui.horizontal(|ui| {
                            let (rect, _) = ui.allocate_exact_size(egui::vec2(68.0, 68.0), egui::Sense::hover());
                            let letter = ready.map(|p| p.gamertag.as_str()).unwrap_or(&user);
                            let ring = ready.and_then(|p| p.plan.as_ref()).map(|_| XBOX_GREEN);
                            avatar(ui, rect, texture.as_ref(), letter, ring);
                            ui.add_space(6.0);
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 3.0;
                                match &profile {
                                    None => {
                                        ui.horizontal(|ui| {
                                            ui.spinner();
                                            ui.label(RichText::new("Loading Xbox profile").color(MUTED));
                                        });
                                    }
                                    Some(Ok(p)) => {
                                        ui.label(RichText::new(&p.gamertag).size(20.0).strong().color(TEXT));
                                    }
                                    Some(Err(error)) => {
                                        ui.label(RichText::new("Xbox profile unavailable").color(WARN)).on_hover_text(error);
                                    }
                                }
                                ui.add(egui::Label::new(RichText::new(&user).color(MUTED)).truncate()).on_hover_text(&user);
                                if let Some(p) = ready {
                                    match &p.plan {
                                        Some(plan) => pill(ui, plan, Color32::WHITE, XBOX_GREEN),
                                        None => pill(ui, "No active Game Pass", WARN, Color32::from_rgb(0x3A, 0x30, 0x22)),
                                    }
                                }
                            });
                        });
                        ui.add_space(12.0);
                        ui.separator();
                        ui.add_space(6.0);
                        if self.confirm_sign_out {
                            ui.label(RichText::new("Sign out on this PC?").strong().color(TEXT));
                            ui.label(RichText::new("Downloads need the account, so you will have to sign in again before the next one.").color(MUTED));
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                if ui.add(danger_button("Sign out")).clicked() {
                                    self.sign_out();
                                }
                                if ui.button("Cancel").clicked() {
                                    self.confirm_sign_out = false;
                                }
                            });
                        } else {
                            ui.horizontal(|ui| {
                                if ui.add_enabled(profile.is_some(), egui::Button::new("Refresh profile")).clicked() {
                                    self.refresh_profile();
                                }
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    let out = ui
                                        .add_enabled(!busy, egui::Button::new(RichText::new("Sign out").color(ERROR)).stroke(Stroke::new(1.0_f32, ERROR)))
                                        .on_disabled_hover_text("Wait for the running download or hand-off to finish.");
                                    if out.clicked() {
                                        self.confirm_sign_out = true;
                                    }
                                });
                            });
                        }
                    });
            });
        let outside = ctx.input(|i| {
            i.pointer.any_pressed()
                && i.pointer.interact_pos().is_some_and(|p| !area.response.rect.contains(p) && !self.chip_rect.contains(p))
        });
        if outside || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.account_open = false;
            self.confirm_sign_out = false;
        }
    }

    /// Shown above the queue while no account is stored.
    fn sign_in_card(&mut self, ui: &mut egui::Ui) {
        let signing_in = self.signing_in();
        card(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(10.0);
                logo(ui, 60.0);
                ui.add_space(10.0);
                ui.label(RichText::new("Sign in to start downloading").size(21.0).strong().color(TEXT));
                ui.label(RichText::new("Use the Microsoft account that has Game Pass or owns the game.").color(MUTED));
                ui.label(RichText::new("Sign-in opens in Edge or Chrome, so a passkey or the Authenticator app works.").color(MUTED));
                ui.add_space(12.0);
                if signing_in {
                    ui.spinner();
                    ui.label("Waiting for you to finish in the browser window");
                    if ui.button("Cancel").clicked()
                        && let Some(login) = &self.login
                    {
                        login.cancel();
                    }
                } else if ui.add(accent_button("Sign in with Microsoft").min_size(egui::vec2(240.0, 40.0))).clicked() {
                    self.start_login();
                }
                ui.add_space(10.0);
            });
        });
    }

    fn game_card(&mut self, ui: &mut egui::Ui, row: &cli::QueueRow) {
        let title = self.title_for(row);
        let game = self.games.entry(row.content_id.clone()).or_default();
        if title.is_some() {
            game.title = title.clone();
        }
        let staging = row.install_dir.as_deref().map(|dir| cli::staging_dir(dir, &row.content_id));
        if staging.is_some() {
            game.staging = staging.clone();
        }
        card(ui, |ui| {
            let xbox_state = if row.xbox_is_downloading() { "Downloading" } else { row.state.as_str() };
            ui.horizontal(|ui| {
                ui.label(RichText::new(title.as_deref().unwrap_or(&row.store_id)).size(18.0).strong().color(TEXT));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (fg, bg) = match xbox_state {
                        "Downloading" | "Running" => (Color32::WHITE, XBOX_GREEN),
                        "Paused" => (WARN, Color32::from_rgb(0x3A, 0x30, 0x22)),
                        "Failed" => (ERROR, Color32::from_rgb(0x3D, 0x24, 0x24)),
                        _ => (MUTED, Color32::from_rgb(0x30, 0x33, 0x38)),
                    };
                    pill(ui, &format!("Xbox app · {xbox_state}"), fg, bg);
                });
            });
            ui.label(RichText::new(format!(
                "{} of {}{}",
                bytes(row.streamed_bytes),
                bytes(row.total_bytes),
                row.install_dir.as_deref().map(|d| format!(" · {d}")).unwrap_or_default(),
            )).color(MUTED));
            ui.add_space(8.0);
            let Some(staging) = staging else {
                ui.label(RichText::new(row.not_ready_reason.as_deref().unwrap_or("Waiting for the Xbox app")).color(WARN));
                return;
            };
            self.stage(ui, row, &staging);
        });
    }

    fn stage(&mut self, ui: &mut egui::Ui, row: &cli::QueueRow, staging: &PathBuf) {
        let repaint = self.repaint.clone();
        let (market, parallel, keep_download) = (self.market.clone(), self.parallel, self.keep_download);
        let game = self.games.get_mut(&row.content_id).expect("card created the entry");

        if let Some(adopt) = &game.adopt {
            let state = adopt.state.lock().unwrap().clone();
            match state.finished {
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(&state.message);
                    });
                    if state.total > 0 {
                        let rate = game.place_rate.update(state.done_bytes);
                        placing_bar(ui, &state, rate);
                    }
                }
                Some(Ok(_)) => {
                    ui.label(RichText::new("Handed to the Xbox app.").color(XBOX_GREEN).strong());
                    ui.label("In the Xbox app install queue, open this game's … menu and choose Resume installation. It downloads only the game's executables, which the Xbox app has to write itself, then finishes.");
                    if ui.add(accent_button("Open Xbox app")).clicked() {
                        cli::open_xbox_app();
                    }
                }
                Some(Err(error)) => {
                    ui.label(RichText::new(error).color(ERROR));
                    if ui.button("Try again").clicked() {
                        game.adopt = None;
                    }
                }
            }
            return;
        }

        if let Some(download) = &game.download {
            let state = download.state.lock().unwrap().clone();
            match &state.finished {
                None => {
                    let rate = game.rate.update(state.completed);
                    progress_bar(ui, &state, rate);
                    ui.horizontal(|ui| {
                        if state.can_pause && !state.paused && ui.button("Pause").clicked() {
                            download.pause();
                        }
                        if state.paused && ui.add(accent_button("Resume")).clicked() {
                            download.resume();
                        }
                    });
                    return;
                }
                // Another process took the folder over; show that one instead.
                Some(Err(_)) if cli::staging_state(staging) == cli::Staging::Busy => game.download = None,
                Some(Err(error)) => {
                    ui.label(RichText::new(format!("Download stopped: {error}")).color(ERROR));
                    if ui.add(accent_button("Resume download")).clicked() {
                        start_download(game, row, staging, &market, parallel, repaint, &mut self.notice);
                    }
                    return;
                }
                Some(Ok(())) => {}
            }
        }

        match cli::staging_state(staging) {
            cli::Staging::Finished => {
                ui.label(RichText::new(format!("Downloaded to {}", staging.display())).color(XBOX_GREEN));
                if row.xbox_is_downloading() {
                    ui.label(RichText::new("The Xbox app is downloading this game. Pause it in the install queue first.").color(WARN));
                }
                let ready = row.ready && !row.xbox_is_downloading();
                let send = ui.add_enabled(ready, accent_button("Send to Xbox app"))
                    .on_hover_text("Moves the files into the Xbox app's install folder. Asks for administrator approval.");
                if !row.ready && let Some(reason) = &row.not_ready_reason {
                    ui.label(RichText::new(reason).color(WARN));
                }
                if send.clicked() {
                    game.adopt = Some(cli::Adopt::start(&row.content_id, staging, keep_download, repaint));
                }
            }
            cli::Staging::Busy => {
                match cli::read_status(staging) {
                    Some(state) => {
                        let rate = game.rate.update(state.completed);
                        progress_bar(ui, &state, rate);
                    }
                    None => {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Downloading");
                        });
                    }
                }
                ui.label(RichText::new(format!("Started from another window · {}", staging.display())).color(MUTED));
            }
            state => {
                let label = if state == cli::Staging::Partial { "Resume download" } else { "Download" };
                ui.horizontal(|ui| {
                    if ui.add(accent_button(label)).clicked() {
                        start_download(game, row, staging, &market, parallel, repaint, &mut self.notice);
                    }
                    ui.label(RichText::new(format!("to {}", staging.display())).color(MUTED));
                });
            }
        }
    }

    /// Games this window handed off that Gaming Services has since installed.
    fn installed_cards(&mut self, ui: &mut egui::Ui, queue: &cli::Queue) {
        let mut remove = None;
        for (id, game) in &self.games {
            let in_queue = queue.rows.iter().any(|r| &r.content_id == id);
            if in_queue || game.adopt.is_none() || !queue.installed.contains(id) {
                continue;
            }
            card(ui, |ui| {
                ui.label(RichText::new(game.title.as_deref().unwrap_or(id)).size(18.0).strong());
                ui.label(RichText::new("Installed. It is ready to play in the Xbox app.").color(XBOX_GREEN).strong());
                ui.horizontal(|ui| {
                    if ui.add(accent_button("Open Xbox app")).clicked() {
                        cli::open_xbox_app();
                    }
                    if let Some(dir) = &game.staging
                        && dir.exists()
                        && ui.button("Delete download leftovers").on_hover_text(dir.display().to_string()).clicked()
                    {
                        remove = Some(dir.clone());
                    }
                });
            });
        }
        if let Some(dir) = remove {
            self.notice = Some(match cli::remove_download(&dir) {
                Ok(()) => (MUTED, format!("Removed {}", dir.display())),
                Err(error) => (ERROR, error),
            });
        }
    }
}

fn start_download(
    game: &mut Game,
    row: &cli::QueueRow,
    staging: &PathBuf,
    market: &str,
    parallel: usize,
    repaint: cli::Repaint,
    notice: &mut Option<(Color32, String)>,
) {
    match cli::Download::start(&row.store_id, &row.content_id, staging, market, parallel, repaint) {
        Ok(download) => game.download = Some(download),
        Err(error) => *notice = Some((ERROR, error)),
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let login_result = self.login.as_ref().and_then(|l| l.result.lock().unwrap().take());
        if let Some(result) = login_result {
            if let Err(error) = result {
                self.notice = Some((ERROR, error));
            }
            self.login = None;
            self.refresh_account();
        }
        if let Some(error) = self.shared.lock().unwrap().sign_out_error.take() {
            self.notice = Some((ERROR, error));
        }
        self.sync_settings();
        self.sync_avatar(ctx);
        let signed_out = matches!(self.shared.lock().unwrap().account, Some(Ok(None)));

        egui::TopBottomPanel::top("top")
            .frame(Frame::new().inner_margin(Margin { left: 20, right: 20, top: 16, bottom: 14 }).fill(HEADER).stroke(Stroke::new(1.0_f32, CARD_EDGE)))
            .show(ctx, |ui| self.top_bar(ui));
        egui::CentralPanel::default().frame(Frame::new().fill(BACKGROUND).inner_margin(Margin::same(20))).show(ctx, |ui| {
            if let Some((color, text)) = self.notice.clone() {
                Frame::new()
                    .fill(color.gamma_multiply(0.12))
                    .stroke(Stroke::new(1.0_f32, color.gamma_multiply(0.5)))
                    .corner_radius(CornerRadius::same(8))
                    .inner_margin(Margin::symmetric(12, 8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(text).color(color));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.add(egui::Button::new(RichText::new("✕").color(MUTED)).frame(false)).clicked() {
                                    self.notice = None;
                                }
                            });
                        });
                    });
                ui.add_space(10.0);
            }
            let queue = self.shared.lock().unwrap().queue.clone();
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| match queue {
                None => {
                    ui.spinner();
                }
                Some(Err(error)) => {
                    ui.label(RichText::new(format!("Cannot read the Xbox app queue: {error}")).color(ERROR));
                }
                Some(Ok(queue)) => {
                    if signed_out {
                        self.sign_in_card(ui);
                    }
                    self.installed_cards(ui, &queue);
                    if queue.rows.is_empty() {
                        card(ui, |ui| {
                            ui.label(RichText::new("Nothing queued").size(18.0).strong().color(TEXT));
                            ui.label(RichText::new("In the Xbox app, install a game and pause it in the install queue. It shows up here.").color(MUTED));
                        });
                    }
                    for row in &queue.rows {
                        self.game_card(ui, row);
                    }
                }
            });
        });
        if self.account_open {
            self.account_popup(ctx);
        }
        // Keep progress and the queue fresh even without input.
        ctx.request_repaint_after(Duration::from_secs(1));
    }
}

impl App {
    fn account_country(&self) -> Option<String> {
        match &self.shared.lock().unwrap().profile {
            Some(Ok(profile)) => profile.country.clone(),
            _ => None,
        }
    }

    /// Upload the gamerpic once per gamertag; drop it when signed out. Also
    /// take the account's country as the market until one is picked.
    fn sync_avatar(&mut self, ctx: &egui::Context) {
        let s = self.shared.lock().unwrap();
        let Some(Ok(profile)) = &s.profile else {
            if !matches!(s.account, Some(Ok(Some(_)))) {
                self.avatar = None;
            }
            return;
        };
        if !self.market_picked
            && let Some(country) = &profile.country
        {
            self.market.clone_from(country);
        }
        if self.avatar.as_ref().is_some_and(|(tag, _)| tag == &profile.gamertag) {
            return;
        }
        self.avatar = profile
            .picture
            .clone()
            .map(|image| (profile.gamertag.clone(), ctx.load_texture("gamerpic", image, egui::TextureOptions::LINEAR)));
    }
}

fn style(ctx: &egui::Context) {
    ctx.set_visuals(egui::Visuals::dark());
    ctx.style_mut(|style| {
        style.visuals.panel_fill = BACKGROUND;
        style.visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(0x30, 0x33, 0x38);
        style.visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x3A, 0x3D, 0x43);
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(14.0, 6.0);
        style.visuals.selection.bg_fill = XBOX_GREEN;
        style.visuals.widgets.hovered.corner_radius = CornerRadius::same(4);
        style.visuals.widgets.inactive.corner_radius = CornerRadius::same(4);
        style.visuals.widgets.active.corner_radius = CornerRadius::same(4);
        for (_, font) in style.text_styles.iter_mut() {
            font.size += 1.5;
        }
    });
}

/// Status, size, speed and time left of one download. `rate` is smoothed.
fn progress_bar(ui: &mut egui::Ui, state: &cli::DownloadState, rate: u64) {
    let fraction = state.total.map(|t| state.completed as f32 / t.max(1) as f32).unwrap_or(0.0);
    let of_total = format!("{} of {}", bytes(state.completed), state.total.map(bytes).unwrap_or_else(|| "?".into()));
    let text = if state.paused {
        format!("Paused · {of_total}")
    } else if state.phase == "downloading" {
        let eta = state.total.filter(|_| rate > 0).map(|t| {
            let secs = t.saturating_sub(state.completed) / rate;
            format!(" · {} left", duration(secs))
        });
        format!("Downloading · {of_total} · {}/s{}", bytes(rate), eta.unwrap_or_default())
    } else if state.phase == "metadata" || state.phase == "initializing" || state.phase == "starting" {
        "Preparing · reading the package and checking files already downloaded".into()
    } else {
        capitalize(&state.phase)
    };
    ui.add(egui::ProgressBar::new(fraction).text(text).fill(XBOX_GREEN).animate(!state.paused));
}

/// Copy or move into the Xbox app's folder, by bytes: a few huge archives
/// up front make a file count misleading.
fn placing_bar(ui: &mut egui::Ui, state: &cli::AdoptState, rate: u64) {
    let left_bytes = state.total_bytes.saturating_sub(state.done_bytes);
    let left_files = state.total.saturating_sub(state.done);
    let fraction = if state.total_bytes > 0 {
        state.done_bytes as f32 / state.total_bytes as f32
    } else {
        state.done as f32 / state.total.max(1) as f32
    };
    let mut text = format!("{} left · {} files left", bytes(left_bytes), group(left_files));
    if rate > 0 && left_bytes > 0 {
        text += &format!(" · {}/s · {} left", bytes(rate), duration(left_bytes / rate));
    }
    ui.add(egui::ProgressBar::new(fraction).text(text).fill(XBOX_GREEN));
    ui.label(RichText::new(format!(
        "{} of {} · {} of {} files",
        bytes(state.done_bytes), bytes(state.total_bytes), group(state.done), group(state.total)
    )).color(MUTED));
}

/// 45651 → "45,651"
fn group(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn duration(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, secs % 3600 / 60),
    }
}

fn card(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui)) {
    Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0_f32, CARD_EDGE))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(18))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            body(ui);
        });
    ui.add_space(12.0);
}

fn accent_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(text).color(Color32::WHITE).strong()).fill(XBOX_GREEN)
}

fn danger_button(text: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(text).color(Color32::WHITE).strong()).fill(Color32::from_rgb(0xB3, 0x3A, 0x3A))
}

/// A small rounded label: plan, queue state.
fn pill(ui: &mut egui::Ui, text: &str, fg: Color32, bg: Color32) {
    Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::symmetric(9, 2))
        .show(ui, |ui| ui.label(RichText::new(text).size(13.0).strong().color(fg)));
}

/// Round gamerpic, or the account's initial on green while there is none.
fn avatar(ui: &egui::Ui, rect: egui::Rect, texture: Option<&egui::TextureHandle>, name: &str, ring: Option<Color32>) {
    let radius = rect.width() / 2.0;
    match texture {
        Some(texture) => {
            egui::Image::new(texture).corner_radius(CornerRadius::same(radius as u8)).paint_at(ui, rect);
        }
        None => {
            ui.painter().circle_filled(rect.center(), radius, Color32::from_rgb(0x2C, 0x5E, 0x2C));
            let letter = name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default();
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, letter, egui::FontId::proportional(radius), TEXT);
        }
    }
    if let Some(ring) = ring {
        ui.painter().circle_stroke(rect.center(), radius + 2.0, Stroke::new(2.0_f32, ring));
    }
}

/// The signed-in account as a clickable pill: gamerpic, gamertag, plan.
fn chip(
    ui: &mut egui::Ui,
    name: &str,
    detail: &str,
    texture: Option<&egui::TextureHandle>,
    ring: Option<Color32>,
    open: bool,
) -> egui::Response {
    const HEIGHT: f32 = 46.0;
    const PICTURE: f32 = 32.0;
    let painter = ui.painter();
    let name = painter.layout_no_wrap(name.to_owned(), egui::FontId::proportional(15.5), TEXT);
    let detail = painter.layout_no_wrap(detail.to_owned(), egui::FontId::proportional(12.5), MUTED);
    let text_width = name.size().x.max(detail.size().x);
    let width = 8.0 + PICTURE + 10.0 + text_width + 12.0 + 10.0 + 14.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, HEIGHT), egui::Sense::click());
    let hovered = response.hovered() || open;
    let painter = ui.painter();
    painter.rect(
        rect,
        CornerRadius::same((HEIGHT / 2.0) as u8),
        if hovered { Color32::from_rgb(0x2E, 0x31, 0x36) } else { CARD },
        Stroke::new(1.0_f32, if hovered { Color32::from_rgb(0x4A, 0x4E, 0x55) } else { CARD_EDGE }),
        egui::StrokeKind::Inside,
    );
    let picture = egui::Rect::from_center_size(egui::pos2(rect.left() + 8.0 + PICTURE / 2.0, rect.center().y), egui::vec2(PICTURE, PICTURE));
    avatar(ui, picture, texture, name.text(), ring);
    let text_left = picture.right() + 10.0;
    let text_top = rect.center().y - (name.size().y + detail.size().y) / 2.0;
    let painter = ui.painter();
    painter.galley(egui::pos2(text_left, text_top), name.clone(), TEXT);
    painter.galley(egui::pos2(text_left, text_top + name.size().y), detail, MUTED);
    // Chevron, pointing up while the panel is open.
    let c = egui::pos2(rect.right() - 18.0, rect.center().y);
    let (dy, stroke) = (if open { -2.5 } else { 2.5 }, Stroke::new(1.6_f32, MUTED));
    painter.line_segment([c + egui::vec2(-4.5, -dy), c + egui::vec2(0.0, dy)], stroke);
    painter.line_segment([c + egui::vec2(0.0, dy), c + egui::vec2(4.5, -dy)], stroke);
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// The app mark: a download arrow into a tray on Xbox green.
fn logo(ui: &mut egui::Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same((size * 0.24) as u8), XBOX_GREEN);
    let at = |u: f32, v: f32| rect.min + egui::vec2(u * size, v * size);
    painter.line_segment([at(0.5, 0.2), at(0.5, 0.5)], Stroke::new(size * 0.11, Color32::WHITE));
    painter.add(egui::Shape::convex_polygon(vec![at(0.3, 0.45), at(0.7, 0.45), at(0.5, 0.68)], Color32::WHITE, Stroke::NONE));
    painter.line_segment([at(0.24, 0.8), at(0.76, 0.8)], Stroke::new(size * 0.09, Color32::WHITE));
}

/// The same mark as the window and taskbar icon.
fn icon() -> egui::IconData {
    const SIDE: u32 = 64;
    egui::IconData { rgba: icon::rgba(SIDE), width: SIDE, height: SIDE }
}

/// Sizes the way the Xbox app shows them (1024-based, labelled GB).
fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{n} B") } else { format!("{value:.1} {}", UNITS[unit]) }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
}
