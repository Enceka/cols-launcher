use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use sysinfo::{
    Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, Signal, System, UpdateKind,
};

pub fn snapshot() -> System {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_exe(UpdateKind::Always)
            .with_cmd(UpdateKind::Always),
    );
    system
}

pub fn matches_image(image: &str, executable: &Path, prefix: Option<&Path>) -> bool {
    if Path::new(image).canonicalize().ok().as_deref() == Some(executable) {
        return true;
    }
    // Only translate a DOS Z: path when this prefix actually maps Z: to /.
    if let Some(prefix) = prefix
        && prefix.join("dosdevices/z:").canonicalize().ok().as_deref() == Some(Path::new("/"))
        && let Some(dos) = image
            .strip_prefix("Z:")
            .or_else(|| image.strip_prefix("z:"))
    {
        return PathBuf::from(dos.replace('\\', "/"))
            .canonicalize()
            .ok()
            .as_deref()
            == Some(executable);
    }
    false
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub pid: u32,
    pub started: u64,
    pub image: PathBuf,
}

impl Identity {
    pub fn from_pid(pid: u32) -> Result<Self> {
        let system = snapshot();
        let process = system.process(Pid::from_u32(pid)).context("进程已退出")?;
        Ok(Self {
            pid,
            started: process.start_time(),
            image: process.exe().context("无法读取进程路径")?.to_owned(),
        })
    }

    pub fn alive(&self) -> bool {
        let system = snapshot();
        system
            .process(Pid::from_u32(self.pid))
            .is_some_and(|process| {
                process.start_time() == self.started
                    && process.exe() == Some(self.image.as_path())
                    && process.status() != ProcessStatus::Zombie
            })
    }

    pub async fn stop(&self, graceful: bool) -> Result<()> {
        let system = snapshot();
        let Some(process) = system.process(Pid::from_u32(self.pid)) else {
            return Ok(());
        };
        if process.start_time() != self.started || process.exe() != Some(self.image.as_path()) {
            return Ok(());
        }
        let signal = if graceful {
            Signal::Interrupt
        } else {
            Signal::Term
        };
        if !process.kill_with(signal).unwrap_or(false) {
            ensure!(process.kill(), "无法关闭本次进程 {}", self.pid);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.alive() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if self.alive() {
            let system = snapshot();
            if let Some(process) = system.process(Pid::from_u32(self.pid))
                && process.start_time() == self.started
                && process.exe() == Some(self.image.as_path())
            {
                process.kill();
            }
            let until = Instant::now() + Duration::from_secs(1);
            while self.alive() && Instant::now() < until {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            ensure!(
                !self.alive(),
                "进程 {} 仍在系统等待中，终止尚未完成",
                self.pid
            );
        }
        Ok(())
    }
}

pub fn client_process(executable: &Path, prefix: Option<&Path>) -> Option<Identity> {
    let system = snapshot();
    system.processes().values().find_map(|process| {
        if process.status() == ProcessStatus::Zombie {
            return None;
        }
        let matching = process
            .exe()
            .is_some_and(|path| matches_image(&path.to_string_lossy(), executable, prefix))
            || process
                .cmd()
                .first()
                .is_some_and(|arg| matches_image(&arg.to_string_lossy(), executable, prefix));
        if matching && let Some(image) = process.exe() {
            Some(Identity {
                pid: process.pid().as_u32(),
                started: process.start_time(),
                image: image.to_owned(),
            })
        } else {
            None
        }
    })
}
