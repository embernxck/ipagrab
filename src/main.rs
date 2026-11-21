#![windows_subsystem = "windows"]

mod config;
mod device;
mod ipatool;
mod store;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText, TextureHandle, Vec2};

use config::Config;
use device::Device;
use store::AppItem;

fn main() -> eframe::Result {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("IPA Grab")
            .with_inner_size([920.0, 680.0])
            .with_min_inner_size([640.0, 480.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native("IPA Grab", opts, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

// ---------- сообщения из фоновых потоков ----------

enum Msg {
    Account(Result<(String, String), String>),
    Login(Result<ipatool::LoginResult, String>),
    Search(Result<Vec<AppItem>, String>),
    Icon(u64, Option<egui::ColorImage>),
    JobStarted(u64),
    JobDone(u64, Result<PathBuf, String>),
    Devices(Result<Vec<Device>, String>),
    PhoneApps(String, Result<Vec<AppItem>, String>),
    IpaInfo(PathBuf, device::IpaInfo),
    InstallProgress(PathBuf, u64),
    InstallDone(PathBuf, Result<(), String>),
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Download,
    Install,
}

enum JobState {
    Queued,
    Running,
    Done(PathBuf),
    Failed(String),
}

struct Job {
    path: PathBuf,
    state: JobState,
    total: Option<u64>,
}

enum IconState {
    Loading,
    Ready(TextureHandle),
    Missing,
}

enum InstState {
    Idle,
    Queued,
    Copying,
    Installing(u64),
    Done,
    Failed(String),
}

struct Ipa {
    path: PathBuf,
    name: String,
    version: String,
    size: u64,
    icon: Option<TextureHandle>,
    picked: bool,
    state: InstState,
}

enum AuthState {
    Checking,
    LoggedOut,
    LoggedIn { name: String, email: String },
}

enum DevCmd {
    Install(String, Vec<PathBuf>),
    Apps(String),
}

struct DlTask {
    id: u64,
    path: PathBuf,
    free: bool,
}

struct App {
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    ctx: egui::Context,
    cfg: Config,
    passphrase: String,
    ipatool: Option<PathBuf>,
    tab: Tab,

    auth: AuthState,
    email: String,
    password: String,
    code: String,
    need_code: bool,
    login_busy: bool,
    login_err: Option<String>,

    query: String,
    searching: bool,
    search_err: Option<String>,
    results: Vec<AppItem>,
    phone_apps: HashMap<String, Result<Vec<AppItem>, String>>,
    selected: HashSet<u64>,
    icons: HashMap<u64, IconState>,
    jobs: HashMap<u64, Job>,
    dl_tx: Sender<DlTask>,

    ipas: Vec<Ipa>,
    devices: Vec<Device>,
    device_err: Option<String>,
    device_pick: usize,
    dev_tx: Sender<DevCmd>,
    installing: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext) -> Self {
        setup_style(&cc.egui_ctx);
        let (tx, rx) = channel();
        let mut cfg = Config::load();
        let passphrase = cfg.passphrase();
        let ipatool = ipatool::find_ipatool(cfg.ipatool.as_deref());
        let ctx = cc.egui_ctx.clone();

        let dl_tx = spawn_download_worker(tx.clone(), ctx.clone(), ipatool.clone(), passphrase.clone());
        let dev_tx = spawn_device_worker(tx.clone(), ctx.clone());

        let mut app = Self {
            tx,
            rx,
            ctx,
            email: cfg.email.clone(),
            cfg,
            passphrase,
            ipatool,
            tab: Tab::Download,
            auth: AuthState::Checking,
            password: String::new(),
            code: String::new(),
            need_code: false,
            login_busy: false,
            login_err: None,
            query: String::new(),
            searching: false,
            search_err: None,
            results: Vec::new(),
            phone_apps: HashMap::new(),
            selected: HashSet::new(),
            icons: HashMap::new(),
            jobs: HashMap::new(),
            dl_tx,
            ipas: Vec::new(),
            devices: Vec::new(),
            device_err: None,
            device_pick: 0,
            dev_tx,
            installing: false,
        };
        app.check_account();
        app
    }

    fn bg(&self, f: impl FnOnce() -> Msg + Send + 'static) {
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(f());
            ctx.request_repaint();
        });
    }

    fn check_account(&mut self) {
        let Some(exe) = self.ipatool.clone() else {
            self.auth = AuthState::LoggedOut;
            return;
        };
        self.auth = AuthState::Checking;
        let pp = self.passphrase.clone();
        self.bg(move || Msg::Account(ipatool::account_info(&exe, &pp)));
    }

    fn do_login(&mut self) {
        let Some(exe) = self.ipatool.clone() else { return };
        self.login_busy = true;
        self.login_err = None;
        self.cfg.email = self.email.trim().to_string();
        self.cfg.save();
        let (pp, e, p, c) = (self.passphrase.clone(), self.cfg.email.clone(), self.password.clone(), self.code.trim().to_string());
        self.bg(move || Msg::Login(ipatool::login(&exe, &pp, &e, &p, &c)));
    }

    fn do_search(&mut self) {
        let q = self.query.trim().to_string();
        if q.is_empty() {
            self.results.clear();
            return;
        }
        self.searching = true;
        self.search_err = None;
        self.bg(move || Msg::Search(store::search(&q)));
    }

    fn icon_for(&mut self, item: &AppItem) -> Option<TextureHandle> {
        match self.icons.get(&item.id) {
            Some(IconState::Ready(t)) => Some(t.clone()),
            Some(_) => None,
            None => {
                self.icons.insert(item.id, IconState::Loading);
                let (id, url) = (item.id, item.icon_url.clone());
                match url {
                    // из поиска App Store — иконка по ссылке
                    Some(u) => self.bg(move || Msg::Icon(id, store::icon(id, Some(u)))),
                    // с телефона — из кэша, а если его нет, телефон пришлёт сам
                    None => {
                        let (tx, ctx) = (self.tx.clone(), self.ctx.clone());
                        std::thread::spawn(move || {
                            if let Some(img) = store::cached_icon(id) {
                                let _ = tx.send(Msg::Icon(id, Some(img)));
                                ctx.request_repaint();
                            }
                        });
                    }
                }
                None
            }
        }
    }

    fn start_downloads(&mut self) {
        let out_dir = self.cfg.out_dir();
        let _ = std::fs::create_dir_all(&out_dir);
        let all: Vec<AppItem> = self
            .results
            .iter()
            .chain(self.phone_apps.values().flat_map(|r| r.iter().flatten()))
            .cloned()
            .collect();
        for id in self.selected.drain().collect::<Vec<_>>() {
            if matches!(self.jobs.get(&id), Some(Job { state: JobState::Queued | JobState::Running, .. })) {
                continue;
            }
            let Some(item) = all.iter().find(|i| i.id == id) else { continue };
            let path = out_dir.join(format!("{} {}.ipa", safe_name(&item.name), item.id));
            self.jobs.insert(id, Job { path: path.clone(), state: JobState::Queued, total: item.size });
            let _ = self.dl_tx.send(DlTask { id, path, free: item.free });
        }
    }

    fn add_ipas(&mut self, paths: Vec<PathBuf>) {
        for p in paths {
            if p.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("ipa")) != Some(true) {
                continue;
            }
            if self.ipas.iter().any(|i| i.path == p) {
                continue;
            }
            let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            let name = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            self.ipas.push(Ipa { path: p.clone(), name, version: String::new(), size, icon: None, picked: true, state: InstState::Idle });
            self.bg(move || {
                let info = device::ipa_info(&p);
                Msg::IpaInfo(p, info)
            });
        }
    }

    fn handle_messages(&mut self) {
        while let Ok(m) = self.rx.try_recv() {
            match m {
                Msg::Account(r) => {
                    self.auth = match r {
                        Ok((name, email)) => AuthState::LoggedIn { name, email },
                        Err(_) => AuthState::LoggedOut,
                    }
                }
                Msg::Login(r) => {
                    self.login_busy = false;
                    match r {
                        Ok(ipatool::LoginResult::NeedCode) => {
                            self.need_code = true;
                            self.login_err = None;
                        }
                        Ok(ipatool::LoginResult::Ok { name, email }) => {
                            self.auth = AuthState::LoggedIn { name, email };
                            self.password.clear();
                            self.code.clear();
                            self.need_code = false;
                        }
                        Err(e) => self.login_err = Some(e),
                    }
                }
                Msg::Search(r) => {
                    self.searching = false;
                    match r {
                        Ok(list) => self.results = list,
                        Err(e) => self.search_err = Some(e),
                    }
                }
                Msg::Icon(id, img) => {
                    let st = match img {
                        Some(img) => IconState::Ready(self.ctx.load_texture(format!("icon{id}"), img, egui::TextureOptions::LINEAR)),
                        None => IconState::Missing,
                    };
                    self.icons.insert(id, st);
                }
                Msg::JobStarted(id) => {
                    if let Some(j) = self.jobs.get_mut(&id) {
                        j.state = JobState::Running;
                    }
                }
                Msg::JobDone(id, r) => {
                    if let Some(j) = self.jobs.get_mut(&id) {
                        j.state = match r {
                            Ok(p) => JobState::Done(p),
                            Err(e) => JobState::Failed(e),
                        };
                    }
                }
                Msg::Devices(r) => match r {
                    Ok(d) => {
                        self.devices = d;
                        self.device_err = None;
                        if self.device_pick >= self.devices.len() {
                            self.device_pick = 0;
                        }
                    }
                    Err(e) => {
                        self.devices.clear();
                        self.device_err = Some(e);
                    }
                },
                Msg::PhoneApps(udid, r) => {
                    self.phone_apps.insert(udid, r);
                }
                Msg::IpaInfo(p, info) => {
                    if let Some(i) = self.ipas.iter_mut().find(|i| i.path == p) {
                        i.name = info.name;
                        i.version = info.version;
                        i.icon = info
                            .icon
                            .map(|img| self.ctx.load_texture(format!("ipa{}", p.display()), img, egui::TextureOptions::LINEAR));
                    }
                }
                Msg::InstallProgress(p, pct) => {
                    if let Some(i) = self.ipas.iter_mut().find(|i| i.path == p) {
                        i.state = if pct == u64::MAX { InstState::Copying } else { InstState::Installing(pct) };
                    }
                }
                Msg::InstallDone(p, r) => {
                    if let Some(i) = self.ipas.iter_mut().find(|i| i.path == p) {
                        i.state = match r {
                            Ok(()) => InstState::Done,
                            Err(e) => InstState::Failed(e),
                        };
                    }
                    self.installing = self
                        .ipas
                        .iter()
                        .any(|i| matches!(i.state, InstState::Queued | InstState::Copying | InstState::Installing(_)));
                }
            }
        }
    }

    // ---------- интерфейс ----------

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            ui.label(RichText::new("IPA Grab").size(20.0).strong());
            ui.add_space(16.0);
            for (t, label) in [(Tab::Download, "Скачать"), (Tab::Install, "Установить на айфон")] {
                if ui.selectable_label(self.tab == t, RichText::new(label).size(16.0)).clicked() {
                    self.tab = t;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let AuthState::LoggedIn { name, email } = &self.auth {
                    let (name, email) = (name.clone(), email.clone());
                    if ui.button("Выйти").clicked() {
                        if let Some(exe) = self.ipatool.clone() {
                            let pp = self.passphrase.clone();
                            std::thread::spawn(move || ipatool::logout(&exe, &pp));
                        }
                        self.auth = AuthState::LoggedOut;
                    }
                    let who = if name.is_empty() { email } else { format!("{name} · {email}") };
                    ui.label(RichText::new(who).color(Color32::GRAY));
                }
            });
        });
    }

    fn login_card(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(50.0);
            ui.set_max_width(360.0);
            ui.label(RichText::new("Вход в Apple ID").size(22.0).strong());
            ui.add_space(4.0);
            ui.label(RichText::new("Нужен, чтобы скачивать приложения из твоей истории покупок").color(Color32::GRAY));
            ui.add_space(16.0);

            if self.ipatool.is_none() {
                ui.colored_label(Color32::from_rgb(230, 120, 90), "Не найден ipatool.exe. Положи его рядом с программой или укажи вручную.");
                if ui.button("Указать ipatool.exe…").clicked() {
                    if let Some(p) = rfd::FileDialog::new().add_filter("ipatool", &["exe"]).pick_file() {
                        self.cfg.ipatool = Some(p.clone());
                        self.cfg.save();
                        self.ipatool = Some(p.clone());
                        self.dl_tx = spawn_download_worker(self.tx.clone(), self.ctx.clone(), Some(p), self.passphrase.clone());
                    }
                }
                return;
            }

            let busy = self.login_busy;
            ui.add_enabled_ui(!busy && !self.need_code, |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.email).hint_text("Почта Apple ID").desired_width(f32::INFINITY));
                ui.add_space(6.0);
                ui.add(egui::TextEdit::singleline(&mut self.password).password(true).hint_text("Пароль").desired_width(f32::INFINITY));
            });

            let mut submit = false;
            if self.need_code {
                ui.add_space(10.0);
                ui.label("Apple отправил код на твои устройства. Введи его:");
                let r = ui.add_enabled(
                    !busy,
                    egui::TextEdit::singleline(&mut self.code).hint_text("Код из 6 цифр").desired_width(f32::INFINITY),
                );
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = true;
                }
            }

            ui.add_space(12.0);
            let ready = !self.email.trim().is_empty() && !self.password.is_empty() && (!self.need_code || self.code.trim().len() >= 6);
            let label = if busy { "Входим… (первый раз до пары минут)" } else if self.need_code { "Подтвердить" } else { "Войти" };
            if ui
                .add_enabled(ready && !busy, egui::Button::new(RichText::new(label).size(16.0)).min_size(Vec2::new(360.0, 36.0)))
                .clicked()
            {
                submit = true;
            }
            if submit && ready && !busy {
                self.do_login();
            }
            if busy {
                ui.add_space(8.0);
                ui.spinner();
            }
            if self.need_code && !busy {
                ui.add_space(6.0);
                ui.label(
                    RichText::new("Код не пришёл? Apple шлёт его только при верном пароле. Проверь пароль и отправь ещё раз.")
                        .size(13.0)
                        .color(Color32::GRAY),
                );
                if ui.link("Отправить код ещё раз").clicked() {
                    self.code.clear();
                    self.do_login();
                }
                if ui.link("Изменить почту или пароль").clicked() {
                    self.need_code = false;
                    self.code.clear();
                }
            }
            if let Some(e) = &self.login_err {
                ui.add_space(8.0);
                ui.colored_label(Color32::from_rgb(230, 120, 90), e);
            }
        });
    }

    fn app_row(&mut self, ui: &mut egui::Ui, item: &AppItem) {
        let tex = self.icon_for(item);
        ui.horizontal(|ui| {
            ui.set_min_height(56.0);
            let mut on = self.selected.contains(&item.id);
            let job_busy = matches!(self.jobs.get(&item.id), Some(Job { state: JobState::Queued | JobState::Running, .. }));
            if ui.add_enabled(!job_busy, egui::Checkbox::without_text(&mut on)).changed() {
                if on {
                    self.selected.insert(item.id);
                } else {
                    self.selected.remove(&item.id);
                }
            }
            icon_box(ui, tex.as_ref());
            ui.vertical(|ui| {
                ui.add_space(6.0);
                ui.label(RichText::new(&item.name).size(16.0));
                ui.label(RichText::new(&item.subtitle).color(Color32::GRAY).size(13.0));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(job) = self.jobs.get(&item.id) {
                    job_status(ui, job);
                } else if !item.free {
                    ui.label(RichText::new("платное").color(Color32::GRAY));
                }
            });
        });
    }

    fn download_tab(&mut self, ui: &mut egui::Ui) {
        if !matches!(self.auth, AuthState::LoggedIn { .. }) {
            if matches!(self.auth, AuthState::Checking) {
                ui.vertical_centered(|ui| {
                    ui.add_space(80.0);
                    ui.spinner();
                    ui.label("Проверяю вход в Apple ID…");
                });
            } else {
                self.login_card(ui);
            }
            return;
        }

        ui.horizontal(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .hint_text("Фильтр по телефону, Enter — искать в App Store")
                    .desired_width(ui.available_width() - 90.0),
            );
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if ui.add_enabled(!self.searching, egui::Button::new("Найти")).clicked() || enter {
                self.do_search();
            }
        });
        if self.searching {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Ищу…");
            });
        }
        if let Some(e) = &self.search_err {
            ui.colored_label(Color32::from_rgb(230, 120, 90), e);
        }
        ui.add_space(6.0);

        // нижняя панель с кнопкой
        egui::Panel::bottom("dl_bottom").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let n = self.selected.len();
                if ui
                    .add_enabled(n > 0, egui::Button::new(RichText::new(format!("Скачать выбранные ({n})")).size(16.0)).min_size(Vec2::new(220.0, 34.0)))
                    .clicked()
                {
                    self.start_downloads();
                }
                if n > 0 && ui.button("Снять выбор").clicked() {
                    self.selected.clear();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Изменить папку").clicked() {
                        if let Some(p) = rfd::FileDialog::new().pick_folder() {
                            self.cfg.out_dir = Some(p);
                            self.cfg.save();
                        }
                    }
                    if ui.button("Открыть папку").clicked() {
                        let d = self.cfg.out_dir();
                        let _ = std::fs::create_dir_all(&d);
                        let _ = std::process::Command::new("explorer").arg(d).spawn();
                    }
                    ui.label(RichText::new(self.cfg.out_dir().display().to_string()).color(Color32::GRAY).size(12.0));
                });
            });
            ui.add_space(4.0);
        });

        // фильтр по мере ввода — по приложениям телефона
        let q = self.query.trim().to_lowercase();
        let phone = self.devices.get(self.device_pick).cloned();
        let phone_list: Option<Result<Vec<AppItem>, String>> = phone.as_ref().and_then(|d| self.phone_apps.get(&d.udid)).map(|r| {
            r.clone().map(|v| {
                v.into_iter()
                    .filter(|i| q.is_empty() || i.name.to_lowercase().contains(&q) || i.bundle.to_lowercase().contains(&q))
                    .collect()
            })
        });
        let results = self.results.clone();

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            if !results.is_empty() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Найдено в App Store").size(14.0).color(Color32::GRAY));
                    if ui.small_button("скрыть").clicked() {
                        self.results.clear();
                    }
                });
                for item in &results {
                    self.app_row(ui, item);
                }
                ui.add_space(12.0);
            }

            match (&phone, phone_list) {
                (None, _) => {
                    ui.add_space(20.0);
                    ui.label(RichText::new("📱 Подключи айфон кабелем — тут появятся все приложения с него").size(15.0).color(Color32::GRAY));
                }
                (Some(d), None) => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(format!("Читаю приложения с «{}»…", d.name));
                    });
                }
                (Some(_), Some(Err(e))) => {
                    ui.colored_label(Color32::from_rgb(230, 120, 90), e);
                }
                (Some(d), Some(Ok(list))) => {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("На телефоне «{}» · {}", d.name, list.len())).size(14.0).color(Color32::GRAY));
                        if ui.small_button("выбрать все").clicked() {
                            for i in &list {
                                self.selected.insert(i.id);
                            }
                        }
                        if ui.small_button("обновить").clicked() {
                            self.phone_apps.remove(&d.udid);
                            let _ = self.dev_tx.send(DevCmd::Apps(d.udid.clone()));
                        }
                    });
                    for item in &list {
                        self.app_row(ui, item);
                    }
                    if list.is_empty() && !q.is_empty() {
                        ui.label("На телефоне такого нет. Нажми «Найти», чтобы поискать в App Store.");
                    }
                }
            }
        });
    }

    fn install_tab(&mut self, ui: &mut egui::Ui) {
        // телефон
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                if self.devices.is_empty() {
                    ui.label(RichText::new("📱").size(22.0));
                    ui.vertical(|ui| {
                        ui.label(RichText::new("Айфон не найден").size(16.0).strong());
                        let hint = self
                            .device_err
                            .clone()
                            .unwrap_or_else(|| "Подключи кабелем, разблокируй и нажми «Доверять» на телефоне".into());
                        ui.label(RichText::new(hint).color(Color32::GRAY));
                    });
                } else {
                    ui.label(RichText::new("📱").size(22.0));
                    if self.devices.len() == 1 {
                        ui.label(RichText::new(&self.devices[0].name).size(16.0).strong());
                    } else {
                        egui::ComboBox::from_id_salt("dev")
                            .selected_text(self.devices[self.device_pick].name.clone())
                            .show_ui(ui, |ui| {
                                for (i, d) in self.devices.iter().enumerate() {
                                    ui.selectable_value(&mut self.device_pick, i, &d.name);
                                }
                            });
                    }
                    ui.label(RichText::new("подключён").color(Color32::from_rgb(110, 200, 130)));
                }
            });
        });
        ui.add_space(8.0);

        // перетаскивание файлов в окно
        let dropped: Vec<PathBuf> = ui.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        if !dropped.is_empty() {
            self.add_ipas(dropped);
        }

        egui::Panel::bottom("inst_bottom").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let picked: Vec<PathBuf> = self.ipas.iter().filter(|i| i.picked).map(|i| i.path.clone()).collect();
                let can = !picked.is_empty() && !self.devices.is_empty() && !self.installing;
                if ui
                    .add_enabled(can, egui::Button::new(RichText::new(format!("Установить ({})", picked.len())).size(16.0)).min_size(Vec2::new(200.0, 34.0)))
                    .clicked()
                {
                    let udid = self.devices[self.device_pick].udid.clone();
                    for i in self.ipas.iter_mut().filter(|i| i.picked) {
                        i.state = InstState::Queued;
                    }
                    self.installing = true;
                    let _ = self.dev_tx.send(DevCmd::Install(udid, picked));
                }
                if ui.button("Добавить .ipa…").clicked() {
                    let start = self.cfg.out_dir();
                    if let Some(files) = rfd::FileDialog::new().add_filter("IPA", &["ipa"]).set_directory(start).pick_files() {
                        self.add_ipas(files);
                    }
                }
                if !self.installing && !self.ipas.is_empty() && ui.button("Очистить список").clicked() {
                    self.ipas.clear();
                }
            });
            ui.add_space(4.0);
        });

        if self.ipas.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(60.0);
                ui.label(RichText::new("Перетащи сюда .ipa или нажми «Добавить .ipa…»").size(16.0).color(Color32::GRAY));
            });
            return;
        }

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let busy = self.installing;
            for ipa in &mut self.ipas {
                ui.horizontal(|ui| {
                    ui.set_min_height(56.0);
                    ui.add_enabled(!busy, egui::Checkbox::without_text(&mut ipa.picked));
                    icon_box(ui, ipa.icon.as_ref());
                    ui.vertical(|ui| {
                        ui.add_space(6.0);
                        ui.label(RichText::new(&ipa.name).size(16.0));
                        let mut sub = format!("{:.0} МБ", ipa.size as f64 / 1_048_576.0);
                        if !ipa.version.is_empty() {
                            sub = format!("версия {} · {sub}", ipa.version);
                        }
                        ui.label(RichText::new(sub).size(13.0).color(Color32::GRAY));
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| match &ipa.state {
                        InstState::Idle => {}
                        InstState::Queued => {
                            ui.label(RichText::new("в очереди").color(Color32::GRAY));
                        }
                        InstState::Copying => {
                            ui.spinner();
                            ui.label("копирую на телефон…");
                        }
                        InstState::Installing(p) => {
                            ui.add(egui::ProgressBar::new(*p as f32 / 100.0).desired_width(160.0).text(format!("установка {p}%")));
                        }
                        InstState::Done => {
                            ui.label(RichText::new("✔ установлено").color(Color32::from_rgb(110, 200, 130)));
                        }
                        InstState::Failed(e) => {
                            ui.label(RichText::new(e).color(Color32::from_rgb(230, 120, 90)));
                        }
                    });
                });
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.handle_messages();
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(6.0);
            self.top_bar(ui);
            ui.add_space(6.0);
        });
        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Download => self.download_tab(ui),
            Tab::Install => self.install_tab(ui),
        });
        if self.jobs.values().any(|j| matches!(j.state, JobState::Running)) || self.login_busy || self.searching {
            self.ctx.request_repaint_after(Duration::from_millis(300));
        }
    }
}

