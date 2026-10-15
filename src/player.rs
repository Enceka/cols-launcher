//! Player creation is delegated to the backend CLI; settings apply once.
use crate::native::LocalProfile;
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlayerSettings {
    pub nickname: String,
    pub level: u8,
    pub unlock_all: bool,
    /// Stable native login handle; independent of the visible nickname.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_name: Option<String>,
}
impl Default for PlayerSettings {
    fn default() -> Self {
        Self {
            nickname: "Player".into(),
            level: 1,
            unlock_all: false,
            login_name: None,
        }
    }
}
impl PlayerSettings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.nickname.trim().is_empty()
                && self.nickname == self.nickname.trim()
                && self.nickname.len() <= 31
                && !self
                    .nickname
                    .chars()
                    .any(|c| c.is_control() || matches!(c, '"' | '\\' | ';' | '^')),
            "昵称需为 1～31 个 UTF-8 字节，不能包含控制字符、引号、反斜杠、分号或颜色代码"
        );
        ensure!((1..=70).contains(&self.level), "初始等级需为 1～70");
        ensure!(
            self.login_name.as_ref().is_none_or(|name| !name.is_empty()
                && name.len() <= 15
                && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')),
            "本地登录标识需为 1～15 个字母、数字或下划线"
        );
        Ok(())
    }
    pub fn client_arguments(&self, original: &[String]) -> Vec<String> {
        let mut arguments = Vec::new();
        let mut index = 0;
        while index < original.len() {
            if original[index] == "+set"
                && original
                    .get(index + 1)
                    .is_some_and(|name| matches!(name.as_str(), "name" | "dw_username"))
            {
                index = (index + 3).min(original.len());
            } else {
                arguments.push(original[index].clone());
                index += 1;
            }
        }
        let name = STANDARD.encode(&self.nickname);
        arguments.extend(["+set".into(), "name".into(), name]);
        if let Some(login) = &self.login_name {
            arguments.extend(["+set".into(), "dw_username".into(), login.clone()]);
        }
        arguments
    }
}

pub fn template(profile: &LocalProfile) -> PathBuf {
    profile.backend.player_template.clone().unwrap_or_else(|| {
        profile
            .backend
            .service_data_dir
            .join("new-player-template.json")
    })
}

