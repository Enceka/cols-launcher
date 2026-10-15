//! Validate the server's process-local startup contract before client writes.
use crate::native::LocalProfile;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::Read;
use std::net::SocketAddr;
use std::path::Path;

#[derive(Clone, Copy)]
pub(crate) enum Kind {
    NativeServices,
    Resources,
}

#[derive(Deserialize)]
struct Record {
    schema_version: u32,
    pid: u32,
    client_sha256: String,
    #[serde(flatten)]
    service: Service,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Service {
    NativeServices {
        lobby_protocol: u32,
        game_protocol: u32,
        title_id: u32,
        auth: SocketAddr,
        lobby: SocketAddr,
        stun: SocketAddr,
        stun_alt: SocketAddr,
        game: SocketAddr,
        continuous_play: bool,
        #[serde(default)]
        room_bot_fill: Option<u8>,
        game_state_loaded: bool,
        map_catalog_loaded: bool,
    },
    TcpObserver {
        listeners: BTreeMap<String, SocketAddr>,
    },
}

pub(crate) fn verify(path: &Path, pid: u32, kind: Kind, profile: &LocalProfile) -> Result<bool> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.len() <= 16 * 1024,
        "服务端就绪记录必须是小于 16 KiB 的普通文件"
    );
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(16 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 16 * 1024, "服务端就绪记录过大");
    let record: Record = serde_json::from_slice(&bytes).context("服务端就绪记录格式不兼容")?;
    ensure!(
        record.schema_version == 1,
        "服务端启动契约版本不兼容，请更新服务端和启动器"
    );
    ensure!(record.pid == pid, "服务端就绪记录不属于本次进程");
    ensure!(
        profile.client.sha256.as_deref() == Some(record.client_sha256.as_str()),
        "服务端支持的客户端 SHA-256 与本地基线不匹配"
    );
    match (kind, record.service) {
        (
            Kind::NativeServices,
            Service::NativeServices {
                lobby_protocol,
                game_protocol,
                title_id,
                auth,
                lobby,
                stun,
                stun_alt,
                game,
                continuous_play,
                room_bot_fill,
                game_state_loaded,
                map_catalog_loaded,
            },
        ) => {
            ensure!(
                lobby_protocol == 210 && game_protocol == 3,
                "服务端原生协议版本不兼容"
            );
            ensure!(
                title_id == profile.backend.title_id,
                "服务端 title_id 不匹配"
            );
            for (actual, port) in [
                (auth, profile.backend.auth_port),
                (lobby, profile.backend.lobby_port),
                (stun, profile.backend.stun_port),
                (stun_alt, profile.backend.stun_alt_port),
                (game, profile.backend.game_port),
            ] {
                ensure!(
                    actual == SocketAddr::V4(profile.address(port)),
                    "服务端就绪地址与本次连接配置不匹配"
                );
            }
            ensure!(
                continuous_play
                    && room_bot_fill == Some(profile.backend.room_bot_fill)
                    && game_state_loaded
                    && map_catalog_loaded,
                "服务端没有完成本地连续对局、地图和 Bot 房间装配"
            );
        }
        (Kind::Resources, Service::TcpObserver { listeners }) => {
            ensure!(
                listeners.len() == 1
                    && listeners.get("resources")
                        == Some(&SocketAddr::V4(
                            profile.address(profile.backend.resource_port)
                        )),
                "资源观察端点与本次连接配置不匹配"
            );
        }
        _ => anyhow::bail!("服务端就绪记录的服务类型不匹配"),
    }
    Ok(true)
}
