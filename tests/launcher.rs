use codol_launcher::inspection::{inspect, inventory, sha256};
use codol_launcher::profile::{Endpoint, Profile, Route, Routing};
use codol_launcher::relay::{Event, RelayGroup};
use codol_launcher::session::run_session;
use serde_json::{Value, json};
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

struct Fixture {
    dir: TempDir,
    source: PathBuf,
    value: Value,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let client = dir.path().join("client");
        std::fs::create_dir(&client).unwrap();
        std::fs::write(
            client.join("synthetic-client"),
            b"synthetic executable baseline",
        )
        .unwrap();
        std::fs::write(client.join("cacert.pem"), b"synthetic certificate bytes").unwrap();
        std::fs::write(client.join("stun.ini"), b"192.0.2.1\n").unwrap();
        let value = json!({"version":1,"client":{"executable":"client/synthetic-client", "sha256":null,
            "arguments":[],"runner":null,"wine_prefix":null},"routing":"direct","routes":[],"state_dir":"state"});
        let source = dir.path().join("profile.json");
        let fixture = Self { dir, source, value };
        fixture.write();
        fixture
    }

    fn write(&self) {
        std::fs::write(
            &self.source,
            serde_json::to_vec_pretty(&self.value).unwrap(),
        )
        .unwrap();
    }
    fn profile(&self) -> Profile {
        Profile::load(&self.source).unwrap()
    }
    fn runnable(&mut self, test_name: &str) -> Profile {
        // Only this project's own test binary is copied into the synthetic installation.
        let binary = self.dir.path().join("client/synthetic-client");
        std::fs::copy(std::env::current_exe().unwrap(), &binary).unwrap();
        self.value["client"]["sha256"] = json!(sha256(&binary).unwrap());
        self.value["client"]["arguments"] = json!(["--ignored", "--exact", test_name]);
        self.write();
        self.profile()
    }
}

