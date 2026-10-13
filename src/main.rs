use anyhow::Result;
use clap::{Parser, ValueEnum};
use codol_launcher::{inspection, profile::Profile, relay::RelayGroup, session};
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
}

#[derive(Parser)]
#[command(version, about = "COLS启动器")]
struct Cli {
    #[arg(value_enum)]
    action: Action,
    #[arg(long, default_value = "profiles/codol3333.example.json")]
    config: PathBuf,
    #[arg(long, help = "指定上游服务器 IP 或域名，不改写客户端文件")]
    server: Option<String>,
    #[arg(long)]
    auth_port: Option<u16>,
    #[arg(long)]
    lobby_port: Option<u16>,
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
    if matches!(cli.action, Action::Gui) {
        #[cfg(feature = "gui")]
        {
            codol_launcher::gui::open(cli.config, cli.server, cli.auth_port, cli.lobby_port)?;
            return Ok(true);
        }
        #[cfg(not(feature = "gui"))]
        anyhow::bail!("GUI 未编译；请去掉 --no-default-features 后重新构建");
    }
    let mut profile = Profile::load(&cli.config)?;
    profile.set_server(cli.server.as_deref(), cli.auth_port, cli.lobby_port)?;
    if matches!(cli.action, Action::Inspect | Action::Plan) {
        let report = inspection::inspect(&profile)?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(report.launch_allowed);
    }
    tokio::runtime::Runtime::new()?.block_on(async {
        let emit: codol_launcher::relay::Event = Arc::new(|message| println!("{message}"));
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
