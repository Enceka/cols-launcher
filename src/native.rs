//! Local original-client orchestration: prepare, wait, launch and restore.
use crate::inspection::{Inventory, inspect, inventory};
use crate::native_files::{ClientLocks, Journal, journal_path};
use crate::native_readiness::{self, Kind};
use crate::process::{Identity, client_process};
use crate::profile::{Client, Profile, Routing, resolve};
use crate::relay::Event;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Seek, Write};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::watch;
use tokio::task::JoinHandle;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Backend {
    pub executable: PathBuf,
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    pub service_data_dir: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub player_template: Option<PathBuf>,
    pub game_state: PathBuf,
    pub map_catalog: PathBuf,
    pub title_id: u32,
    pub ip: Ipv4Addr,
    pub auth_port: u16,
    pub lobby_port: u16,
    pub stun_port: u16,
    pub stun_alt_port: u16,
    pub game_port: u16,
    pub resource_port: u16,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalProfile {
    pub version: u32,
    pub client: Client,
    pub backend: Backend,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub player: Option<crate::player::PlayerSettings>,
    pub state_dir: PathBuf,
    pub startup_timeout_seconds: u64,
    pub client_startup_timeout_seconds: u64,
    #[serde(default)]
    pub trace_wine_files: bool,
    #[serde(skip)]
    pub source: PathBuf,
}

impl LocalProfile {
    pub fn load(path: &Path) -> Result<Self> {
        let source = path.canonicalize().context("找不到本地原生配置")?;
        ensure!(source.metadata()?.len() <= 64 * 1024, "本地配置超过 64 KiB");
        let mut value: Self = serde_json::from_slice(&std::fs::read(&source)?)?;
        value.source = source.clone();
        let base = source.parent().context("配置没有父目录")?;
        value.client.executable = resolve(&value.client.executable, base)?;
        value.state_dir = resolve(&value.state_dir, base)?;
        value.client.runner = value
            .client
            .runner
            .as_deref()
            .map(|path| resolve(path, base))
            .transpose()?;
        value.client.wine_prefix = value
            .client
            .wine_prefix
            .as_deref()
            .map(|path| resolve(path, base))
            .transpose()?;
        for path in [
            &mut value.backend.executable,
            &mut value.backend.certificate,
            &mut value.backend.private_key,
            &mut value.backend.service_data_dir,
            &mut value.backend.game_state,
            &mut value.backend.map_catalog,
        ] {
            *path = resolve(path, base)?;
        }
        value.backend.player_template = value
            .backend
            .player_template
            .as_deref()
            .map(|path| resolve(path, base))
            .transpose()?;
        value.validate()?;
        Ok(value)
    }

    pub fn client_profile(&self) -> Profile {
        let mut client = self.client.clone();
        if let Some(player) = &self.player {
            client.arguments = player.client_arguments(&client.arguments);
        }
        Profile {
            version: self.version,
            client,
            routing: Routing::Direct,
            routes: Vec::new(),
            state_dir: self.state_dir.clone(),
            source: self.source.clone(),
        }
    }

    pub fn root(&self) -> &Path {
        self.client
            .executable
            .parent()
            .expect("validated executable path")
    }
    pub fn address(&self, port: u16) -> SocketAddrV4 {
        SocketAddrV4::new(self.backend.ip, port)
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(player) = &self.player {
            player.validate()?;
        }
        self.client_profile().validate()?;
        ensure!(
            self.client.sha256.is_some(),
            "原生本地模式需要明确的 EXE SHA-256 基线"
        );
        ensure!(
            self.backend.ip.is_loopback(),
            "本地模式只允许 IPv4 回环地址"
        );
        ensure!(
            (1..=240).contains(&self.startup_timeout_seconds)
                && (1..=120).contains(&self.client_startup_timeout_seconds),
            "启动超时范围无效"
        );
        ensure!(self.backend.title_id != 0, "本地 title_id 不能为零");
        let backend = &self.backend;
        let tcp = [backend.auth_port, backend.lobby_port, backend.resource_port];
        let udp = [backend.stun_port, backend.stun_alt_port, backend.game_port];
        ensure!(
            !tcp.contains(&0)
                && !udp.contains(&0)
                && tcp
                    .into_iter()
                    .collect::<std::collections::HashSet<_>>()
                    .len()
                    == 3
                && udp
                    .into_iter()
                    .collect::<std::collections::HashSet<_>>()
                    .len()
                    == 3,
            "本地服务端端口不能为零或在同一传输协议内重复"
        );
        for path in [
            &backend.executable,
            &backend.certificate,
            &backend.private_key,
            &backend.service_data_dir,
            &backend.game_state,
            &backend.map_catalog,
        ] {
            ensure!(
                !resolve(path, self.root())?.starts_with(self.root()),
                "本地服务端文件和资料必须位于客户端目录之外"
            );
        }
        if let Some(path) = &self.backend.player_template {
            ensure!(
                !resolve(path, self.root())?.starts_with(self.root()),
                "初始玩家资料必须位于客户端目录之外"
            );
        }
        Ok(())
    }

    pub fn changes(&self) -> Result<Vec<(String, Vec<u8>)>> {
        let cert = std::fs::read(&self.backend.certificate)?;
        ensure!(
            cert.len() <= 128 * 1024
                && cert
                    .windows(27)
                    .any(|bytes| bytes == b"-----BEGIN CERTIFICATE-----"),
            "本地证书必须是 PEM 格式且不超过 128 KiB"
        );
        let bundle = self.root().join("cacert.pem");
        ensure!(
            bundle.is_file() && bundle.metadata()?.len() <= 2 * 1024 * 1024,
            "原生客户端需要现有根 CA bundle"
        );
        let mut trusted = std::fs::read(bundle)?;
        // Preserve the existing trust roots; append the configured local certificate only.
        if !trusted
            .windows(cert.len())
            .any(|bytes| bytes == cert.as_slice())
        {
            trusted.push(b'\n');
            trusted.extend(&cert);
        }
        Ok(vec![
            (
                "dwauth.ini".into(),
                format!(
                    "{} {}\n{} {}\n0x{:08X}\n",
                    self.backend.ip,
                    self.backend.lobby_port,
                    self.backend.ip,
                    self.backend.auth_port,
                    self.backend.title_id
                )
                .into_bytes(),
            ),
            (
                "stun.ini".into(),
                format!("{}\n", self.backend.ip).into_bytes(),
            ),
            (
                "iips.ini".into(),
                format!(
                    "enable_iips=1\niips_url=\"http://{}:{}/\"\n",
                    self.backend.ip, self.backend.resource_port
                )
                .into_bytes(),
            ),
            ("cacert.pem".into(), trusted.clone()),
            ("IIPS/cacert.pem".into(), trusted),
        ])
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let destination = resolve(path, &std::env::current_dir()?)?;
        let mut value = self.clone();
        value.source = destination.clone();
        value.validate()?;
        std::fs::write(destination, serde_json::to_vec_pretty(&value)?)?;
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct LocalReport {
    pub client: crate::inspection::Report,
    pub backend: String,
    pub edited_files: Vec<String>,
    pub existing_client_pid: Option<u32>,
    pub launch_allowed: bool,
    pub blockers: Vec<String>,
    pub notes: Vec<String>,
}

pub fn plan(profile: &LocalProfile) -> Result<LocalReport> {
    profile.validate()?;
    let mut client = inspect(&profile.client_profile())?;
    client.notes = vec![
        "这里只检查原生程序、运行器与文件基线；本机连接配置由原生会话流程管理。".into(),
        "摘要检查范围有限，不表示全部资源完整性或客户端授权。".into(),
    ];
    let mut blockers = client.blockers.clone();
    for (label, path) in [
        ("服务端程序", &profile.backend.executable),
        ("本地证书", &profile.backend.certificate),
        ("本地私钥", &profile.backend.private_key),
        ("地图状态", &profile.backend.game_state),
        ("地图目录", &profile.backend.map_catalog),
    ] {
        if !path.is_file() {
            blockers.push(format!("缺少{label}：{}", path.display()));
        }
    }
    for name in ["configs_s.info", "mp_playlists_shipRetail_release.info"] {
        if !profile
            .backend
            .service_data_dir
            .join("publisher")
            .join(name)
            .is_file()
        {
            blockers.push(format!("本地服务资料缺少 publisher/{name}"));
        }
    }
    if !profile.root().join("IIPS").is_dir() {
        blockers.push("缺少原生 IIPS 目录".into());
    }
    if let Err(error) = profile.changes() {
        blockers.push(error.to_string());
    }
    let existing = client_process(
        &profile.client.executable,
        profile.client.wine_prefix.as_deref(),
    );
    if let Some(process) = &existing {
        blockers.push(format!(
            "原生客户端已在运行 PID={}；不重复启动或改写其配置",
            process.pid
        ));
    }
    if journal_path(&profile.state_dir, profile.root()).exists() {
        blockers.push("有待恢复的配置；先运行 native-recover".into());
    }
    Ok(LocalReport {
        client,
        backend: profile.backend.executable.display().to_string(),
        edited_files: crate::native_files::EDITABLE
            .iter()
            .map(|name| (*name).into())
            .collect(),
        existing_client_pid: existing.map(|process| process.pid),
        launch_allowed: blockers.is_empty(),
        blockers,
        notes: vec![
            "启动前临时写入连接 INI 和附加本地 CA；退出后恢复原内容，原来不存在的文件会删除。"
                .into(),
            "不替换 EXE、DLL 或游戏资源；不导入系统证书库，不调用 Python。".into(),
            "先等待 AUTH/LOBBY/STUN/游戏与资源监听就绪，再启动原生程序；就绪不等于登录成功。"
                .into(),
            "这是本机模式，尚不提供公网联机配置。".into(),
        ],
    })
}

struct Service {
    child: Child,
    identity: Identity,
    readers: Vec<JoinHandle<()>>,
    ready_path: PathBuf,
    kind: Kind,
    ready: bool,
}

impl Service {
    async fn spawn(
        mut command: Command,
        ready_path: PathBuf,
        kind: Kind,
        log: Arc<Mutex<(File, usize)>>,
    ) -> Result<Self> {
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env("RUST_LOG", "info")
            .env("NO_COLOR", "1");
        let mut child = command.spawn()?;
        let identity = Identity::from_pid(child.id().context("服务端进程没有 PID")?)?;
        let mut readers = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            readers.push(read_output(stdout, log.clone()));
        }
        if let Some(stderr) = child.stderr.take() {
            readers.push(read_output(stderr, log));
        }
        Ok(Self {
            child,
            identity,
            readers,
            ready_path,
            kind,
            ready: false,
        })
    }

    async fn stop(&mut self) -> Result<()> {
        let result = self.identity.stop(true).await;
        if self.child.try_wait()?.is_none() {
            self.child.start_kill()?;
        }
        let waited = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
        for reader in self.readers.drain(..) {
            reader.abort();
        }
        waited.context("服务进程仍在系统退出等待中")??;
        result
    }
}

fn read_output<T: tokio::io::AsyncRead + Unpin + Send + 'static>(
    stream: T,
    log: Arc<Mutex<(File, usize)>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stream).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if let Ok(mut log) = log.lock()
                && log.1 < 4 * 1024 * 1024
            {
                let bytes = line.as_bytes();
                let count = bytes.len().min(16 * 1024);
                let _ = log.0.write_all(&bytes[..count]);
                let _ = log.0.write_all(b"\n");
                log.1 += count + 1;
            }
        }
    })
}