// ---------- фоновые потоки ----------

fn spawn_download_worker(tx: Sender<Msg>, ctx: egui::Context, exe: Option<PathBuf>, pp: String) -> Sender<DlTask> {
    let (dtx, drx) = channel::<DlTask>();
    std::thread::spawn(move || {
        for t in drx {
            let _ = tx.send(Msg::JobStarted(t.id));
            ctx.request_repaint();
            let r = match &exe {
                Some(exe) => ipatool::download(exe, &pp, t.id, &t.path, t.free),
                None => Err("Не найден ipatool.exe".into()),
            };
            let _ = tx.send(Msg::JobDone(t.id, r));
            ctx.request_repaint();
        }
    });
    dtx
}

fn load_phone_apps(rt: &tokio::runtime::Runtime, tx: &Sender<Msg>, ctx: &egui::Context, udid: &str) {
    let r = rt.block_on(device::phone_apps(udid));
    // иконки, которых ещё нет в кэше, берём с телефона
    let need: Vec<(u64, String)> = r
        .iter()
        .flatten()
        .filter(|a| !store::icon_cache(a.id).exists())
        .map(|a| (a.id, a.bundle.clone()))
        .collect();
    let _ = tx.send(Msg::PhoneApps(udid.to_string(), r));
    ctx.request_repaint();
    let mut got: Vec<(u64, Vec<u8>)> = Vec::new();
    let _ = rt.block_on(device::phone_icons(udid, &need, |id, png| got.push((id, png))));
    // одинаковая картинка у нескольких приложений = заглушка телефона; такие берём из App Store
    let mut counts: HashMap<&[u8], usize> = HashMap::new();
    for (_, png) in &got {
        *counts.entry(png.as_slice()).or_default() += 1;
    }
    let mut fallback = Vec::new();
    for (id, png) in &got {
        if counts[png.as_slice()] > 1 {
            fallback.push(*id);
            continue;
        }
        store::save_icon(*id, png);
        let _ = tx.send(Msg::Icon(*id, store::decode_icon(png)));
        ctx.request_repaint();
    }
    for id in fallback {
        let (tx, ctx) = (tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            if let Some(img) = store::icon(id, None) {
                let _ = tx.send(Msg::Icon(id, Some(img)));
                ctx.request_repaint();
            }
        });
    }
}