fn endpoint(port: u16) -> Endpoint {
    Endpoint {
        host: Ipv4Addr::LOCALHOST.to_string(),
        port,
    }
}
fn free_port() -> u16 {
    std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
fn route(listen: u16, upstream: u16) -> Route {
    Route {
        name: "synthetic".into(),
        listen: endpoint(listen),
        upstream: endpoint(upstream),
    }
}
fn quiet() -> Event {
    Arc::new(|_| {})
}

#[test]
fn preview_is_read_only_and_resolves_relative_paths() {
    let fixture = Fixture::new();
    let profile = fixture.profile();
    let before = inventory(&profile).unwrap();
    let report = inspect(&profile).unwrap();
    assert!(report.launch_allowed);
    assert_eq!(before, inventory(&profile).unwrap());
    assert!(!profile.state_dir.exists());
    assert!(profile.client.executable.is_absolute());
    assert_eq!(
        std::fs::read_dir(profile.client.executable.parent().unwrap())
            .unwrap()
            .count(),
        3
    );
}

#[test]
fn invalid_baseline_and_external_interception_block_launch() {
    let fixture = Fixture::new();
    let mut profile = fixture.profile();
    profile.client.sha256 = Some("0".repeat(64));
    assert!(!inspect(&profile).unwrap().launch_allowed);
    profile.client.sha256 = None;
    profile.routing = Routing::External;
    profile.routes = vec![route(49001, 39001)];
    let report = inspect(&profile).unwrap();
    assert!(!report.launch_allowed);
    assert!(
        report
            .blockers
            .iter()
            .any(|message| message.contains("尚未实现"))
    );
}

#[test]
fn existing_bootstrap_must_match_without_rewriting() {
    let fixture = Fixture::new();
    let mut profile = fixture.profile();
    profile.routing = Routing::ExistingConfig;
    profile.routes = vec![
        route(39001, 49001),
        Route {
            name: "auth".into(),
            ..route(39002, 49002)
        },
    ];
    let bootstrap = profile
        .client
        .executable
        .parent()
        .unwrap()
        .join("dwauth.ini");
    std::fs::write(&bootstrap, "127.0.0.1 39001\n127.0.0.1 39002\n0x000003EA\n").unwrap();
    let original = std::fs::read(&bootstrap).unwrap();
    assert!(inspect(&profile).unwrap().launch_allowed);
    profile.routes[0].listen.port = 39010;
    assert!(!inspect(&profile).unwrap().launch_allowed);
    assert_eq!(std::fs::read(&bootstrap).unwrap(), original);
}

#[test]
fn profiles_reject_client_tree_writes_unknown_fields_and_cycles() {
    let mut fixture = Fixture::new();
    fixture.value["state_dir"] = json!("client/session");
    fixture.write();
    assert!(Profile::load(&fixture.source).is_err());
    fixture.value["state_dir"] = json!("state");
    fixture.value["disable_tls_validation"] = json!(true);
    fixture.write();
    assert!(Profile::load(&fixture.source).is_err());
    fixture
        .value
        .as_object_mut()
        .unwrap()
        .remove("disable_tls_validation");
    fixture.write();
    let mut profile = fixture.profile();
    profile.routing = Routing::External;
    profile.routes = vec![
        route(39001, 39002),
        Route {
            name: "two".into(),
            ..route(39002, 39001)
        },
    ];
    assert!(profile.validate().is_err());
    profile.routes = vec![route(39001, 49001)];
    profile.routes[0].listen.host = Ipv4Addr::UNSPECIFIED.to_string();
    assert!(profile.validate().is_err());
}

#[test]
fn server_ip_domain_and_ports_can_be_selected_and_saved_outside_client() {
    let fixture = Fixture::new();
    let mut profile = fixture.profile();
    profile.routing = Routing::External;
    profile.routes = vec![
        Route {
            name: "lobby".into(),
            ..route(49001, 39001)
        },
        Route {
            name: "auth".into(),
            ..route(49002, 39002)
        },
    ];
    let before = inventory(&profile).unwrap();
    profile
        .set_server(Some("game.example.com"), Some(443), Some(3075))
        .unwrap();
    assert_eq!(profile.routes[0].upstream.host, "game.example.com");
    assert_eq!(profile.routes[0].upstream.port, 3075);
    assert_eq!(profile.routes[1].upstream.port, 443);
    let saved = profile.save_local().unwrap();
    let loaded = Profile::load(&saved).unwrap();
    assert_eq!(loaded.routes[1].upstream.host, "game.example.com");
    assert_eq!(before, inventory(&profile).unwrap());
    profile
        .set_server(Some("203.0.113.10"), None, None)
        .unwrap();
    assert_eq!(profile.routes[0].upstream.host, "203.0.113.10");
    assert!(
        profile
            .set_server(Some("https://example.com/path"), None, None)
            .is_err()
    );
    assert!(
        profile
            .set_server(Some("--bad.example"), None, None)
            .is_err()
    );
}

#[tokio::test]
async fn resolves_server_domain_and_rejects_dns_forwarding_loop() {
    let backend = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let mut rule = route(free_port(), backend.local_addr().unwrap().port());
    rule.upstream.host = "localhost".into();
    let service = tokio::spawn(async move {
        let (mut socket, _) = backend.accept().await.unwrap();
        let mut request = [0_u8; 4];
        socket.read_exact(&mut request).await.unwrap();
        assert_eq!(&request, b"ping");
        socket.write_all(b"synthetic-domain-server").await.unwrap();
        socket.shutdown().await.unwrap();
    });
    let mut relay = RelayGroup::start(std::slice::from_ref(&rule), quiet())
        .await
        .unwrap();
    let mut client = TcpStream::connect(rule.listen.address()).await.unwrap();
    client.write_all(b"ping").await.unwrap();
    client.shutdown().await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(bytes, b"synthetic-domain-server");
    service.await.unwrap();
    relay.close().await;
    rule.upstream.port = rule.listen.port;
    let mut relay = RelayGroup::start(std::slice::from_ref(&rule), quiet())
        .await
        .unwrap();
    let mut client = TcpStream::connect(rule.listen.address()).await.unwrap();
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), client.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    relay.close().await;
}

#[cfg(unix)]
#[test]
fn symlink_cannot_move_state_into_client_tree() {
    let mut fixture = Fixture::new();
    std::os::unix::fs::symlink(
        fixture.dir.path().join("client"),
        fixture.dir.path().join("outside-link"),
    )
    .unwrap();
    fixture.value["state_dir"] = json!("outside-link/state");
    fixture.write();
    assert!(Profile::load(&fixture.source).is_err());
}

