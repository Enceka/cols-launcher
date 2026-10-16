use crate::inspection;
use crate::native::{self, LocalProfile};
use crate::profile::{Profile, resolve};
use crate::relay::Event;
use crate::session;
use anyhow::Result;
use eframe::egui;
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use tokio::sync::watch;

enum Message {
    Log(String),
    Finished(Result<(), String>),
    PlayerCreated(Result<PathBuf, String>),
}

struct App {
    config: String,
    executable: String,
    server: String,
    auth_port: String,
    lobby_port: String,
    show_free_notice: bool,
    native_mode: bool,
    player: crate::player::PlayerSettings,
    loaded_player_config: String,
    saved_players: Vec<crate::player::SavedPlayer>,
    status: String,
    report: String,
    log: String,
    sender: mpsc::Sender<Message>,
    receiver: mpsc::Receiver<Message>,
    stop: Option<watch::Sender<bool>>,
    worker: Option<JoinHandle<()>>,
}

impl App {
    fn new(
        context: &eframe::CreationContext<'_>,
        path: PathBuf,
        server: Option<String>,
        auth_port: Option<u16>,
        lobby_port: Option<u16>,
    ) -> Self {
        // Use a locally installed system font; never bundle proprietary game fonts.
        for path in [
            "/System/Library/Fonts/PingFang.ttc",
            "/System/Library/Fonts/LanguageSupport/PingFang.ttc",
            "/System/Library/Fonts/STHeiti Medium.ttc",
            "C:\\Windows\\Fonts\\msyh.ttc",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        ] {
            if let Ok(bytes) = std::fs::read(path) {
                let mut fonts = egui::FontDefinitions::default();
                fonts.font_data.insert(
                    "system-cjk".into(),
                    egui::FontData::from_owned(bytes).into(),
                );
                for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                    fonts
                        .families
                        .entry(family)
                        .or_default()
                        .insert(0, "system-cjk".into());
                }
                context.egui_ctx.set_fonts(fonts);
                break;
            }
        }
        let (sender, receiver) = mpsc::channel();
        let player = crate::player::load_settings(&path);
        let mut saved_players = crate::player::saved_players();
        if let Ok(config) = path.canonicalize()
            && LocalProfile::load(&config).is_ok()
            && !saved_players.iter().any(|player| player.config == config)
        {
            saved_players.push(crate::player::SavedPlayer {
                nickname: "当前账号".into(),
                config,
            });
        }
        Self {
            config: path.display().to_string(),
            executable: String::new(),
            server: server.unwrap_or_default(),
            auth_port: auth_port.map(|p| p.to_string()).unwrap_or_default(),
            lobby_port: lobby_port.map(|p| p.to_string()).unwrap_or_default(),
            show_free_notice: true,
            native_mode: LocalProfile::load(&path).is_ok()
                || path.to_string_lossy().contains("native"),
            player,
            loaded_player_config: path.display().to_string(),
            saved_players,
            status: "先检查配置与客户端。".into(),
            report: String::new(),
            log: String::new(),
            sender,
            receiver,
            stop: None,
            worker: None,
        }
    }

    fn profile(&mut self) -> Result<Profile> {
        let mut profile = Profile::load(&PathBuf::from(&self.config))?;
        if self.executable.trim().is_empty() {
            self.executable = profile.client.executable.display().to_string();
        } else {
            profile.client.executable = resolve(
                &PathBuf::from(self.executable.trim()),
                &std::env::current_dir()?,
            )?;
            profile.validate()?;
        }
        let port = |text: &str| -> Result<Option<u16>> {
            if text.trim().is_empty() {
                Ok(None)
            } else {
                Ok(Some(text.trim().parse()?))
            }
        };
        profile.set_server(
            if self.server.trim().is_empty() {
                None
            } else {
                Some(self.server.trim())
            },
            port(&self.auth_port)?,
            port(&self.lobby_port)?,
        )?;
        Ok(profile)
    }

    fn check(&mut self) {
        if self.native_mode {
            match self
                .local_profile()
                .and_then(|profile| native::plan(&profile))
            {
                Ok(report) => {
                    self.status = if report.launch_allowed {
                        "本机原生前置检查通过，可启动。".into()
                    } else {
                        format!("无法启动：{}", report.blockers.join("；"))
                    };
                    self.report = serde_json::to_string_pretty(&report).unwrap_or_default();
                }
                Err(error) => self.status = format!("{error:#}"),
            }
            return;
        }
        match self
            .profile()
            .and_then(|profile| inspection::inspect(&profile))
        {
            Ok(report) => {
                self.status = if report.launch_allowed {
                    "前置检查通过；尚未验证登录。".into()
                } else {
                    format!("无法启动：{}", report.blockers.join("；"))
                };
                self.report = serde_json::to_string_pretty(&report).unwrap_or_default();
            }
            Err(error) => self.status = format!("{error:#}"),
        }
    }

    fn launch(&mut self) {
        if self.native_mode {
            self.launch_native(false);
            return;
        }
        let profile = match self.profile().and_then(|profile| {
            let report = inspection::inspect(&profile)?;
            anyhow::ensure!(report.launch_allowed, "{}", report.blockers.join("；"));
            Ok(profile)
        }) {
            Ok(profile) => profile,
            Err(error) => {
                self.status = format!("{error:#}");
                return;
            }
        };
        let (stop, receiver) = watch::channel(false);
        self.stop = Some(stop);
        self.status = "正在启动…".into();
        let sender = self.sender.clone();
        self.worker = Some(std::thread::spawn(move || {
            let logs = sender.clone();
            let emit: Event = Arc::new(move |message| {
                let _ = logs.send(Message::Log(message.into()));
            });
            let result = tokio::runtime::Runtime::new()
                .map_err(anyhow::Error::from)
                .and_then(|runtime| {
                    runtime
                        .block_on(session::run_session(&profile, receiver, emit))
                        .map(|_| ())
                });
            let _ = sender.send(Message::Finished(
                result.map_err(|error| format!("{error:#}")),
            ));
        }));
    }

    fn local_profile(&mut self) -> Result<LocalProfile> {
        let mut profile = LocalProfile::load(&PathBuf::from(&self.config))?;
        if self.executable.trim().is_empty() {
            self.executable = profile.client.executable.display().to_string();
        } else {
            profile.client.executable = resolve(
                &PathBuf::from(self.executable.trim()),
                &std::env::current_dir()?,
            )?;
            profile.validate()?;
        }
        Ok(profile)
    }

    fn launch_native(&mut self, recover: bool) {
        let profile = match self.local_profile().and_then(|profile| {
            if !recover {
                let report = native::plan(&profile)?;
                anyhow::ensure!(report.launch_allowed, "{}", report.blockers.join("；"));
            }
            Ok(profile)
        }) {
            Ok(profile) => profile,
            Err(error) => {
                self.status = format!("{error:#}");
                return;
            }
        };
        let (stop, receiver) = watch::channel(false);
        self.stop = Some(stop);
        self.status = if recover {
            "正在恢复…".into()
        } else {
            "正在启动本机原生游戏…".into()
        };
        let sender = self.sender.clone();
        self.worker = Some(std::thread::spawn(move || {
            let logs = sender.clone();
            let emit: Event = Arc::new(move |message| {
                let _ = logs.send(Message::Log(message.into()));
            });
            let result = tokio::runtime::Runtime::new()
                .map_err(anyhow::Error::from)
                .and_then(|runtime| {
                    if recover {
                        runtime.block_on(native::recover(&profile))
                    } else {
                        runtime
                            .block_on(native::run(&profile, receiver, emit))
                            .map(|_| ())
                    }
                });
            let _ = sender.send(Message::Finished(
                result.map_err(|error| format!("{error:#}")),
            ));
        }));
    }

    fn create_player(&mut self) {
        let settings = crate::player::PlayerSettings {
            nickname: self.player.nickname.trim().into(),
            ..self.player.clone()
        };
        let profile = match settings.validate().and_then(|()| self.local_profile()) {
            Ok(profile) => profile,
            Err(error) => {
                self.status = format!("{error:#}");
                return;
            }
        };
        let (stop, mut receiver) = watch::channel(false);
        self.stop = Some(stop);
        self.status = "正在创建独立玩家存档…".into();
        let sender = self.sender.clone();
        self.worker = Some(std::thread::spawn(move || {
            let result = tokio::runtime::Runtime::new()
                .map_err(anyhow::Error::from)
                .and_then(|runtime| {
                    runtime.block_on(async {
                        tokio::select! {
                            result = crate::player::create(&profile, settings) => result,
                            _ = receiver.changed() => anyhow::bail!("创建玩家已取消"),
                        }
                    })
                });
            let _ = sender.send(Message::PlayerCreated(
                result.map_err(|error| format!("{error:#}")),
            ));
        }));
    }
}

