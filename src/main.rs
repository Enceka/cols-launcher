use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use cols_launcher::{inspection, native, profile::Profile, relay::RelayGroup, session};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::watch;

#[derive(Clone, Copy, ValueEnum)]
enum Action {
    Inspect,
    Plan,
    Launch,
    Relay,
    Gui,
    NativeInit,
    NativeCreatePlayer,
    NativePlan,
    NativeLaunch,
    NativeRecover,
}

#[derive(Parser)]
#[command(version, about = "COLS启动器")]
struct Cli {
    #[arg(value_enum)]
    action: Action,
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(
        long,
        help = "native-init 使用的本机工作区（包含 CODOL 和 server 目录）"
    )]
    workspace: Option<PathBuf>,
    #[arg(long, help = "指定上游服务器 IP 或域名，不改写客户端文件")]
    server: Option<String>,
    #[arg(long)]
    auth_port: Option<u16>,
    #[arg(long)]
    lobby_port: Option<u16>,
    #[arg(long, help = "本机原生验证的可选运行时限，默认运行到游戏退出", value_parser = clap::value_parser!(u64).range(1..=3600))]
    seconds: Option<u64>,
    #[arg(long, help = "新玩家昵称，可与其他玩家重名")]
    nickname: Option<String>,
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=70), help = "新玩家初始等级，默认 1")]
    level: Option<u8>,
    #[arg(long, help = "新玩家授予全部物品")]
    unlock_all: bool,
}

async fn interrupted() -> Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! { result = tokio::signal::ctrl_c() => result?, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    Ok(())
}

fn run() -> Result<bool> {
    let cli = Cli::parse();
    anyhow::ensure!(
        cli.seconds.is_none() || matches!(cli.action, Action::NativeLaunch),
        "--seconds 仅用于 native-launch"
    );
    anyhow::ensure!(
        matches!(cli.action, Action::NativeCreatePlayer)
            || (cli.nickname.is_none() && cli.level.is_none() && !cli.unlock_all),
        "玩家创建参数仅用于 native-create-player"
    );
    let native_mode = matches!(
        cli.action,
        Action::NativeInit
            | Action::NativeCreatePlayer
            | Action::NativePlan
            | Action::NativeLaunch
            | Action::NativeRecover
    );
    let config = cli.config.unwrap_or_else(|| {
        if matches!(cli.action, Action::Gui) {
            return cols_launcher::player::last_config(std::path::Path::new(
                "profiles/native.local.json",
            ));
        }
        PathBuf::from(if native_mode || matches!(cli.action, Action::Gui) {
            "profiles/native.local.json"
        } else {
            "profiles/codol3333.example.json"
        })
    });
    if matches!(cli.action, Action::NativeInit) {
        let path = native::initialize(&config, cli.workspace.as_deref())?;
        println!("已生成本机原生配置：{}", path.display());
        return Ok(true);
    }
    if matches!(cli.action, Action::Gui) {
        #[cfg(feature = "gui")]
        {
            cols_launcher::gui::open(config, cli.server, cli.auth_port, cli.lobby_port)?;
            return Ok(true);
        }
        #[cfg(not(feature = "gui"))]
        anyhow::bail!("GUI 未编译；请去掉 --no-default-features 后重新构建");
    }
    if native_mode {
        anyhow::ensure!(
            cli.server.is_none() && cli.auth_port.is_none() && cli.lobby_port.is_none(),
            "本地原生模式使用配置中的回环端点；远端选项用于转发模式"
        );
        let profile = native::LocalProfile::load(&config)?;
        if matches!(cli.action, Action::NativeCreatePlayer) {
            let settings = cols_launcher::player::PlayerSettings {
                nickname: cli
                    .nickname
                    .context("创建玩家需要 --nickname")?
                    .trim()
                    .into(),
                level: cli.level.unwrap_or(1),
                unlock_all: cli.unlock_all,
            };
            let path = tokio::runtime::Runtime::new()?
                .block_on(cols_launcher::player::create(&profile, settings))?;
            println!("已创建玩家配置：{}", path.display());
            return Ok(true);
        }
        if matches!(cli.action, Action::NativePlan) {
            let report = native::plan(&profile)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            return Ok(report.launch_allowed);
        }
        return tokio::runtime::Runtime::new()?.block_on(async {
            if matches!(cli.action, Action::NativeRecover) {
                native::recover(&profile).await?;
                println!("本地原生配置已恢复。");
            } else {
                let (stop, receiver) = watch::channel(false);
                let mut running = Box::pin(native::run(
                    &profile,
                    receiver,
                    Arc::new(|message| println!("{message}")),
                ));
                tokio::select! {
                    result = &mut running => { result?; },
                    result = interrupted() => {
                        stop.send_replace(true);
                        running.await?;
                        result?;
                    },
                    _ = async {
                        if let Some(seconds) = cli.seconds { tokio::time::sleep(std::time::Duration::from_secs(seconds)).await; }
                        else { std::future::pending::<()>().await; }
                    } => {
                        stop.send_replace(true);
                        running.await?;
                    },
                }
            }
            Ok(true)
        });
    }
    let mut profile = Profile::load(&config)?;
    profile.set_server(cli.server.as_deref(), cli.auth_port, cli.lobby_port)?;
    if matches!(cli.action, Action::Inspect | Action::Plan) {
        let report = inspection::inspect(&profile)?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(report.launch_allowed);
    }
    tokio::runtime::Runtime::new()?.block_on(async {
        let emit: cols_launcher::relay::Event = Arc::new(|message| println!("{message}"));
        if matches!(cli.action, Action::Relay) {
            let mut relays = RelayGroup::start(&profile.routes, emit).await?;
            println!("仅运行 TCP 转发；未截获其他进程请求，也未启动客户端。");
            let result = interrupted().await;
            relays.close().await;
            result?;
        } else {
            let (stop, receiver) = watch::channel(false);
            let mut running = Box::pin(session::run_session(&profile, receiver, emit));
            tokio::select! {
                result = &mut running => { result?; },
                result = interrupted() => {
                    stop.send_replace(true);
                    running.await?;
                    result?;
                }
            }
        }
        Ok(true)
    })
}

fn main() {
    match run() {
        Ok(true) => {}
        Ok(false) => std::process::exit(2),
        Err(error) => {
            eprintln!("启动器：{error:#}");
            std::process::exit(2);
        }
    }
}