#[tokio::test]
async fn forwards_large_binary_payload_and_half_close_without_logging_data() {
    let request: Vec<u8> = (0..2 * 1024 * 1024).map(|n| (n % 256) as u8).collect();
    let expected: Vec<_> = request.iter().rev().copied().collect();
    let backend = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let rule = route(free_port(), backend.local_addr().unwrap().port());
    let service = tokio::spawn(async move {
        let (mut socket, _) = backend.accept().await.unwrap();
        let mut bytes = Vec::new();
        socket.read_to_end(&mut bytes).await.unwrap();
        bytes.reverse();
        socket.write_all(&bytes).await.unwrap();
    });
    let messages = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = messages.clone();
    let mut relay = RelayGroup::start(
        std::slice::from_ref(&rule),
        Arc::new(move |message| {
            captured.lock().unwrap().push(message.to_owned());
        }),
    )
    .await
    .unwrap();
    let mut client = TcpStream::connect(rule.listen.address()).await.unwrap();
    client.write_all(&request).await.unwrap();
    client.shutdown().await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut response))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response, expected);
    service.await.unwrap();
    relay.close().await;
    assert!(
        messages
            .lock()
            .unwrap()
            .iter()
            .all(|message| message.len() < 200)
    );
    assert!(TcpListener::bind(rule.listen.address()).await.is_ok());
}

#[tokio::test]
async fn failed_multiport_start_releases_prior_bindings() {
    let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let first = route(free_port(), 9);
    let second = Route {
        name: "occupied".into(),
        ..route(occupied.local_addr().unwrap().port(), 9)
    };
    assert!(
        RelayGroup::start(&[first.clone(), second], quiet())
            .await
            .is_err()
    );
    assert!(TcpListener::bind(first.listen.address()).await.is_ok());
}

#[tokio::test]
async fn unreachable_upstream_closes_client() {
    let rule = route(free_port(), free_port());
    let mut relay = RelayGroup::start(std::slice::from_ref(&rule), quiet())
        .await
        .unwrap();
    let mut client = TcpStream::connect(rule.listen.address()).await.unwrap();
    let mut byte = [0];
    let read = tokio::time::timeout(Duration::from_secs(6), client.read(&mut byte))
        .await
        .unwrap();
    assert!(read.is_err() || read.unwrap() == 0);
    relay.close().await;
}

#[tokio::test]
async fn stopping_relay_closes_active_flows_and_listener() {
    let backend = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let rule = route(free_port(), backend.local_addr().unwrap().port());
    let mut relay = RelayGroup::start(std::slice::from_ref(&rule), quiet())
        .await
        .unwrap();
    let mut client = TcpStream::connect(rule.listen.address()).await.unwrap();
    let (_upstream, _) = tokio::time::timeout(Duration::from_secs(2), backend.accept())
        .await
        .unwrap()
        .unwrap();
    relay.close().await;
    let mut byte = [0];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), client.read(&mut byte))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert!(TcpListener::bind(rule.listen.address()).await.is_ok());
}

#[tokio::test]
async fn launches_synthetic_native_child_without_client_writes() {
    let mut fixture = Fixture::new();
    let profile = fixture.runnable("child_exits");
    let before = inventory(&profile).unwrap();
    let (_stop, receiver) = watch::channel(false);
    let record = run_session(&profile, receiver, quiet()).await.unwrap();
    assert_eq!(record.exit_code, Some(0));
    assert!(record.changed_files.is_empty());
    assert_eq!(before, inventory(&profile).unwrap());
    assert!(profile.state_dir.is_dir());
    assert_eq!(
        std::fs::read_dir(profile.client.executable.parent().unwrap())
            .unwrap()
            .count(),
        3
    );
}

#[tokio::test]
async fn same_client_lock_rejects_second_session_and_stop_cleans_up() {
    let mut fixture = Fixture::new();
    let profile = fixture.runnable("child_waits");
    let (stop, receiver) = watch::channel(false);
    let (events, mut messages) = tokio::sync::mpsc::unbounded_channel();
    let emit: Event = Arc::new(move |message| {
        let _ = events.send(message.to_owned());
    });
    let first_profile = profile.clone();
    let first = tokio::spawn(async move { run_session(&first_profile, receiver, emit).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(message) = messages.recv().await {
            if message.contains("已启动") {
                break;
            }
        }
    })
    .await
    .unwrap();
    let (_other_stop, receiver) = watch::channel(false);
    let error = run_session(&profile, receiver, quiet()).await.unwrap_err();
    assert!(format!("{error:#}").contains("已有启动器会话"));
    stop.send_replace(true);
    let record = tokio::time::timeout(Duration::from_secs(6), first)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(record.changed_files.is_empty());
    let (_stop, receiver) = watch::channel(false);
    let profile = fixture.runnable("child_exits");
    assert!(run_session(&profile, receiver, quiet()).await.is_ok());
}

#[test]
#[ignore = "synthetic child entry point, launched by integration tests"]
fn child_exits() {}

#[test]
#[ignore = "synthetic child entry point, launched by integration tests"]
fn child_waits() {
    std::thread::sleep(Duration::from_secs(60));
}
