use crate::profile::{Route, validate_host};
use anyhow::{Context, Result, ensure};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinSet;

pub type Event = Arc<dyn Fn(&str) + Send + Sync>;

pub struct RelayGroup {
    cancel: watch::Sender<bool>,
    listeners: JoinSet<()>,
}

impl RelayGroup {
    pub async fn start(routes: &[Route], emit: Event) -> Result<Self> {
        let mut bound = Vec::new();
        let local_addresses: Vec<_> = routes
            .iter()
            .map(|route| route.listen.literal())
            .collect::<Result<_>>()?;
        // Bind every port before accepting connections. Failure drops all prior binds.
        for route in routes {
            ensure!(
                route.listen.literal()?.ip().is_loopback()
                    && route.listen.port != 0
                    && route.upstream.port != 0,
                "转发器需要回环监听地址及有效端口"
            );
            validate_host(&route.upstream.host)?;
            bound.push((
                route.clone(),
                TcpListener::bind(route.listen.address())
                    .await
                    .with_context(|| format!("无法监听 {}", route.listen.address()))?,
            ));
        }
        let (cancel, receiver) = watch::channel(false);
        let permits = Arc::new(Semaphore::new(64));
        let mut listeners = JoinSet::new();
        for (route, listener) in bound {
            emit(&format!(
                "转发就绪 {} {} → {}",
                route.name,
                route.listen.address(),
                route.upstream.address()
            ));
            let emit = emit.clone();
            let permits = permits.clone();
            let mut stop = receiver.clone();
            let local_addresses = local_addresses.clone();
            listeners.spawn(async move {
                let mut connections = JoinSet::new();
                loop {
                    tokio::select! {
                        _ = stop.changed() => break,
                        Some(_) = connections.join_next(), if !connections.is_empty() => {},
                        accepted = listener.accept() => {
                            let socket = match accepted {
                                Ok((socket, _)) => socket,
                                Err(error) => { emit(&format!("监听错误 {} {}", route.name, error.kind())); break; }
                            };
                            let Ok(permit) = permits.clone().try_acquire_owned() else {
                                emit(&format!("连接上限 {}", route.name));
                                continue;
                            };
                            let route = route.clone();
                            let emit = emit.clone();
                            let mut stop = stop.clone();
                            let local_addresses = local_addresses.clone();
                            connections.spawn(async move {
                                let _permit = permit;
                                tokio::select! {
                                    _ = stop.changed() => {},
                                    result = forward(socket, &route, &local_addresses) => {
                                        match result {
                                            Ok(()) => emit(&format!("连接结束 {} 完成", route.name)),
                                            Err(error) => emit(&format!("连接结束 {} 失败：{error}", route.name)),
                                        }
                                    }
                                }
                            });
                        }
                    }
                }
                connections.abort_all();
                while connections.join_next().await.is_some() {}
            });
        }
        Ok(Self { cancel, listeners })
    }

    pub async fn close(&mut self) {
        self.cancel.send_replace(true);
        while self.listeners.join_next().await.is_some() {}
    }
}

async fn pump<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    mut source: R,
    mut target: W,
) -> Result<()> {
    let idle = Duration::from_secs(120);
    let mut buffer = [0_u8; 65536];
    loop {
        let count = tokio::time::timeout(idle, source.read(&mut buffer)).await??;
        if count == 0 {
            tokio::time::timeout(idle, target.shutdown()).await??;
            return Ok(());
        }
        tokio::time::timeout(idle, target.write_all(&buffer[..count])).await??;
    }
}

fn canonical(address: SocketAddr) -> SocketAddr {
    SocketAddr::new(address.ip().to_canonical(), address.port())
}

async fn forward(client: TcpStream, route: &Route, listeners: &[SocketAddr]) -> Result<()> {
    let addresses: Vec<_> = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::lookup_host((route.upstream.host.as_str(), route.upstream.port)),
    )
    .await??
    .take(32)
    .collect();
    ensure!(!addresses.is_empty(), "服务器域名没有解析结果");
    for address in &addresses {
        let ip = address.ip().to_canonical();
        ensure!(
            !ip.is_unspecified()
                && !ip.is_multicast()
                && ip != IpAddr::V4(std::net::Ipv4Addr::BROADCAST),
            "无效的上游解析地址"
        );
        ensure!(
            !listeners
                .iter()
                .any(|listener| canonical(*listener) == canonical(*address)),
            "DNS 解析会形成转发循环"
        );
    }
    let upstream = tokio::time::timeout(Duration::from_secs(5), async {
        let mut last_error = None;
        for address in addresses {
            match TcpStream::connect(address).await {
                Ok(socket) => return Ok(socket),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| std::io::Error::other("无可用的服务器地址")))
    })
    .await??;
    let (client_read, client_write) = client.into_split();
    let (upstream_read, upstream_write) = upstream.into_split();
    // A client half-close must still permit the backend to send its response.
    tokio::try_join!(
        pump(upstream_read, client_write),
        pump(client_read, upstream_write)
    )?;
    Ok(())
}