fn spawn_device_worker(tx: Sender<Msg>, ctx: egui::Context) -> Sender<DevCmd> {
    let (itx, irx) = channel::<DevCmd>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let mut names = HashMap::new();
        let mut last: Option<Result<Vec<Device>, String>> = None;
        let mut seen: HashSet<String> = HashSet::new();
        let mut next_poll = Instant::now();
        loop {
            match irx.recv_timeout(Duration::from_millis(200)) {
                Ok(DevCmd::Apps(udid)) => load_phone_apps(&rt, &tx, &ctx, &udid),
                Ok(DevCmd::Install(udid, files)) => {
                for f in files {
                    let _ = tx.send(Msg::InstallProgress(f.clone(), u64::MAX));
                    ctx.request_repaint();
                    let (tx2, ctx2, f2) = (tx.clone(), ctx.clone(), f.clone());
                    let r = rt.block_on(device::install(&udid, &f, move |p| {
                        let _ = tx2.send(Msg::InstallProgress(f2.clone(), p));
                        ctx2.request_repaint();
                    }));
                    let _ = tx.send(Msg::InstallDone(f, r));
                    ctx.request_repaint();
                }
                // после установки список приложений телефона изменился
                load_phone_apps(&rt, &tx, &ctx, &udid);
                next_poll = Instant::now();
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                Err(_) => {}
            }
            if Instant::now() >= next_poll {
                let r = rt.block_on(device::list(&mut names));
                if last.as_ref() != Some(&r) {
                    let _ = tx.send(Msg::Devices(r.clone()));
                    ctx.request_repaint();
                    // новый телефон — сразу читаем его приложения
                    let fresh: Vec<String> = r.iter().flatten().map(|d| d.udid.clone()).filter(|u| !seen.contains(u)).collect();
                    seen = r.iter().flatten().map(|d| d.udid.clone()).collect();
                    last = Some(r);
                    for u in fresh {
                        load_phone_apps(&rt, &tx, &ctx, &u);
                    }
                }
                next_poll = Instant::now() + Duration::from_secs(2);
            }
        }
    });
    itx
}