fn capture_client_output<T: tokio::io::AsyncRead + Unpin + Send + 'static>(
    mut stream: T,
    log: Arc<Mutex<(File, usize)>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buffer = [0_u8; 8192];
        while let Ok(count) = stream.read(&mut buffer).await {
            if count == 0 {
                break;
            }
            if let Ok(mut log) = log.lock() {
                if log.1 + count > 4 * 1024 * 1024 {
                    if log.0.set_len(0).is_err() || log.0.rewind().is_err() {
                        continue;
                    }
                    log.1 = 0;
                }
                if log.0.write_all(&buffer[..count]).is_ok() {
                    log.1 += count;
                }
            }
        }
    })
}

fn backend_command(profile: &LocalProfile, output: &Path) -> Command {
    let backend = &profile.backend;
    let mut command = Command::new(&backend.executable);
    command
        .arg("serve-auth")
        .arg("--bind")
        .arg(profile.address(backend.auth_port).to_string())
        .arg("--cert")
        .arg(&backend.certificate)
        .arg("--key")
        .arg(&backend.private_key)
        .arg("--capture-dir")
        .arg(output.join("auth-captures"))
        .arg("--local-title-id")
        .arg(backend.title_id.to_string())
        .arg("--lobby-bind")
        .arg(profile.address(backend.lobby_port).to_string())
        .arg("--service-data-dir")
        .arg(&backend.service_data_dir)
        .arg("--stun-bind")
        .arg(profile.address(backend.stun_port).to_string())
        .arg("--stun-alt")
        .arg(profile.address(backend.stun_alt_port).to_string())
        .arg("--game-bind")
        .arg(profile.address(backend.game_port).to_string())
        .arg("--game-state")
        .arg(&backend.game_state)
        .arg("--game-map-catalog")
        .arg(&backend.map_catalog)
        .arg("--native-bot-rooms")
        .arg("--continuous-play")
        .arg("--game-capture-every")
        .arg("1000")
        .arg("--launcher-ready-file")
        .arg(output.join("backend-ready.json"));
    command
}

