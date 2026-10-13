use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, Deserialize, Serialize, Eq, Hash, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

impl Endpoint {
    pub fn address(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    pub fn literal(&self) -> Result<SocketAddr> {
        Ok(SocketAddr::new(
            self.host.parse().context("监听端点必须是 IP 字面地址")?,
            self.port,
        ))
    }
}

pub fn validate_host(host: &str) -> Result<()> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        ensure!(
            !ip.is_unspecified() && !ip.is_multicast() && ip != IpAddr::V4(Ipv4Addr::BROADCAST),
            "不能使用通配、多播或广播地址"
        );
        return Ok(());
    }
    let name = host.strip_suffix('.').unwrap_or(host);
    ensure!(
        !name.is_empty()
            && name.len() <= 253
            && name.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            }),
        "服务器地址必须是 IP 或有效的 ASCII 域名（国际化域名请使用 punycode）"
    );
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub name: String,
    pub listen: Endpoint,
    pub upstream: Endpoint,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Routing {
    Direct,
    ExistingConfig,
    External,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Client {
    pub executable: PathBuf,
    pub sha256: Option<String>,
    pub arguments: Vec<String>,
    pub runner: Option<PathBuf>,
    pub wine_prefix: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub version: u32,
    pub client: Client,
    pub routing: Routing,
    pub routes: Vec<Route>,
    pub state_dir: PathBuf,
    #[serde(skip)]
    pub source: PathBuf,
}

// Resolve existing symlink ancestors even when the final file is missing.
pub fn resolve(path: &Path, base: &Path) -> Result<PathBuf> {
    ensure!(!path.as_os_str().is_empty(), "路径不能为空");
    let full = if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in full.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    let mut ancestor = normalized.as_path();
    let mut tail = Vec::new();
    while !ancestor.exists() {
        tail.push(
            ancestor
                .file_name()
                .context("找不到可解析的路径祖先")?
                .to_owned(),
        );
        ancestor = ancestor.parent().context("找不到路径祖先")?;
    }
    let mut resolved = ancestor.canonicalize()?;
    for part in tail.into_iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

impl Profile {
    pub fn load(path: &Path) -> Result<Self> {
        let source = path.canonicalize().context("找不到启动器配置文件")?;
        ensure!(source.metadata()?.len() <= 64 * 1024, "配置文件超过 64 KiB");
        let mut profile: Self =
            serde_json::from_slice(&std::fs::read(&source)?).context("启动器 JSON 配置无效")?;
        profile.source = source.clone();
        let base = source.parent().context("配置文件没有父目录")?;
        profile.client.executable = resolve(&profile.client.executable, base)?;
        profile.state_dir = resolve(&profile.state_dir, base)?;
        profile.client.runner = profile
            .client
            .runner
            .as_deref()
            .map(|p| resolve(p, base))
            .transpose()?;
        profile.client.wine_prefix = profile
            .client
            .wine_prefix
            .as_deref()
            .map(|p| resolve(p, base))
            .transpose()?;
        profile.validate()?;
        Ok(profile)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.version == 1, "仅支持配置 version=1");
        let root = self
            .client
            .executable
            .parent()
            .context("客户端没有父目录")?;
        for path in [&self.state_dir, &self.source] {
            ensure!(
                !resolve(path, root)?.starts_with(root),
                "配置与 state_dir 必须位于客户端目录之外"
            );
        }
        if let Some(prefix) = &self.client.wine_prefix {
            ensure!(self.client.runner.is_some(), "wine_prefix 需要配置 runner");
            ensure!(
                !resolve(prefix, root)?.starts_with(root),
                "Wine prefix 必须位于客户端目录之外"
            );
        }
        if let Some(hash) = &self.client.sha256 {
            ensure!(
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
                "sha256 必须是 64 位小写十六进制"
            );
        }
        ensure!(
            self.client.arguments.len() <= 64
                && self
                    .client
                    .arguments
                    .iter()
                    .all(|arg| arg.len() <= 4096 && !arg.contains('\0')),
            "arguments 最多 64 项，每项不超过 4096 字节且不能含 NUL"
        );
        ensure!(self.routes.len() <= 16, "最多允许 16 条转发规则");
        let mut names = HashSet::new();
        let mut listeners = HashSet::new();
        for route in &self.routes {
            ensure!(
                route.name.len() <= 32
                    && route
                        .name
                        .bytes()
                        .next()
                        .is_some_and(|b| b.is_ascii_lowercase())
                    && route.name.bytes().all(|b| b.is_ascii_lowercase()
                        || b.is_ascii_digit()
                        || b == b'_'
                        || b == b'-'),
                "route.name 必须是最多 32 位的英文标识"
            );
            ensure!(
                names.insert(&route.name) && listeners.insert(route.listen.literal()?),
                "转发名称或监听端点重复"
            );
            ensure!(
                route.listen.literal()?.ip().is_loopback(),
                "转发器只允许监听 IP 回环地址"
            );
            for endpoint in [&route.listen, &route.upstream] {
                ensure!(endpoint.port != 0, "转发端口不能为零");
                validate_host(&endpoint.host)?;
            }
        }
        ensure!(
            !self.routes.iter().any(|r| r
                .upstream
                .literal()
                .ok()
                .is_some_and(|a| listeners.contains(&a))),
            "上游不能指向任一转发监听端口，以免形成循环"
        );
        match self.routing {
            Routing::Direct => ensure!(self.routes.is_empty(), "direct 模式 routes 必须为空"),
            _ if self.routes.is_empty() => bail!("转发模式至少需要一条规则"),
            _ => {}
        }
        Ok(())
    }

    pub fn set_server(
        &mut self,
        host: Option<&str>,
        auth_port: Option<u16>,
        lobby_port: Option<u16>,
    ) -> Result<()> {
        if host.is_none() && auth_port.is_none() && lobby_port.is_none() {
            return Ok(());
        }
        ensure!(
            self.routing != Routing::Direct && !self.routes.is_empty(),
            "指定服务器需要转发模式；direct 不改变原客户端网络路径"
        );
        if let Some(host) = host {
            validate_host(host)?;
        }
        for (name, port) in [("auth", auth_port), ("lobby", lobby_port)] {
            if let Some(port) = port {
                ensure!(
                    port != 0 && self.routes.iter().any(|route| route.name == name),
                    "设置 {name} 端口需要同名规则和有效端口"
                );
            }
        }
        for route in &mut self.routes {
            if let Some(host) = host {
                route.upstream.host = host.to_owned();
            }
            if route.name == "auth"
                && let Some(port) = auth_port
            {
                route.upstream.port = port;
            }
            if route.name == "lobby"
                && let Some(port) = lobby_port
            {
                route.upstream.port = port;
            }
        }
        self.validate()
    }

    pub fn save_local(&self) -> Result<PathBuf> {
        let stem = self
            .source
            .file_stem()
            .context("配置文件没有名称")?
            .to_string_lossy();
        let name = if stem.ends_with(".local") {
            format!("{stem}.json")
        } else {
            format!("{stem}.local.json")
        };
        let destination = self.source.with_file_name(name);
        let mut saved = self.clone();
        saved.source = destination.clone();
        saved.validate()?;
        std::fs::write(&destination, serde_json::to_vec_pretty(&saved)?)?;
        Ok(destination)
    }

    pub fn command(&self) -> Vec<String> {
        let mut command = Vec::new();
        if let Some(runner) = &self.client.runner {
            command.push(runner.display().to_string());
        }
        command.push(self.client.executable.display().to_string());
        command.extend(self.client.arguments.clone());
        command
    }
}