// ---------- мелочи ----------

fn job_status(ui: &mut egui::Ui, job: &Job) {
    match &job.state {
        JobState::Queued => {
            ui.label(RichText::new("в очереди").color(Color32::GRAY));
        }
        JobState::Running => {
            let tmp = PathBuf::from(format!("{}.tmp", job.path.display()));
            let got = std::fs::metadata(&tmp).or_else(|_| std::fs::metadata(&job.path)).map(|m| m.len()).unwrap_or(0);
            let mb = got as f64 / 1_048_576.0;
            match job.total {
                Some(t) if t > 0 => {
                    let f = (got as f32 / t as f32).min(1.0);
                    ui.add(egui::ProgressBar::new(f).desired_width(180.0).text(format!("{mb:.0} / {:.0} МБ", t as f64 / 1_048_576.0)));
                }
                _ => {
                    ui.label(format!("{mb:.0} МБ"));
                    ui.spinner();
                }
            }
        }
        JobState::Done(p) => {
            if ui.small_button("Показать").clicked() {
                let _ = std::process::Command::new("explorer").arg(format!("/select,{}", p.display())).spawn();
            }
            ui.label(RichText::new("✔ скачано").color(Color32::from_rgb(110, 200, 130)));
        }
        JobState::Failed(e) => {
            ui.label(RichText::new(e).color(Color32::from_rgb(230, 120, 90)));
        }
    }
}

