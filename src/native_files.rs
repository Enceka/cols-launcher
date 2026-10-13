//! A persistent journal for the five explicitly permitted configuration files.
use crate::inspection::sha256;
use crate::process::Identity;
use crate::profile::resolve;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const EDITABLE: &[&str] = &[
    "dwauth.ini",
    "stun.ini",
    "iips.ini",
    "cacert.pem",
    "IIPS/cacert.pem",
];

pub struct ClientLocks {
    _global: File,
    _legacy: Option<File>,
}

impl ClientLocks {
    pub fn acquire(root: &Path) -> Result<Self> {
        let key = format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()));
        let global = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(std::env::temp_dir().join(format!("cols-native-{key}.lock")))?;
        global.try_lock().context("该客户端已有启动器会话运行")?;
        let legacy = root.join(".codol-client-lock");
        let legacy = if legacy.is_file() {
            let lock = OpenOptions::new().read(true).write(true).open(legacy)?;
            lock.try_lock().context("该客户端正由现有研究工具使用")?;
            Some(lock)
        } else {
            None
        };
        Ok(Self {
            _global: global,
            _legacy: legacy,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    name: String,
    before: Option<String>,
    applied: String,
    backup: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Journal {
    version: u32,
    root: PathBuf,
    pub owner: Identity,
    pub processes: Vec<Identity>,
    entries: Vec<Entry>,
    #[serde(skip)]
    path: PathBuf,
}

pub fn journal_path(state: &Path, root: &Path) -> PathBuf {
    let key = format!("{:x}", Sha256::digest(root.as_os_str().as_encoded_bytes()));
    state.join(format!("recovery-{}.json", &key[..24]))
}

fn checked_file(root: &Path, name: &str) -> Result<PathBuf> {
    ensure!(EDITABLE.contains(&name), "恢复记录包含不允许改写的文件");
    let path = root.join(name);
    ensure!(
        path.parent().context("配置没有父目录")?.is_dir(),
        "客户端配置目录不存在：{name}"
    );
    ensure!(
        resolve(path.parent().unwrap(), root)?.starts_with(root),
        "配置父目录通过链接指向客户端之外：{name}"
    );
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "不改写链接或非普通文件：{name}"
        );
    }
    Ok(path)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_file_name(format!(
        ".{}.cols-{}.tmp",
        path.file_name().context("缺少文件名")?.to_string_lossy(),
        std::process::id()
    ));
    let result: Result<()> = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        if let Ok(metadata) = path.metadata() {
            file.set_permissions(metadata.permissions())?;
        }
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        #[cfg(unix)]
        if let Some(parent) = path.parent() {
            File::open(parent)?.sync_all()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

impl Journal {
    pub fn prepare(
        root: &Path,
        state: &Path,
        output: &Path,
        changes: &[(String, Vec<u8>)],
    ) -> Result<Self> {
        let path = journal_path(state, root);
        ensure!(
            !path.exists(),
            "发现未恢复的客户端配置，请先运行 native-recover"
        );
        ensure!(
            changes.len() == EDITABLE.len(),
            "必须记录完整的允许修改文件集合"
        );
        ensure!(
            changes
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<std::collections::HashSet<_>>()
                .len()
                == EDITABLE.len(),
            "允许修改文件不能重复"
        );
        let mut entries = Vec::new();
        for (index, (name, applied)) in changes.iter().enumerate() {
            let target = checked_file(root, name)?;
            let (before, backup) = if target.exists() {
                ensure!(
                    target.metadata()?.len() <= 2 * 1024 * 1024,
                    "配置文件过大：{name}"
                );
                let original = std::fs::read(&target)?;
                let backup = output.join(format!("config-{index}.backup"));
                atomic_write(&backup, &original)?;
                (
                    Some(format!("{:x}", Sha256::digest(&original))),
                    Some(backup),
                )
            } else {
                (None, None)
            };
            entries.push(Entry {
                name: name.clone(),
                before,
                applied: format!("{:x}", Sha256::digest(applied)),
                backup,
            });
        }
        let journal = Self {
            version: 1,
            root: root.to_owned(),
            owner: Identity::from_pid(std::process::id())?,
            processes: Vec::new(),
            entries,
            path,
        };
        journal.save()?;
        // The complete journal and backups are durable before the first client write.
        Ok(journal)
    }

    pub fn apply(&self, changes: &[(String, Vec<u8>)]) -> Result<()> {
        for (name, bytes) in changes {
            let entry = self
                .entries
                .iter()
                .find(|entry| &entry.name == name)
                .context("缺少恢复记录")?;
            ensure!(
                format!("{:x}", Sha256::digest(bytes)) == entry.applied,
                "配置修改与恢复记录不一致"
            );
            let path = checked_file(&self.root, name)?;
            let current = if path.is_file() {
                Some(sha256(&path)?)
            } else {
                None
            };
            ensure!(
                current == entry.before,
                "配置在备份后被其他程序修改：{name}"
            );
            if current.as_deref() == Some(entry.applied.as_str()) {
                continue;
            }
            atomic_write(&path, bytes)?;
        }
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        atomic_write(&self.path, &serde_json::to_vec_pretty(self)?)
    }

    pub fn load(path: &Path, root: &Path, state: &Path) -> Result<Self> {
        ensure!(path.metadata()?.len() <= 64 * 1024, "恢复记录过大");
        let mut journal: Self = serde_json::from_slice(&std::fs::read(path)?)?;
        ensure!(
            journal.version == 1 && journal.root == root && journal.entries.len() == EDITABLE.len(),
            "恢复记录身份无效"
        );
        let names: std::collections::HashSet<_> = journal
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        ensure!(
            names.len() == EDITABLE.len() && EDITABLE.iter().all(|name| names.contains(name)),
            "恢复记录文件集合无效"
        );
        for entry in &journal.entries {
            checked_file(root, &entry.name)?;
            if let Some(backup) = &entry.backup {
                ensure!(
                    resolve(backup, state)?.starts_with(state) && entry.before.is_some(),
                    "备份位置无效"
                );
                ensure!(
                    Some(sha256(backup)?) == entry.before,
                    "备份摘要不一致：{}",
                    entry.name
                );
            } else {
                ensure!(entry.before.is_none(), "恢复记录缺少备份");
            }
        }
        journal.path = path.to_owned();
        Ok(journal)
    }

    pub fn restore(&self) -> Result<()> {
        let mut conflicts = Vec::new();
        // Check every file before restoring any, so a conflict is reviewable.
        for entry in &self.entries {
            let path = checked_file(&self.root, &entry.name)?;
            let current = if path.is_file() {
                Some(sha256(&path)?)
            } else {
                None
            };
            if current != entry.before && current.as_deref() != Some(entry.applied.as_str()) {
                conflicts.push(entry.name.as_str());
            }
        }
        if !conflicts.is_empty() {
            bail!("配置被外部修改，保留备份且未覆盖：{}", conflicts.join(", "));
        }
        for entry in &self.entries {
            let path = checked_file(&self.root, &entry.name)?;
            let current = if path.is_file() {
                Some(sha256(&path)?)
            } else {
                None
            };
            if current == entry.before {
                continue;
            }
            if let Some(backup) = &entry.backup {
                ensure!(
                    Some(sha256(backup)?) == entry.before,
                    "恢复备份摘要无效：{}",
                    entry.name
                );
                atomic_write(&path, &std::fs::read(backup)?)?;
            } else if path.exists() {
                std::fs::remove_file(path)?;
            }
        }
        std::fs::remove_file(&self.path)?;
        Ok(())
    }
}
