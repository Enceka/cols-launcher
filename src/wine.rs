//! Wine prefix health checks and recovery of dead macOS IPC lock owners.
use serde::Serialize;
use std::path::PathBuf;

#[derive(Clone, Debug, Default, Serialize)]
pub struct RecoveryReport {
    pub probe_passed: bool,
    pub managed_prefix: bool,
    pub retired_ipc: Option<PathBuf>,
    pub stale_pid: Option<u32>,
}

#[cfg(target_os = "macos")]
pub use macos::Runtime;

#[cfg(not(target_os = "macos"))]
pub struct Runtime {
    pub report: RecoveryReport,
}

#[cfg(not(target_os = "macos"))]
impl Runtime {
    pub async fn prepare(
        _: &crate::profile::Client,
        _: &std::path::Path,
        _: &crate::relay::Event,
    ) -> anyhow::Result<Option<Self>> {
        Ok(None)
    }
    pub fn observe_server(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    pub async fn shutdown(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use super::RecoveryReport;
    use crate::{process::Identity, profile::Client, relay::Event};
    use anyhow::{Context, Result, bail, ensure};
    use nix::fcntl::{FcntlArg, fcntl};
    use nix::unistd::getuid;
    use std::fs::{File, OpenOptions};
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    use std::path::{Path, PathBuf};
    use std::process::{Output, Stdio};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tokio::process::Command;

    pub struct Runtime {
        pub report: RecoveryReport,
        runner: PathBuf,
        prefix: PathBuf,
        ipc: PathBuf,
        server: Option<Identity>,
        _lease: File,
        _legacy_lease: Option<File>,
    }

    // Wine 10 derives this path from the prefix's filesystem identity.
    fn ipc_path(prefix: &Path, base: &Path) -> Result<PathBuf> {
        let info = prefix.metadata()?;
        ensure!(
            info.uid() == getuid().as_raw(),
            "Wine prefix 不属于当前用户"
        );
        Ok(base.join(format!("server-{:x}-{:x}", info.dev(), info.ino())))
    }

    fn check_directory(path: &Path) -> Result<()> {
        let info = std::fs::symlink_metadata(path)?;
        ensure!(
            info.is_dir()
                && !info.file_type().is_symlink()
                && info.uid() == getuid().as_raw()
                && info.mode() & 0o077 == 0,
            "Wine 通信目录类型、权限或所有者不匹配"
        );
        Ok(())
    }

    fn lock_owner(ipc: &Path) -> Result<Option<u32>> {
        if !ipc.exists() {
            return Ok(None);
        }
        check_directory(ipc.parent().context("缺少 IPC 父目录")?)?;
        check_directory(ipc)?;
        let path = ipc.join("lock");
        if !path.exists() {
            return Ok(None);
        }
        let info = std::fs::symlink_metadata(&path)?;
        ensure!(
            info.is_file() && !info.file_type().is_symlink() && info.uid() == getuid().as_raw(),
            "Wine 锁文件身份不匹配"
        );
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut lock = nix::libc::flock {
            l_start: 0,
            l_len: 1,
            l_pid: 0,
            l_type: nix::libc::F_WRLCK as _,
            l_whence: nix::libc::SEEK_SET as _,
        };
        fcntl(&file, FcntlArg::F_GETLK(&mut lock))?;
        Ok(
            (lock.l_type == nix::libc::F_WRLCK as nix::libc::c_short && lock.l_pid > 0)
                .then_some(lock.l_pid as u32),
        )
    }

    fn stale_pid(stderr: &str) -> Option<u32> {
        if !stderr.contains("a wine server seems to be running, but I cannot connect to it.") {
            return None;
        }
        let remainder = stderr.split_once("it might be pid ")?.1;
        let number = remainder.split_once(')')?.0;
        number.parse::<u32>().ok().filter(|pid| *pid > 1)
    }

    fn exiting_owner(row: &str, pid: u32, uid: u32) -> bool {
        let fields: Vec<_> = row.split_whitespace().collect();
        if fields.len() < 5 {
            return false;
        }
        let command = fields[4..].join(" ");
        let server_name = Path::new(&command)
            .file_name()
            .and_then(|name| name.to_str());
        fields[0].parse::<u32>().ok() == Some(uid)
            && fields[1].parse::<u32>().ok() == Some(pid)
            && fields[2] == "0"
            && fields[3].contains('E')
            && !fields[3].contains('U')
            && matches!(server_name, Some("wineserver" | "(wineserver)"))
    }

    async fn confirm_exiting(pid: u32) -> Result<()> {
        let mut command = Command::new("/bin/ps");
        command.args(["-p", &pid.to_string(), "-o", "uid=,pid=,rss=,stat=,comm="]);
        let output = bounded(command, 3).await?;
        ensure!(
            output.status.success()
                && exiting_owner(
                    &String::from_utf8_lossy(&output.stdout),
                    pid,
                    getuid().as_raw()
                ),
            "Wine 锁持有者仍可能运行；不隔离活动环境的通信目录"
        );
        Ok(())
    }

    fn retire(ipc: &Path, expected_pid: u32) -> Result<PathBuf> {
        check_directory(ipc.parent().context("缺少 IPC 父目录")?)?;
        check_directory(ipc)?;
        for item in std::fs::read_dir(ipc)? {
            let item = item?;
            let kind = item.file_type()?;
            ensure!(
                (item.file_name() == "lock" && kind.is_file())
                    || (item.file_name() == "socket" && kind.is_socket()),
                "Wine IPC 目录含未知内容，不自动处理"
            );
        }
        ensure!(
            lock_owner(ipc)? == Some(expected_pid),
            "Wine 锁持有者已改变"
        );
        let suffix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let retired = ipc.with_file_name(format!(
            "{}.cols-retired-{suffix}",
            ipc.file_name().context("缺少 IPC 名称")?.to_string_lossy()
        ));
        std::fs::rename(ipc, &retired)?;
        Ok(retired)
    }

    async fn bounded(mut command: Command, seconds: u64) -> Result<Output> {
        command
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        tokio::time::timeout(Duration::from_secs(seconds), command.output())
            .await
            .context("Wine 管理命令超时；已停止等待")?
            .context("无法执行 Wine 管理命令")
    }

    impl Runtime {
        fn command(&self, executable: &Path) -> Command {
            let mut command = Command::new(executable);
            command
                .env("WINEPREFIX", &self.prefix)
                .env("WINEDEBUG", "-all");
            command
        }

        async fn probe(&self) -> Result<Output> {
            let mut command = self.command(&self.runner);
            // Resolve cmd through Wine; some WoW64 prefixes have no syswow64/cmd.exe.
            command.args(["cmd", "/d", "/c", "exit", "0"]);
            bounded(command, 12).await
        }

        pub async fn prepare(client: &Client, output: &Path, emit: &Event) -> Result<Option<Self>> {
            let (Some(runner), Some(prefix)) = (&client.runner, &client.wine_prefix) else {
                return Ok(None);
            };
            let runner = runner.canonicalize()?;
            let prefix = prefix.canonicalize()?;
            let base = PathBuf::from(format!("/private/tmp/.wine-{}", getuid()));
            let ipc = ipc_path(&prefix, &base)?;
            let key = format!(
                "{:x}-{:x}",
                prefix.metadata()?.dev(),
                prefix.metadata()?.ino()
            );
            let lease = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(std::env::temp_dir().join(format!("cols-wine-{key}.lock")))?;
            lease
                .try_lock()
                .context("该 Wine 环境正被另一个启动器使用")?;
            let legacy = prefix.join(".codol-lock");
            let legacy_lease = if legacy.is_file() {
                let lock = OpenOptions::new().read(true).write(true).open(legacy)?;
                lock.try_lock().context("该 Wine 环境正由项目工具使用")?;
                Some(lock)
            } else {
                None
            };
            let marker = prefix.join(".codol-prefix.json");
            let managed = if marker.is_file() && marker.metadata()?.len() <= 4096 {
                let data: serde_json::Value = serde_json::from_slice(&std::fs::read(marker)?)?;
                data["purpose"] == "CODOL research"
                    && data["engine_id"].as_str().is_some_and(|id| {
                        runner
                            .parent()
                            .and_then(Path::parent)
                            .and_then(Path::file_name)
                            .is_some_and(|name| name == id)
                    })
            } else {
                false
            };
            let mut runtime = Self {
                report: RecoveryReport {
                    managed_prefix: managed,
                    ..Default::default()
                },
                runner,
                prefix,
                ipc,
                server: None,
                _lease: lease,
                _legacy_lease: legacy_lease,
            };
            emit("检查 Wine 运行环境…");
            let first = runtime.probe().await;
            if !first.as_ref().is_ok_and(|output| output.status.success()) {
                let pid = match first {
                    Ok(output) => {
                        let error = String::from_utf8_lossy(&output.stderr);
                        stale_pid(&error).context(format!("Wine 基础检查失败：{}", error.trim()))?
                    }
                    Err(error) => lock_owner(&runtime.ipc)?.context(format!("{error:#}"))?,
                };
                ensure!(
                    lock_owner(&runtime.ipc)? == Some(pid),
                    "Wine 报告的旧锁与当前环境不匹配"
                );
                confirm_exiting(pid).await?;
                runtime.report.retired_ipc = Some(retire(&runtime.ipc, pid)?);
                runtime.report.stale_pid = Some(pid);
                std::fs::write(
                    output.join("wine-runtime.json"),
                    serde_json::to_vec_pretty(&runtime.report)?,
                )?;
                emit("已保留并隔离失效的 Wine 通信目录，重新检查…");
                let second = runtime.probe().await?;
                ensure!(
                    second.status.success(),
                    "Wine 通信恢复后检查仍失败：{}",
                    String::from_utf8_lossy(&second.stderr).trim()
                );
            }
            runtime.report.probe_passed = true;
            std::fs::write(
                output.join("wine-runtime.json"),
                serde_json::to_vec_pretty(&runtime.report)?,
            )?;
            Ok(Some(runtime))
        }

        pub fn observe_server(&mut self) -> Result<()> {
            if let Some(pid) = lock_owner(&self.ipc)?
                && self.server.as_ref().is_none_or(|server| server.pid != pid)
            {
                let identity = Identity::from_pid(pid)?;
                ensure!(
                    identity
                        .image
                        .file_name()
                        .is_some_and(|name| name == "wineserver"),
                    "Wine 锁持有者不是 wineserver"
                );
                self.server = Some(identity);
            }
            Ok(())
        }

        pub async fn shutdown(&mut self) -> Result<()> {
            if !self.report.managed_prefix {
                return Ok(());
            }
            let Some(server) = &self.server else {
                return Ok(());
            };
            let Some(owner) = lock_owner(&self.ipc)? else {
                return Ok(());
            };
            ensure!(
                owner == server.pid,
                "Wine 环境已由其他服务进程接管，不终止该实例"
            );
            if !server.alive() {
                return Ok(());
            }
            let executable = self
                .runner
                .parent()
                .context("Wine 程序没有父目录")?
                .join("wineserver");
            ensure!(executable.is_file(), "缺少匹配的 wineserver");
            let mut command = self.command(&executable);
            command.arg("-k");
            let output = bounded(command, 12).await?;
            ensure!(
                output.status.success(),
                "Wine 环境关闭失败：{}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            let mut command = self.command(&executable);
            command.arg("-w");
            let waited = bounded(command, 3).await?;
            if !waited.status.success() {
                bail!("Wine 环境尚未完成退出");
            }
            Ok(())
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        #[test]
        fn only_exact_stale_server_errors_yield_a_pid() {
            let error = "wine: a wine server seems to be running, but I cannot connect to it.\n   You probably need to kill that process (it might be pid 123).";
            assert_eq!(stale_pid(error), Some(123));
            assert_eq!(stale_pid("it might be pid 123)"), None);
            assert_eq!(stale_pid(&error.replace("123", "1")), None);
            assert_eq!(stale_pid(&error.replace("123", "12; rm")), None);
        }

        #[test]
        fn active_or_foreign_processes_are_not_retirable() {
            assert!(exiting_owner("501 123 0 ?Es (wineserver)", 123, 501));
            for row in [
                "501 123 64 ?Es (wineserver)",
                "501 123 0 S wineserver",
                "501 123 0 U wineserver",
                "501 123 0 UE wineserver",
                "501 123 0 Z wineserver",
                "502 123 0 ?Es (wineserver)",
                "501 124 0 ?Es (wineserver)",
                "501 123 0 ?Es another-app",
                "501 123 0 ?Es not-wineserver",
            ] {
                assert!(!exiting_owner(row, 123, 501), "{row}");
            }
        }

        #[test]
        fn ipc_identity_is_per_prefix_and_symlinked_directories_are_rejected() {
            let temp = tempfile::tempdir().unwrap();
            let prefix = temp.path().join("prefix");
            std::fs::create_dir(&prefix).unwrap();
            let base = temp.path().join("ipc");
            std::fs::create_dir(&base).unwrap();
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
            let directory = ipc_path(&prefix, &base).unwrap();
            let other = temp.path().join("other");
            std::fs::create_dir(&other).unwrap();
            assert_ne!(directory, ipc_path(&other, &base).unwrap());
            std::os::unix::fs::symlink(&other, &directory).unwrap();
            assert!(lock_owner(&directory).is_err());
        }

        #[test]
        fn free_or_unexpected_lock_is_never_retired() {
            let temp = tempfile::tempdir().unwrap();
            std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let directory = temp.path().join("server");
            std::fs::create_dir(&directory).unwrap();
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
            File::create(directory.join("lock")).unwrap();
            assert_eq!(lock_owner(&directory).unwrap(), None);
            assert!(retire(&directory, 123).is_err());
            assert!(directory.exists());
            std::fs::write(directory.join("important-data"), "keep").unwrap();
            assert!(retire(&directory, 123).is_err());
            assert_eq!(
                std::fs::read_to_string(directory.join("important-data")).unwrap(),
                "keep"
            );
        }
    }
}