fn resource_command(profile: &LocalProfile, output: &Path) -> Result<Command> {
    let capture = output.join("resource-captures").display().to_string();
    let config = format!(
        "capture_dir = {}\nmax_connections = 16\nmax_sessions_per_run = 128\nmax_bytes_per_session = 1048576\nmax_chunks_per_session = 4096\nidle_timeout_ms = 10000\nsession_timeout_ms = 30000\n[[listeners]]\nname = \"resources\"\nbind = \"{}\"\n",
        serde_json::to_string(&capture)?,
        profile.address(profile.backend.resource_port)
    );
    let path = output.join("resources.toml");
    std::fs::write(&path, config)?;
    let mut command = Command::new(&profile.backend.executable);
    command
        .arg("observe")
        .arg("--config")
        .arg(path)
        .arg("--launcher-ready-file")
        .arg(output.join("resources-ready.json"));
    Ok(command)
}

async fn wait_ready(
    services: &mut [Service],
    profile: &LocalProfile,
    stop: &mut watch::Receiver<bool>,
    timeout: u64,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(timeout);
    loop {
        ensure!(!*stop.borrow(), "启动已取消");
        for service in services.iter_mut() {
            ensure!(
                service.child.try_wait()?.is_none(),
                "本地服务端提前退出；查看 backend.log，旧版服务端须更新以支持 --launcher-ready-file"
            );
            if !service.ready {
                service.ready = native_readiness::verify(
                    &service.ready_path,
                    service.identity.pid,
                    service.kind,
                    profile,
                )?;
            }
        }
        if services.iter().all(|service| service.ready) {
            return Ok(());
        }
        ensure!(
            Instant::now() < deadline,
            "等待本地服务端就绪超时；尚缺 {}",
            services
                .iter()
                .filter(|service| !service.ready)
                .map(|service| service.ready_path.file_name().unwrap().to_string_lossy())
                .collect::<Vec<_>>()
                .join(", ")
        );
        tokio::select! {
            _ = stop.changed() => bail!("启动已取消"),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {},
        }
    }
}