fn icon_box(ui: &mut egui::Ui, tex: Option<&TextureHandle>) {
    let size = Vec2::splat(48.0);
    match tex {
        Some(t) => {
            ui.add(egui::Image::new(t).fit_to_exact_size(size).corner_radius(11));
        }
        None => {
            let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
            ui.painter().rect_filled(rect, 11.0, Color32::from_gray(60));
        }
    }
}

fn safe_name(s: &str) -> String {
    s.chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) { '_' } else { c })
        .collect::<String>()
        .trim()
        .chars()
        .take(60)
        .collect()
}

fn setup_style(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    if let Ok(bytes) = std::fs::read("C:\\Windows\\Fonts\\segoeui.ttf") {
        fonts.font_data.insert("segoe".into(), egui::FontData::from_owned(bytes).into());
        fonts.families.entry(egui::FontFamily::Proportional).or_default().insert(0, "segoe".into());
    }
    if let Ok(bytes) = std::fs::read("C:\\Windows\\Fonts\\seguiemj.ttf") {
        fonts.font_data.insert("emoji".into(), egui::FontData::from_owned(bytes).into());
        fonts.families.entry(egui::FontFamily::Proportional).or_default().push("emoji".into());
    }
    ctx.set_fonts(fonts);
    ctx.set_visuals(egui::Visuals::dark());
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = Vec2::new(8.0, 6.0);
        s.spacing.button_padding = Vec2::new(12.0, 6.0);
        for (_, f) in s.text_styles.iter_mut() {
            f.size = (f.size * 1.08).round();
        }
    });
}
