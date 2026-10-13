use crate::inspection;
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
}

struct App {
    config: String,
    executable: String,
    server: String,
    auth_port: String,
    lobby_port: String,
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
        Self {
            config: path.display().to_string(),
            executable: String::new(),
            server: server.unwrap_or_default(),
            auth_port: auth_port.map(|p| p.to_string()).unwrap_or_default(),
            lobby_port: lobby_port.map(|p| p.to_string()).unwrap_or_default(),
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
            }
        }
        let running = self.stop.is_some();
        if !running {
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
            ui.heading("COLS启动器");
            ui.label("本地客户端 · 只读检查 · TCP 转发");
            ui.add_space(12.0);
            ui.add_enabled_ui(!running, |ui| {
                ui.label("配置 JSON（可拖入文件；修改配置路径后重新检查）");
                if ui.text_edit_singleline(&mut self.config).changed() {
                    self.executable.clear();
                }
                ui.label("客户端路径（可拖入原生可执行文件）");
                ui.text_edit_singleline(&mut self.executable);
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
                    .add_enabled(!running, egui::Button::new("启动客户端"))
                    .clicked()
                {
                    self.launch();
                }
                if ui
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
                ui.label("外部请求拦截尚未实现；TCP 转发不改变 TLS 校验，也不转发游戏 UDP。");
            });
        });
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