#[derive(Debug, Serialize)]
pub struct LocalSession {
    pub client_pid: Option<u32>,
    pub client_exit_code: Option<i32>,
    pub before: Inventory,
    pub after: Inventory,
    pub changed_files: Vec<String>,
    pub backend_ready: bool,
    pub configs_restored: bool,
    pub error: Option<String>,
    pub wine_runtime: Option<crate::wine::RecoveryReport>,
}

pub async fn run(
    profile: &LocalProfile,
    mut stop: watch::Receiver<bool>,
    emit: Event,
) -> Result<LocalSession> {
    let report = plan(profile)?;
    ensure!(report.launch_allowed, "{}", report.blockers.join("；"));
    ensure!(!*stop.borrow(), "启动已取消");
    let _locks = ClientLocks::acquire(profile.root())?;
    ensure!(
        client_process(
            &profile.client.executable,
            profile.client.wine_prefix.as_deref()
        )
        .is_none(),
        "客户端已被其他程序启动"
    );
    profile.validate()?;
    std::fs::create_dir_all(&profile.state_dir)?;
    profile.validate()?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let output = profile.state_dir.join(format!("native-{stamp}"));
    std::fs::create_dir(&output)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut record = LocalSession {
        client_pid: None,
        client_exit_code: None,
        before: inventory(&profile.client_profile())?,
        after: Inventory::new(),
        changed_files: Vec::new(),
        backend_ready: false,
        configs_restored: false,
        error: None,
        wine_runtime: None,
    };
    let changes = profile.changes()?;
    let mut journal = Journal::prepare(profile.root(), &profile.state_dir, &output, &changes)?;
    let log = Arc::new(Mutex::new((File::create(output.join("backend.log"))?, 0)));
    let mut services = Vec::new();
    let mut wrapper: Option<Child> = None;
    let mut wrapper_pid = None;
    let mut game: Option<Identity> = None;
    let mut wine = None;
    let mut client_readers = Vec::new();
    let result: Result<()> = async {
        wine = crate::wine::Runtime::prepare(&profile.client, &output, &emit).await?;
        record.wine_runtime = wine.as_ref().map(|runtime| runtime.report.clone());
        emit("启动本地服务端，等待地图与网络端点就绪…");
        for (command, name, kind) in [
            (backend_command(profile, &output), "backend-ready.json", Kind::NativeServices),
            (resource_command(profile, &output)?, "resources-ready.json", Kind::Resources),
        ] {
            let service = Service::spawn(command, output.join(name), kind, log.clone()).await?;
            journal.processes.push(service.identity.clone());
            services.push(service);
            journal.save()?;
        }
        wait_ready(&mut services, profile, &mut stop, profile.startup_timeout_seconds).await?;
        record.backend_ready = true;
        ensure!(client_process(&profile.client.executable, profile.client.wine_prefix.as_deref()).is_none(), "等待期间客户端被其他程序启动");
        journal.apply(&changes)?;
        emit("本地服务端就绪；已临时配置连接与本地 CA，正在启动原生客户端…");
        let client = &profile.client;
        let mut command = Command::new(client.runner.as_ref().unwrap_or(&client.executable));
        if client.runner.is_some() { command.arg(&client.executable); }
        let arguments = profile.player.as_ref().map_or_else(|| client.arguments.clone(), |player| player.client_arguments(&client.arguments));
        command.args(arguments).current_dir(profile.root()).kill_on_drop(true).stdout(Stdio::piped()).stderr(Stdio::piped());
        if let Some(prefix) = &client.wine_prefix {
            command.env("WINEPREFIX", prefix).env("WINEDEBUG", if profile.trace_wine_files { "-all,err+all,+timestamp,+file" } else { "-all,err+all" });
        }
        wrapper = Some(command.spawn()?);
        let client_log = Arc::new(Mutex::new((File::create(output.join("client.log"))?, 0)));
        if let Some(stdout) = wrapper.as_mut().unwrap().stdout.take() { client_readers.push(capture_client_output(stdout, client_log.clone())); }
        if let Some(stderr) = wrapper.as_mut().unwrap().stderr.take() { client_readers.push(capture_client_output(stderr, client_log)); }
        wrapper_pid = wrapper.as_ref().and_then(Child::id);
        if let Ok(identity) = Identity::from_pid(wrapper.as_ref().and_then(Child::id).context("缺少启动进程 PID")?) {
            journal.processes.push(identity);
            journal.save()?;
        }
        let until = Instant::now() + Duration::from_secs(profile.client_startup_timeout_seconds);
        while game.is_none() {
            if let Some(runtime) = wine.as_mut() { runtime.observe_server()?; }
            game = client_process(&client.executable, client.wine_prefix.as_deref());
            if game.is_some() { break; }
            if let Some(status) = wrapper.as_mut().unwrap().try_wait()? { ensure!(status.success(), "原生启动程序失败：{status}"); }
            ensure!(Instant::now() < until, "原生客户端进程未出现");
            for service in &mut services { ensure!(service.child.try_wait()?.is_none(), "本地服务端提前退出"); }
            tokio::select! { _ = stop.changed() => bail!("启动已取消"), _ = tokio::time::sleep(Duration::from_millis(100)) => {} }
        }
        let identity = game.as_ref().context("缺少原生进程身份")?;
        record.client_pid = Some(identity.pid);
        journal.processes.push(identity.clone());
        journal.save()?;
        emit(&format!("原生客户端已启动 PID={}；在游戏内完成登录并创建本地人机房间。", identity.pid));
        while identity.alive() && !*stop.borrow() {
            for service in &mut services { ensure!(service.child.try_wait()?.is_none(), "本地服务端意外退出"); }
            tokio::select! { _ = stop.changed() => break, _ = tokio::time::sleep(Duration::from_millis(250)) => {} }
        }
        if !*stop.borrow() && wrapper_pid == Some(identity.pid)
            && let Some(status) = wrapper.as_mut().unwrap().try_wait()? {
            record.client_exit_code = status.code();
            ensure!(status.success(), "原生客户端异常退出：{status}");
        }
        Ok(())
    }.await;
    // Stop our own processes before restoring files read by the native client.
    let mut cleanup_errors = Vec::new();
    if let Some(runtime) = wine.as_mut()
        && let Err(error) = runtime.shutdown().await
    {
        cleanup_errors.push(error.to_string());
    }
    if let Some(identity) = &game
        && let Err(error) = identity.stop(false).await
    {
        cleanup_errors.push(error.to_string());
    }
    if let Some(wrapper) = wrapper.as_mut() {
        if wrapper.try_wait()?.is_none() {
            let _ = wrapper.start_kill();
        }
        if tokio::time::timeout(Duration::from_secs(5), wrapper.wait())
            .await
            .is_err()
        {
            cleanup_errors.push("客户端仍在系统退出等待中，未无限等待该进程".into());
        }
    }
    for reader in client_readers {
        reader.abort();
    }
    for service in &mut services {
        if let Err(error) = service.stop().await {
            cleanup_errors.push(error.to_string());
        }
    }
    if client_process(
        &profile.client.executable,
        profile.client.wine_prefix.as_deref(),
    )
    .is_none()
    {
        match journal.restore() {
            Ok(()) => record.configs_restored = true,
            Err(error) => cleanup_errors.push(error.to_string()),
        }
    } else {
        cleanup_errors.push("原生客户端仍在运行，配置与恢复记录保持不动".into());
    }
    record.after = inventory(&profile.client_profile())?;
    record.changed_files = record
        .before
        .iter()
        .filter(|(name, hash)| record.after.get(*name) != Some(*hash))
        .map(|(name, _)| name.clone())
        .collect();
    record.error = result.as_ref().err().map(|error| format!("{error:#}"));
    if !cleanup_errors.is_empty() {
        record.error = Some(format!(
            "{}；{}",
            record.error.unwrap_or_default(),
            cleanup_errors.join("；")
        ));
    }
    std::fs::write(
        output.join("session.json"),
        serde_json::to_vec_pretty(&record)?,
    )?;
    emit(if record.configs_restored {
        "本次会话结束；连接配置与 CA 文件已恢复。"
    } else {
        "本次会话结束；有待恢复配置，查看恢复记录。"
    });
    result?;
    ensure!(cleanup_errors.is_empty(), "{}", cleanup_errors.join("；"));
    Ok(record)
}

