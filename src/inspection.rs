use crate::profile::{Endpoint, Profile, Routing};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

pub const PROTECTED_FILES: &[&str] = &[
    "codoMP_client_shipRetail.exe",
    "dwauth.ini",
    "stun.ini",
    "iips.ini",
    "cacert.pem",
    "IIPS/cacert.pem",
    "IIPS/IIPSVFS.dll",
    "IIPS/IFS2.dll",
    "TenProxy.dll",
    "Base.dll",
];
pub type Inventory = BTreeMap<String, Option<String>>;

pub fn sha256(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub fn inventory(profile: &Profile) -> Result<Inventory> {
    let root = profile
        .client
        .executable
        .parent()
        .context("客户端没有父目录")?;
    let mut result = Inventory::new();
    for name in PROTECTED_FILES {
        let path = root.join(name);
        result.insert(
            (*name).into(),
            if path.is_file() {
                Some(sha256(&path)?)
            } else {
                None
            },
        );
    }
    result.insert(
        "selected_executable".into(),
        if profile.client.executable.is_file() {
            Some(sha256(&profile.client.executable)?)
        } else {
            None
        },
    );
    Ok(result)
}

pub fn bootstrap_endpoints(root: &Path) -> Result<Vec<Endpoint>> {
    let path = root.join("dwauth.ini");
    ensure!(
        path.is_file() && path.metadata()?.len() <= 4096,
        "缺少或无法读取现有 dwauth.ini；启动器不会创建或改写它"
    );
    let text = std::fs::read_to_string(&path)?;
    let fields: Vec<_> = text.split_whitespace().collect();
    ensure!(fields.len() == 5, "dwauth.ini 必须包含 5 个字段");
    let title = fields[4]
        .strip_prefix("0x")
        .or_else(|| fields[4].strip_prefix("0X"))
        .unwrap_or(fields[4]);
    u32::from_str_radix(title, 16).context("dwauth.ini title_id 无效")?;
    let mut endpoints = Vec::new();
    for offset in [0, 2] {
        let endpoint = Endpoint {
            host: fields[offset].to_owned(),
            port: fields[offset + 1].parse().context("dwauth.ini 端口无效")?,
        };
        endpoint
            .literal()
            .context("existing-config 需要可直接匹配的 IP 字面地址")?;
        ensure!(endpoint.port != 0, "dwauth.ini 端口不能为零");
        endpoints.push(endpoint);
    }
    Ok(endpoints)
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub executable: String,
    pub command: Vec<String>,
    pub routing: Routing,
    pub routes: Vec<crate::profile::Route>,
    pub protected_files: Inventory,
    pub existing_auth_lobby: Vec<Endpoint>,
    pub launch_allowed: bool,
    pub blockers: Vec<String>,
    pub notes: Vec<String>,
}

pub fn inspect(profile: &Profile) -> Result<Report> {
    profile.validate()?;
    let files = inventory(profile)?;
    let mut blockers = Vec::new();
    match &files["selected_executable"] {
        None => blockers.push("找不到客户端可执行文件".into()),
        Some(actual)
            if profile
                .client
                .sha256
                .as_ref()
                .is_some_and(|expected| expected != actual) =>
        {
            blockers.push("客户端 SHA-256 与配置基线不一致".into())
        }
        _ => {}
    }
    if profile
        .client
        .runner
        .as_ref()
        .is_some_and(|runner| !runner.is_file())
    {
        blockers.push("找不到配置的 Wine/兼容运行器".into());
    }
    if profile
        .client
        .wine_prefix
        .as_ref()
        .is_some_and(|prefix| !prefix.is_dir())
    {
        blockers.push("Wine prefix 必须已经存在；启动器不创建运行环境".into());
    }
    #[cfg(not(target_os = "windows"))]
    if profile.client.runner.is_none()
        && profile
            .client
            .executable
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
    {
        blockers.push("本机不是 Windows；启动 EXE 需要配置现有 Wine runner".into());
    }
    let mut endpoints = Vec::new();
    match profile.routing {
        Routing::External => {
            blockers.push("外部请求拦截适配器尚未实现；TCP 转发器不会自动截获原连接".into())
        }
        Routing::ExistingConfig => {
            match bootstrap_endpoints(profile.client.executable.parent().context("缺少父目录")?)
            {
                Ok(found) => {
                    if !found
                        .iter()
                        .all(|ep| profile.routes.iter().any(|r| &r.listen == ep))
                    {
                        blockers
                            .push("现有 dwauth.ini 的 AUTH/LOBBY 必须全部匹配转发监听地址".into());
                    }
                    endpoints = found;
                }
                Err(error) => blockers.push(error.to_string()),
            }
        }
        Routing::Direct => {}
    }
    Ok(Report {
        executable: profile.client.executable.display().to_string(),
        command: profile.command(),
        routing: profile.routing,
        routes: profile.routes.clone(),
        protected_files: files,
        existing_auth_lobby: endpoints,
        launch_allowed: blockers.is_empty(),
        blockers,
        notes: vec![
            "前置检查通过不表示真实游戏登录成功。".into(),
            "TLS 字节原样转发；不导入 CA、不修改 cacert.pem、不关闭验证。".into(),
            "UDP/STUN/游戏通道不由 v0 转发。".into(),
            "摘要只检查已列出的文件，不证明全部资源完整性或客户端授权。".into(),
        ],
    })
}