pub async fn create(profile: &LocalProfile, mut settings: PlayerSettings) -> Result<PathBuf> {
    profile.validate()?;
    settings.validate()?;
    let report = crate::native::plan(profile)?;
    ensure!(report.launch_allowed, "{}", report.blockers.join("；"));
    let _lock = crate::native_files::ClientLocks::acquire(profile.root())?;
    let template = template(profile);
    ensure!(
        template.is_file(),
        "缺少新玩家初始资料：{}",
        template.display()
    );
    let parent = profile.state_dir.join("players");
    std::fs::create_dir_all(&parent)?;
    let parent = parent.canonicalize()?;
    ensure!(
        !parent.starts_with(profile.root()),
        "玩家存档必须位于客户端目录之外"
    );
    let mut command = tokio::process::Command::new(&profile.backend.executable);
    command
        .arg("create-local-player")
        .arg("--source-data-dir")
        .arg(&profile.backend.service_data_dir)
        .arg("--template")
        .arg(&template)
        .arg("--output-parent")
        .arg(&parent)
        .arg("--title-id")
        .arg(profile.backend.title_id.to_string())
        .arg("--nickname")
        .arg(&settings.nickname)
        .arg("--level")
        .arg(settings.level.to_string())
        .kill_on_drop(true);
    if settings.unlock_all {
        command.arg("--unlock-all");
    }
    let output = tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .context("创建玩家超时")??;
    ensure!(
        output.status.success(),
        "创建玩家失败：{}",
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(output.stdout.len() <= 4096, "创建玩家响应超出限制");
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Created {
        player_directory: PathBuf,
    }
    let created: Created =
        serde_json::from_slice(&output.stdout).context("服务端返回了无效的玩家创建结果")?;
    let directory = created.player_directory.canonicalize()?;
    ensure!(directory.parent() == Some(parent.as_path()) && directory.file_name().and_then(|name| name.to_str())
        .is_some_and(|name| name.len() == 32 && name.bytes().all(|c| c.is_ascii_hexdigit())), "服务端返回了无效的玩家目录");
    let mut configured = profile.clone();
    let id = directory.file_name().unwrap().to_str().unwrap();
    settings.login_name = Some(format!("c{}", &id[..14]));
    configured.backend.service_data_dir = directory.join("server-data");
    configured.backend.player_template = Some(template.canonicalize()?);
    configured.state_dir = directory.join("state");
    configured.player = Some(settings);
    let config = directory.join("native.json");
    configured.save(&config)?;
    Ok(config)
}

pub fn load_settings(path: &Path) -> PlayerSettings {
    LocalProfile::load(path)
        .ok()
        .and_then(|profile| profile.player)
        .unwrap_or_default()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedPlayer {
    pub nickname: String,
    pub config: PathBuf,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    selected: PathBuf,
    players: Vec<SavedPlayer>,
}
fn selection_path() -> PathBuf {
    PathBuf::from("state/players.json")
}
fn selection() -> Selection {
    let path = selection_path();
    if path
        .metadata()
        .is_ok_and(|metadata| metadata.len() <= 64 * 1024)
    {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    } else {
        Selection::default()
    }
}
pub fn saved_players() -> Vec<SavedPlayer> {
    selection()
        .players
        .into_iter()
        .filter(|player| LocalProfile::load(&player.config).is_ok())
        .collect()
}
pub fn last_config(fallback: &Path) -> PathBuf {
    let selected = selection().selected;
    if LocalProfile::load(&selected).is_ok() {
        selected
    } else {
        fallback.to_owned()
    }
}
pub fn remember(path: &Path) -> Result<()> {
    let profile = LocalProfile::load(path)?;
    let mut selection = selection();
    let config = path.canonicalize()?;
    if !selection
        .players
        .iter()
        .any(|player| player.config == config)
    {
        ensure!(selection.players.len() < 128, "已保存玩家数量超过限制");
        selection.players.push(SavedPlayer {
            nickname: profile
                .player
                .as_ref()
                .map_or_else(|| "原有账号".into(), |player| player.nickname.clone()),
            config: config.clone(),
        });
    }
    selection.selected = config;
    let path = selection_path();
    std::fs::create_dir_all(path.parent().context("缺少玩家列表目录")?)?;
    ensure!(
        !path
            .parent()
            .unwrap()
            .canonicalize()?
            .starts_with(profile.root()),
        "玩家列表必须位于客户端目录之外"
    );
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temporary, serde_json::to_vec_pretty(&selection)?)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_allow_duplicates_and_unicode_but_cannot_change_command_structure() {
        let mut settings = PlayerSettings {
            nickname: "同名玩家".into(),
            login_name: Some("c123456789abcde".into()),
            ..Default::default()
        };
        settings.validate().unwrap();
        let old = [
            "+set",
            "name",
            "Old",
            "+set",
            "r_fullscreen",
            "0",
            "+set",
            "dw_username",
            "Old",
        ]
        .map(String::from)
        .to_vec();
        let arguments = settings.client_arguments(&old);
        assert_eq!(&arguments[..3], &["+set", "r_fullscreen", "0"]);
        assert_eq!(arguments.len(), 9);
        assert_eq!(
            STANDARD.decode(&arguments[5]).unwrap(),
            "同名玩家".as_bytes()
        );
        assert_eq!(arguments[8], "c123456789abcde");
        for name in [
            "", "   ", "a\nb", "\"bad", "a\\b", "a;b", "^1bad", " leading",
        ] {
            settings.nickname = name.into();
            assert!(settings.validate().is_err());
        }
        settings.nickname = "字".repeat(11);
        assert!(settings.validate().is_err());
        settings.nickname = "Player".into();
        for level in [0, 71, 255] {
            settings.level = level;
            assert!(settings.validate().is_err());
        }
    }
}