pub async fn recover(profile: &LocalProfile) -> Result<()> {
    let _locks = ClientLocks::acquire(profile.root())?;
    let path = journal_path(&profile.state_dir, profile.root());
    let journal = Journal::load(&path, profile.root(), &profile.state_dir)?;
    ensure!(!journal.owner.alive(), "原启动器会话仍在运行，不能抢占恢复");
    // Recorded identities include start time and image to guard against PID reuse.
    for identity in journal.processes.iter().rev() {
        let allowed = [&profile.backend.executable, &profile.client.executable];
        ensure!(
            allowed
                .iter()
                .any(|path| path.canonicalize().ok().as_deref() == Some(identity.image.as_path()))
                || profile.client.runner.as_ref().is_some_and(|path| path
                    .canonicalize()
                    .ok()
                    .as_deref()
                    == Some(identity.image.as_path())),
            "恢复记录包含不属于本次配置的程序，保留记录并拒绝结束该进程"
        );
        identity.stop(false).await?;
    }
    ensure!(
        client_process(
            &profile.client.executable,
            profile.client.wine_prefix.as_deref()
        )
        .is_none(),
        "客户端仍在运行；暂不恢复配置"
    );
    journal.restore()
}

pub fn initialize(path: &Path, workspace: Option<&Path>) -> Result<PathBuf> {
    ensure!(!path.exists(), "本地配置已存在，不覆盖已有配置");
    let mut profile: LocalProfile =
        serde_json::from_str(include_str!("../profiles/native-local.example.json"))?;
    let destination = resolve(path, &std::env::current_dir()?)?;
    let current = std::env::current_dir()?;
    let workspace = workspace
        .map(|path| resolve(path, &current))
        .transpose()?
        .unwrap_or_else(|| current.parent().unwrap_or(&current).to_owned());
    // The embedded example's relative paths are rooted in the selected workspace,
    // not the source directory used to build this binary.
    let base_directory = workspace.join("launcher/profiles");
    let base = base_directory.as_path();
    profile.backend.player_template = profile
        .backend
        .player_template
        .as_deref()
        .map(|path| resolve(path, base))
        .transpose()?;
    for target in [
        &mut profile.client.executable,
        &mut profile.state_dir,
        &mut profile.backend.executable,
        &mut profile.backend.certificate,
        &mut profile.backend.private_key,
        &mut profile.backend.service_data_dir,
        &mut profile.backend.game_state,
        &mut profile.backend.map_catalog,
    ] {
        *target = resolve(target, base)?;
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").context("缺少用户目录")?;
        let index = PathBuf::from(home).join("Library/Application Support/com.gamemac.www/wine-engine/container/wine_installations.json");
        if index.is_file() {
            let values: serde_json::Value = serde_json::from_slice(&std::fs::read(index)?)?;
            if let Some(engine) = values["wine_installations"]
                .as_object()
                .and_then(|entries| entries.values().find(|entry| entry["id"] == "10000033"))
                && let Some(path) = engine["install_path"].as_str()
            {
                let runner = Path::new(path).join("bin/wine");
                if runner.is_file() {
                    profile.client.runner = Some(runner);
                }
            }
        }
        let prefix = resolve(
            Path::new("../../server/client-work/wine10-throwables-20261011"),
            base,
        )?;
        if prefix.is_dir() {
            profile.client.wine_prefix = Some(prefix);
        }
    }
    profile.source = destination.clone();
    #[cfg(target_os = "windows")]
    {
        profile.backend.executable.set_extension("exe");
    }
    profile.validate()?;
    std::fs::create_dir_all(destination.parent().context("配置没有父目录")?)?;
    profile.save(&destination)?;
    Ok(destination)
}
