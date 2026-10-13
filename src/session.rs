use crate::inspection::{Inventory, inspect, inventory};
use crate::profile::Profile;
use crate::relay::{Event, RelayGroup};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::Child;
use tokio::sync::watch;

#[derive(Debug, Serialize)]
pub struct SessionRecord {
    pub client: String,
    pub pid: Option<u32>,
    pub before: Inventory,
    pub after: Inventory,
    pub changed_files: Vec<String>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
}

fn lock(profile: &Profile) -> Result<File> {
    let key = format!(
        "{:x}",
        Sha256::digest(profile.client.executable.as_os_str().as_encoded_bytes())
    );
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(profile.state_dir.join(format!("{}.lock", &key[..24])))?;
    file.try_lock().context("该客户端已有启动器会话运行")?;
    Ok(file)
}

async fn stop_child(child: &mut Child) -> Result<()> {
    if child.try_wait()?.is_none() {
        child.start_kill()?;
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .context("等待客户端退出超时")??;
    }
    Ok(())
}

pub async fn run_session(
    profile: &Profile,
    mut stop: watch::Receiver<bool>,
    emit: Event,
) -> Result<SessionRecord> {
    let report = inspect(profile)?;
    ensure!(report.launch_allowed, "{}", report.blockers.join("；"));
    ensure!(!*stop.borrow(), "会话已取消");
    // Recheck symlink containment before and after making the external directory.
    profile.validate()?;
    std::fs::create_dir_all(&profile.state_dir)?;
    profile.validate()?;
    let _lock = lock(profile)?;
    let before = inventory(profile)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let output = profile
        .state_dir
        .join(format!("session-{stamp}-{}", std::process::id()));
    std::fs::create_dir(&output)?;
    let log = Arc::new(Mutex::new((
        File::create(output.join("launcher.log"))?,
        0_usize,
    )));
    let external_emit = emit.clone();
    let events: Event = Arc::new(move |message| {
        if let Ok(mut locked) = log.lock() {
            // Bounded metadata log. No packet bytes, argv or child stdout.
            if locked.1 + message.len() < 256 * 1024 {
                let _ = writeln!(locked.0, "{message}");
                locked.1 += message.len() + 1;
            }
        }
        external_emit(message);
    });
    let mut record = SessionRecord {
        client: profile.client.executable.display().to_string(),
        pid: None,
        before,
        after: Inventory::new(),
        changed_files: Vec::new(),
        exit_code: None,
        error: None,
    };
    let mut relay = None;
    let mut child = None;
    let result: Result<()> = async {
        relay = Some(RelayGroup::start(&profile.routes, events.clone()).await?);
        let client = &profile.client;
        let mut command =
            tokio::process::Command::new(client.runner.as_ref().unwrap_or(&client.executable));
        if client.runner.is_some() {
            command.arg(&client.executable);
        }
        command
            .args(&client.arguments)
            .current_dir(client.executable.parent().context("缺少客户端父目录")?)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        if let Some(prefix) = &client.wine_prefix {
            command.env("WINEPREFIX", prefix);
        }
        child = Some(command.spawn().context("无法启动原生客户端程序")?);
        let running = child.as_mut().context("缺少子进程")?;
        record.pid = running.id();
        events(&format!(
            "客户端已启动 PID={}；登录结果需由客户端确认",
            record.pid.unwrap_or_default()
        ));
        std::fs::write(
            output.join("session.json"),
            serde_json::to_vec_pretty(&record)?,
        )?;
        if !*stop.borrow() {
            tokio::select! {
                status = running.wait() => { record.exit_code = status?.code(); },
                _ = stop.changed() => {},
            }
        }
        Ok(())
    }
    .await;
    let cleanup = if let Some(child) = child.as_mut() {
        stop_child(child).await
    } else {
        Ok(())
    };
    if let Some(relay) = relay.as_mut() {
        relay.close().await;
    }
    if record.exit_code.is_none() {
        record.exit_code = child
            .as_mut()
            .and_then(|child| child.try_wait().ok().flatten())
            .and_then(|status| status.code());
    }
    record.error = result
        .as_ref()
        .err()
        .or_else(|| cleanup.as_ref().err())
        .map(|error| format!("{error:#}"));
    record.after = inventory(profile)?;
    record.changed_files = record
        .before
        .iter()
        .filter(|(name, hash)| record.after.get(*name) != Some(*hash))
        .map(|(name, _)| name.clone())
        .collect();
    std::fs::write(
        output.join("session.json"),
        serde_json::to_vec_pretty(&record)?,
    )?;
    events(&format!(
        "会话结束；受检文件变更：{}",
        if record.changed_files.is_empty() {
            "无".to_owned()
        } else {
            record.changed_files.join(", ")
        }
    ));
    result?;
    cleanup?;
    Ok(record)
}