impl eframe::App for App {
    fn update(&mut self, context: &egui::Context, _: &mut eframe::Frame) {
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                Message::Log(message) => {
                    if self.log.len() > 64 * 1024 {
                        self.log.clear();
                    }
                    self.log.push_str(&message);
                    self.log.push('\n');
                    self.status = message;
                }
                Message::Finished(result) => {
                    self.status = result.err().unwrap_or_else(|| "会话已结束。".into());
                    self.stop = None;
                    if let Some(worker) = self.worker.take() {
                        let _ = worker.join();
                    }
                }
                Message::PlayerCreated(result) => {
                    self.stop = None;
                    if let Some(worker) = self.worker.take() {
                        let _ = worker.join();
                    }
                    match result {
                        Ok(path) => {
                            let previous = PathBuf::from(&self.config);
                            let remembered = crate::player::remember(&previous)
                                .and_then(|()| crate::player::remember(&path));
                            self.config = path.display().to_string();
                            self.player = crate::player::load_settings(&path);
                            self.loaded_player_config = self.config.clone();
                            self.status =
                                format!("已创建玩家 {}，已切换到独立存档。", self.player.nickname);
                            if let Err(error) = remembered {
                                self.status.push_str(&format!(" 记住选择失败：{error:#}"));
                            }
                            self.saved_players = crate::player::saved_players();
                            self.report.clear();
                        }
                        Err(error) => self.status = error,
                    }
                }
            }
        }
        let running = self.stop.is_some();
        if !running && self.native_mode && self.loaded_player_config != self.config {
            self.player = crate::player::load_settings(&PathBuf::from(&self.config));
            self.loaded_player_config = self.config.clone();
        }
        if !running && !self.show_free_notice {
            let dropped = context.input(|input| input.raw.dropped_files.clone());
            for file in dropped {
                if let Some(path) = file.path {
                    if path
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
                    {
                        self.config = path.display().to_string();
                        self.executable.clear();
                    } else {
                        self.executable = path.display().to_string();
                    }
                }
            }
        }
        egui::CentralPanel::default().show(context, |ui| {
            if self.show_free_notice {
                ui.disable();
            }
            ui.horizontal(|ui| {
                ui.heading("COLS启动器");
                if ui.button("免费声明").clicked() {
                    self.show_free_notice = true;
                }
            });
            ui.add_enabled_ui(!running, |ui| {
                ui.horizontal(|ui| {
                    if ui.selectable_value(&mut self.native_mode, true, "本机原生游戏").changed() {
                        self.config = crate::player::last_config(std::path::Path::new("profiles/native.local.json")).display().to_string(); self.executable.clear();
                    }
                    if ui.selectable_value(&mut self.native_mode, false, "连接转发").changed() {
                        self.config = "profiles/multiplayer.example.json".into(); self.executable.clear();
                    }
                });
            });
            ui.label(if self.native_mode { "保留原生画面与玩法 · 本地服务端 · 会话结束自动恢复配置" }
                else { "本地客户端 · 只读检查 · TCP 转发" });
            ui.add_space(12.0);
            ui.add_enabled_ui(!running, |ui| {
                ui.label("配置 JSON（可拖入文件；修改配置路径后重新检查）");
                if ui.text_edit_singleline(&mut self.config).changed() {
                    self.executable.clear();
                }
                ui.label("客户端路径（可拖入原生可执行文件）");
                ui.text_edit_singleline(&mut self.executable);
                if self.native_mode {
                    if !self.saved_players.is_empty() {
                        let current_path = PathBuf::from(&self.config).canonicalize().unwrap_or_else(|_| PathBuf::from(&self.config));
                        let current = self.saved_players.iter().find(|player| player.config == current_path)
                            .map_or("选择玩家", |player| player.nickname.as_str()).to_owned();
                        egui::ComboBox::from_id_salt("saved-local-player").selected_text(current).show_ui(ui, |ui| {
                            for player in &self.saved_players {
                                let label = if self.saved_players.iter().filter(|entry| entry.nickname == player.nickname).count() > 1 {
                                    let id = player.config.parent().and_then(|path| path.file_name()).and_then(|name| name.to_str()).unwrap_or("原有账号");
                                    format!("{} · {}", player.nickname, &id[..id.char_indices().nth(8).map_or(id.len(), |(i,_)| i)])
                                } else { player.nickname.clone() };
                                if ui.selectable_label(player.config == current_path, label).clicked() {
                                    self.config = player.config.display().to_string();
                                    self.executable.clear();
                                    self.player = crate::player::load_settings(&player.config);
                                    self.loaded_player_config = self.config.clone();
                                    if let Err(error) = crate::player::remember(&player.config) { self.status = format!("{error:#}"); }
                                }
                            }
                        });
                    }
                    ui.label("本机使用配置中的回环地址。连接 INI 和本地 CA 会临时调整，EXE/DLL/游戏资源不变。");
                    if ui.button("生成本机配置").clicked() {
                        match native::initialize(&PathBuf::from(&self.config), None) {
                            Ok(path) => self.status = format!("已生成 {}", path.display()),
                            Err(error) => self.status = format!("{error:#}"),
                        }
                    }
                    ui.add_space(8.0);
                    ui.group(|ui| {
                        ui.label("新玩家配置");
                        ui.horizontal(|ui| {
                            ui.label("昵称");
                            ui.add(egui::TextEdit::singleline(&mut self.player.nickname).desired_width(220.0));
                            ui.label("初始等级");
                            ui.add(egui::DragValue::new(&mut self.player.level).range(1..=70));
                            ui.checkbox(&mut self.player.unlock_all, "全物品解锁");
                        });
                        ui.label("昵称可重名；新玩家使用独立存档。等级与解锁仅在创建时生效。");
                        if ui.button("新建玩家并切换").clicked() { self.create_player(); }
                        if let Ok(profile) = LocalProfile::load(&PathBuf::from(&self.config))
                            && let Some(player) = profile.player {
                            ui.label(format!("当前玩家：{} · 初始等级 {} · {}", player.nickname, player.level,
                                if player.unlock_all { "全物品" } else { "初始装备" }));
                        }
                    });
                } else {
                ui.label("联机服务器 IP / 域名（留空沿用配置）");
                ui.add(
                    egui::TextEdit::singleline(&mut self.server)
                        .hint_text("例如 127.0.0.1 或 localhost"),
                );
                ui.horizontal(|ui| {
                    ui.label("AUTH 端口");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.auth_port)
                            .desired_width(90.0)
                            .hint_text("沿用配置"),
                    );
                    ui.label("LOBBY 端口");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.lobby_port)
                            .desired_width(90.0)
                            .hint_text("沿用配置"),
                    );
                });
                }
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!running, egui::Button::new("检查 / 预览连接"))
                    .clicked()
                {
                    self.check();
                }
                if ui
                    .add_enabled(!running, egui::Button::new(if self.native_mode { "启动本机游戏" } else { "启动客户端" }))
                    .clicked()
                {
                    self.launch();
                }
                if !self.native_mode && ui
                    .add_enabled(!running, egui::Button::new("保存联机配置"))
                    .clicked()
                {
                    match self.profile().and_then(|profile| profile.save_local()) {
                        Ok(path) => {
                            self.config = path.display().to_string();
                            self.status = format!("已保存到 {}", path.display());
                        }
                        Err(error) => self.status = format!("{error:#}"),
                    }
                }
                if self.native_mode && ui.add_enabled(!running, egui::Button::new("恢复上次配置")).clicked() {
                    self.launch_native(true);
                }
                if ui
                    .add_enabled(running, egui::Button::new("结束本次会话"))
                    .clicked()
                    && let Some(stop) = &self.stop
                {
                    stop.send_replace(true);
                }
            });
            ui.label(&self.status);
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.collapsing("只读检查结果", |ui| {
                    ui.add(
                        egui::TextEdit::multiline(&mut self.report)
                            .desired_width(f32::INFINITY)
                            .desired_rows(18)
                            .interactive(false),
                    );
                });
                ui.label("会话日志");
                ui.add(
                    egui::TextEdit::multiline(&mut self.log)
                        .desired_width(f32::INFINITY)
                        .desired_rows(12)
                        .interactive(false),
                );
                ui.label(if self.native_mode { "使用原生客户端连接本地服务端。启动器不打包客户端或游戏资源。" }
                    else { "外部请求拦截尚未实现；TCP 转发不改变 TLS 校验，也不转发游戏 UDP。" });
            });
        });
        if self.show_free_notice {
            let response = egui::Modal::new(egui::Id::new("free-resource-notice"))
                .frame(egui::Frame::popup(&context.style()).inner_margin(24.0))
                .show(context, |ui| {
                    ui.set_width(400.0);
                    ui.vertical_centered(|ui| {
                        ui.heading("免费使用声明");
                        ui.add_space(16.0);
                        ui.label(
                            egui::RichText::new("本启动器及相关资源均免费提供。")
                                .size(20.0)
                                .strong()
                                .color(ui.visuals().warn_fg_color),
                        );
                        ui.add_space(8.0);
                        ui.label("本项目不收取下载、激活或使用费用。");
                        ui.add_space(8.0);
                        ui.label("如您通过付费渠道获取本启动器或相关资源，请及时申请退款，并向交易平台举报。");
                        ui.add_space(16.0);
                        ui.add_sized(
                            [ui.available_width(), 36.0],
                            egui::Button::new("已阅读并知悉"),
                        )
                        .clicked()
                    })
                    .inner
                });
            if response.inner {
                self.show_free_notice = false;
            }
        }
        context.request_repaint_after(std::time::Duration::from_millis(100));
    }
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop {
            stop.send_replace(true);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn open(
    path: PathBuf,
    server: Option<String>,
    auth_port: Option<u16>,
    lobby_port: Option<u16>,
) -> Result<()> {
    eframe::run_native(
        "COLS启动器",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([900.0, 680.0])
                .with_min_inner_size([700.0, 500.0]),
            ..Default::default()
        },
        Box::new(move |context| {
            Ok(Box::new(App::new(
                context, path, server, auth_port, lobby_port,
            )))
        }),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}
