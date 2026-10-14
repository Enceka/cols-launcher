use cols_launcher::inspection::{inventory, sha256};
use cols_launcher::native::{self, Backend, LocalProfile};
use cols_launcher::native_files::{ClientLocks, Journal, journal_path};
use cols_launcher::process::{Identity, client_process, matches_image};
use cols_launcher::profile::Client;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::watch;

fn helper() -> PathBuf {
    static HELPER: std::sync::OnceLock<(TempDir, PathBuf)> = std::sync::OnceLock::new();
    HELPER
        .get_or_init(|| {
            let temporary = tempfile::tempdir().unwrap();
            let output = temporary.path().join(if cfg!(windows) {
                "native-helper.exe"
            } else {
                "native-helper"
            });
            let rustc = std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into());
            let result = std::process::Command::new(rustc)
                .arg(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/support/native_helper.rs"
                ))
                .arg("-o")
                .arg(&output)
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            (temporary, output)
        })
        .1
        .clone()
}

fn free_tcp() -> u16 {
    unique_port(false)
}
fn free_udp() -> u16 {
    unique_port(true)
}

fn unique_port(udp: bool) -> u16 {
    static PORTS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<(bool, u16)>>> =
        std::sync::OnceLock::new();
    let mut used = PORTS.get_or_init(Default::default).lock().unwrap();
    loop {
        let port = if udp {
            std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
                .unwrap()
                .local_addr()
                .unwrap()
                .port()
        } else {
            std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .unwrap()
                .local_addr()
                .unwrap()
                .port()
        };
        if used.insert((udp, port)) {
            return port;
        }
    }
}

struct Fixture {
    _temporary: TempDir,
    profile: LocalProfile,
    marker: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let base = temporary.path().canonicalize().unwrap();
        let root = base.join("client");
        std::fs::create_dir_all(root.join("IIPS")).unwrap();
        let binary = root.join(if cfg!(windows) {
            "synthetic-client.exe"
        } else {
            "synthetic-client"
        });
        std::fs::copy(helper(), &binary).unwrap();
        std::fs::write(base.join("client-sha256"), sha256(&binary).unwrap()).unwrap();
        std::fs::write(root.join("cacert.pem"), b"synthetic original trust roots\n").unwrap();
        std::fs::write(
            root.join("dwauth.ini"),
            b"192.0.2.1 1234\n192.0.2.1 1235\n0x1\n",
        )
        .unwrap();
        std::fs::write(root.join("stun.ini"), b"192.0.2.1\n").unwrap();
        std::fs::write(root.join("Base.dll"), b"synthetic protected DLL\n").unwrap();
        std::fs::write(
            root.join("game-resource.fixture"),
            b"synthetic protected resource\n",
        )
        .unwrap();
        let state = base.join("state");
        std::fs::create_dir(&state).unwrap();
        let marker = state.join("client-started");
        let certificate = base.join("certificate.pem");
        std::fs::write(
            &certificate,
            b"-----BEGIN CERTIFICATE-----\nZmFrZQ==\n-----END CERTIFICATE-----\n",
        )
        .unwrap();
        let private_key = base.join("key.pem");
        std::fs::write(&private_key, b"synthetic key").unwrap();
        let game_state = base.join("game-state.json");
        std::fs::write(&game_state, b"ready").unwrap();
        let map_catalog = base.join("map-catalog.json");
        std::fs::write(&map_catalog, b"synthetic catalog").unwrap();
        let service_data_dir = base.join("services");
        std::fs::create_dir_all(service_data_dir.join("publisher")).unwrap();
        for name in ["configs_s.info", "mp_playlists_shipRetail_release.info"] {
            std::fs::write(
                service_data_dir.join("publisher").join(name),
                b"synthetic publisher",
            )
            .unwrap();
        }
        let profile = LocalProfile {
            version: 1,
            client: Client {
                executable: binary.clone(),
                sha256: Some(sha256(&binary).unwrap()),
                arguments: vec![
                    "fixture-client".into(),
                    marker.display().to_string(),
                    "500".into(),
                ],
                runner: None,
                wine_prefix: None,
            },
            backend: Backend {
                executable: helper(),
                certificate,
                private_key,
                service_data_dir,
                player_template: None,
                game_state,
                map_catalog,
                title_id: 1002,
                ip: Ipv4Addr::LOCALHOST,
                auth_port: free_tcp(),
                lobby_port: free_tcp(),
                stun_port: free_udp(),
                stun_alt_port: free_udp(),
                game_port: free_udp(),
                resource_port: free_tcp(),
            },
            state_dir: state,
            player: None,
            startup_timeout_seconds: 2,
            client_startup_timeout_seconds: 2,
            trace_wine_files: false,
            source: base.join("native.json"),
        };
        profile.save(&profile.source).unwrap();
        Self {
            _temporary: temporary,
            profile,
            marker,
        }
    }

    fn output(&self) -> PathBuf {
        let output = self.profile.state_dir.join("transaction");
        std::fs::create_dir(&output).unwrap();
        output
    }
}

#[tokio::test]
async fn player_creation_persists_settings_and_switches_to_an_independent_root() {
    let mut fixture = Fixture::new();
    let template = fixture
        .profile
        .backend
        .service_data_dir
        .join("new-player-template.json");
    std::fs::write(&template, b"synthetic template").unwrap();
    let source = fixture.profile.backend.service_data_dir.clone();
    let before_config = std::fs::read(&fixture.profile.source).unwrap();
    let before_client = inventory(&fixture.profile.client_profile()).unwrap();
    let settings = cols_launcher::player::PlayerSettings {
        nickname: "同名玩家".into(),
        level: 37,
        unlock_all: true,
    };
    let config = cols_launcher::player::create(&fixture.profile, settings.clone())
        .await
        .unwrap();
    let player = LocalProfile::load(&config).unwrap();
    assert_eq!(player.player, Some(settings));
    assert_ne!(player.backend.service_data_dir, source);
    assert!(native::plan(&player).unwrap().launch_allowed);
    assert_eq!(player.backend.player_template, Some(template));
    let arguments =
        std::fs::read_to_string(config.parent().unwrap().join("creation-arguments")).unwrap();
    assert!(arguments.contains("--level\n37"));
    assert!(arguments.contains("--nickname\n同名玩家"));
    assert!(arguments.contains("--unlock-all"));
    assert_eq!(
        std::fs::read(&fixture.profile.source).unwrap(),
        before_config
    );
    assert_eq!(
        inventory(&fixture.profile.client_profile()).unwrap(),
        before_client
    );
    let (_stop, receiver) = watch::channel(false);
    native::run(&player, receiver, Arc::new(|_| {}))
        .await
        .unwrap();
    assert_eq!(
        LocalProfile::load(&config).unwrap().player.unwrap().level,
        37
    );
    fixture.profile.backend.player_template = Some(source.join("missing-template"));
    assert!(
        cols_launcher::player::create(&fixture.profile, Default::default())
            .await
            .is_err()
    );
}

#[test]
fn native_profile_roundtrip_and_plan_have_no_client_writes() {
    let fixture = Fixture::new();
    let before = inventory(&fixture.profile.client_profile()).unwrap();
    let loaded = LocalProfile::load(&fixture.profile.source).unwrap();
    let report = native::plan(&loaded).unwrap();
    assert!(report.launch_allowed, "{:?}", report.blockers);
    assert_eq!(report.edited_files.len(), 5);
    assert_eq!(
        before,
        inventory(&fixture.profile.client_profile()).unwrap()
    );
    assert!(!journal_path(&fixture.profile.state_dir, fixture.profile.root()).exists());
}

#[test]
fn native_local_mode_rejects_remote_backend_and_client_data_paths() {
    let mut fixture = Fixture::new();
    fixture.profile.backend.ip = Ipv4Addr::new(203, 0, 113, 10);
    assert!(fixture.profile.validate().is_err());
    fixture.profile.backend.ip = Ipv4Addr::LOCALHOST;
    fixture.profile.backend.service_data_dir = fixture.profile.root().join("server-data");
    assert!(fixture.profile.validate().is_err());
}

#[test]
fn temporary_connection_and_ca_files_restore_exactly_including_absent_files() {
    let fixture = Fixture::new();
    let profile = &fixture.profile;
    let _lock = ClientLocks::acquire(profile.root()).unwrap();
    let before = inventory(&profile.client_profile()).unwrap();
    let resource = std::fs::read(profile.root().join("game-resource.fixture")).unwrap();
    let changes = profile.changes().unwrap();
    let journal = Journal::prepare(
        profile.root(),
        &profile.state_dir,
        &fixture.output(),
        &changes,
    )
    .unwrap();
    journal.apply(&changes).unwrap();
    assert!(profile.root().join("IIPS/cacert.pem").is_file());
    assert!(
        std::fs::read_to_string(profile.root().join("dwauth.ini"))
            .unwrap()
            .contains("0x000003EA")
    );
    assert!(
        std::fs::read_to_string(profile.root().join("cacert.pem"))
            .unwrap()
            .starts_with("synthetic original trust roots")
    );
    journal.restore().unwrap();
    assert_eq!(before, inventory(&profile.client_profile()).unwrap());
    assert!(!profile.root().join("IIPS/cacert.pem").exists());
    assert!(!profile.root().join("iips.ini").exists());
    assert_eq!(
        resource,
        std::fs::read(profile.root().join("game-resource.fixture")).unwrap()
    );
}

#[test]
fn durable_journal_recovers_partial_apply_and_refuses_external_overwrites() {
    let fixture = Fixture::new();
    let profile = &fixture.profile;
    let before = inventory(&profile.client_profile()).unwrap();
    let changes = profile.changes().unwrap();
    let _journal = Journal::prepare(
        profile.root(),
        &profile.state_dir,
        &fixture.output(),
        &changes,
    )
    .unwrap();
    std::fs::write(profile.root().join("dwauth.ini"), &changes[0].1).unwrap();
    let journal = Journal::load(
        &journal_path(&profile.state_dir, profile.root()),
        profile.root(),
        &profile.state_dir,
    )
    .unwrap();
    journal.restore().unwrap();
    assert_eq!(before, inventory(&profile.client_profile()).unwrap());
    drop(journal);
    let output = profile.state_dir.join("other-transaction");
    std::fs::create_dir(&output).unwrap();
    let journal = Journal::prepare(profile.root(), &profile.state_dir, &output, &changes).unwrap();
    journal.apply(&changes).unwrap();
    std::fs::write(profile.root().join("stun.ini"), b"external change").unwrap();
    assert!(journal.restore().is_err());
    assert_eq!(
        std::fs::read(profile.root().join("stun.ini")).unwrap(),
        b"external change"
    );
    assert!(journal_path(&profile.state_dir, profile.root()).exists());
}

#[cfg(unix)]
#[test]
fn temporary_ca_writes_cannot_follow_a_symlink() {
    let fixture = Fixture::new();
    let outside = fixture.profile.state_dir.join("outside.pem");
    std::fs::write(&outside, b"outside certificate").unwrap();
    std::fs::remove_file(fixture.profile.root().join("cacert.pem")).unwrap();
    std::os::unix::fs::symlink(&outside, fixture.profile.root().join("cacert.pem")).unwrap();
    let changes = fixture.profile.changes().unwrap();
    assert!(
        Journal::prepare(
            fixture.profile.root(),
            &fixture.profile.state_dir,
            &fixture.output(),
            &changes
        )
        .is_err()
    );
    assert_eq!(std::fs::read(outside).unwrap(), b"outside certificate");
}

#[test]
fn client_lock_is_shared_across_different_state_directories() {
    let fixture = Fixture::new();
    let _first = ClientLocks::acquire(fixture.profile.root()).unwrap();
    assert!(ClientLocks::acquire(fixture.profile.root()).is_err());
}

#[tokio::test]
async fn waits_for_every_backend_then_launches_native_and_restores() {
    let fixture = Fixture::new();
    let before = inventory(&fixture.profile.client_profile()).unwrap();
    let (_stop, receiver) = watch::channel(false);
    let record = native::run(&fixture.profile, receiver, Arc::new(|_| {}))
        .await
        .unwrap();
    assert!(record.backend_ready && record.configs_restored);
    assert!(record.changed_files.is_empty());
    assert!(fixture.marker.is_file());
    assert_eq!(
        before,
        inventory(&fixture.profile.client_profile()).unwrap()
    );
    assert!(
        std::net::TcpListener::bind(fixture.profile.address(fixture.profile.backend.auth_port))
            .is_ok()
    );
}

#[tokio::test]
async fn verbose_native_output_is_drained_and_bounded_without_blocking_cleanup() {
    let mut fixture = Fixture::new();
    fixture.profile.client.arguments.push("noisy".into());
    let (_stop, receiver) = watch::channel(false);
    let record = tokio::time::timeout(
        Duration::from_secs(10),
        native::run(&fixture.profile, receiver, Arc::new(|_| {})),
    )
    .await
    .expect("client output must not block the session")
    .unwrap();
    assert!(record.configs_restored && record.changed_files.is_empty());
    assert!(fixture.marker.is_file());
    let output = std::fs::read_dir(&fixture.profile.state_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path().join("client.log"))
        .find(|path| path.is_file())
        .unwrap();
    let bytes = std::fs::read(output).unwrap();
    assert!(!bytes.is_empty() && bytes.len() <= 4 * 1024 * 1024);
    assert!(String::from_utf8_lossy(&bytes).contains("native-output-drained"));
}

#[tokio::test]
async fn incompatible_backend_readiness_never_launches_or_changes_client_config() {
    for mode in [
        "wrong-pid",
        "wrong-version",
        "wrong-endpoint",
        "wrong-client",
        "wrong-protocol",
        "missing-maps",
        "logs-only",
    ] {
        let fixture = Fixture::new();
        std::fs::write(&fixture.profile.backend.game_state, mode).unwrap();
        let before = inventory(&fixture.profile.client_profile()).unwrap();
        let modified = fixture
            .profile
            .root()
            .join("dwauth.ini")
            .metadata()
            .unwrap()
            .modified()
            .unwrap();
        let (_stop, receiver) = watch::channel(false);
        let error = native::run(&fixture.profile, receiver, Arc::new(|_| {}))
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        let expected = match mode {
            "wrong-pid" => "不属于本次进程",
            "wrong-version" => "契约版本不兼容",
            "wrong-endpoint" => "就绪地址",
            "wrong-client" => "基线不匹配",
            "wrong-protocol" => "协议版本不兼容",
            "missing-maps" => "地图和 Bot 房间装配",
            _ => "就绪超时",
        };
        assert!(message.contains(expected), "{mode}: {message}");
        assert!(!fixture.marker.exists());
        assert_eq!(
            before,
            inventory(&fixture.profile.client_profile()).unwrap()
        );
        assert_eq!(
            modified,
            fixture
                .profile
                .root()
                .join("dwauth.ini")
                .metadata()
                .unwrap()
                .modified()
                .unwrap()
        );
        assert!(!journal_path(&fixture.profile.state_dir, fixture.profile.root()).exists());
    }
}

#[tokio::test]
async fn bound_auth_without_game_readiness_does_not_launch_or_edit_client() {
    let fixture = Fixture::new();
    std::fs::write(&fixture.profile.backend.game_state, b"not-ready").unwrap();
    let before = inventory(&fixture.profile.client_profile()).unwrap();
    let original_modified = fixture
        .profile
        .root()
        .join("dwauth.ini")
        .metadata()
        .unwrap()
        .modified()
        .unwrap();
    let (_stop, receiver) = watch::channel(false);
    let error = native::run(&fixture.profile, receiver, Arc::new(|_| {}))
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("就绪超时"));
    assert!(!fixture.marker.exists());
    assert_eq!(
        before,
        inventory(&fixture.profile.client_profile()).unwrap()
    );
    assert!(!journal_path(&fixture.profile.state_dir, fixture.profile.root()).exists());
    assert_eq!(
        original_modified,
        fixture
            .profile
            .root()
            .join("dwauth.ini")
            .metadata()
            .unwrap()
            .modified()
            .unwrap()
    );
}

#[tokio::test]
async fn backend_startup_failure_rolls_back_without_launching_client() {
    let fixture = Fixture::new();
    std::fs::write(&fixture.profile.backend.game_state, b"exit").unwrap();
    let before = inventory(&fixture.profile.client_profile()).unwrap();
    let (_stop, receiver) = watch::channel(false);
    assert!(
        native::run(&fixture.profile, receiver, Arc::new(|_| {}))
            .await
            .is_err()
    );
    assert!(!fixture.marker.exists());
    assert_eq!(
        before,
        inventory(&fixture.profile.client_profile()).unwrap()
    );
}

#[tokio::test]
async fn cancelling_native_game_stops_owned_processes_and_restores() {
    let mut fixture = Fixture::new();
    fixture.profile.client.arguments[2] = "60000".into();
    let (stop, receiver) = watch::channel(false);
    let profile = fixture.profile.clone();
    let (events, mut messages) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        native::run(
            &profile,
            receiver,
            Arc::new(move |message| {
                let _ = events.send(message.to_owned());
            }),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(message) = messages.recv().await {
            if message.contains("原生客户端已启动") {
                break;
            }
        }
    })
    .await
    .unwrap();
    stop.send_replace(true);
    let record = tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(record.configs_restored && record.changed_files.is_empty());
    assert!(client_process(&fixture.profile.client.executable, None).is_none());
}

#[tokio::test]
async fn running_client_is_rejected_before_configuration_changes() {
    let fixture = Fixture::new();
    let mut child = tokio::process::Command::new(&fixture.profile.client.executable)
        .arg("fixture-client")
        .arg(&fixture.marker)
        .arg("60000")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let before = inventory(&fixture.profile.client_profile()).unwrap();
    let (_stop, receiver) = watch::channel(false);
    let error = native::run(&fixture.profile, receiver, Arc::new(|_| {}))
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("已在运行"));
    assert_eq!(
        before,
        inventory(&fixture.profile.client_profile()).unwrap()
    );
    child.kill().await.unwrap();
}

#[test]
fn process_identity_matches_exact_image_and_rejects_reused_pid() {
    let fixture = Fixture::new();
    assert!(matches_image(
        fixture.profile.client.executable.to_str().unwrap(),
        &fixture.profile.client.executable,
        None
    ));
    assert!(!matches_image(
        "start.exe",
        &fixture.profile.client.executable,
        None
    ));
    let mut identity = Identity::from_pid(std::process::id()).unwrap();
    assert!(identity.alive());
    identity.started += 1;
    assert!(!identity.alive());
}

#[tokio::test]
async fn successful_detached_wrapper_is_tracked_until_native_game_exit() {
    let mut fixture = Fixture::new();
    fixture.profile.client.runner = Some(helper());
    let (_stop, receiver) = watch::channel(false);
    let record = native::run(&fixture.profile, receiver, Arc::new(|_| {}))
        .await
        .unwrap();
    assert!(record.client_pid.is_some() && record.configs_restored);
    assert!(fixture.marker.exists());
    assert!(record.changed_files.is_empty());
}

#[tokio::test]
async fn failed_native_process_restores_temporary_ini_and_ca() {
    let mut fixture = Fixture::new();
    fixture.profile.client.arguments = vec!["fixture-fail".into()];
    let before = inventory(&fixture.profile.client_profile()).unwrap();
    let (_stop, receiver) = watch::channel(false);
    assert!(
        native::run(&fixture.profile, receiver, Arc::new(|_| {}))
            .await
            .is_err()
    );
    assert_eq!(
        before,
        inventory(&fixture.profile.client_profile()).unwrap()
    );
    assert!(!journal_path(&fixture.profile.state_dir, fixture.profile.root()).exists());
}

#[tokio::test]
async fn recovery_refuses_active_owner_then_recovers_a_crash_record() {
    let fixture = Fixture::new();
    let profile = &fixture.profile;
    let before = inventory(&profile.client_profile()).unwrap();
    let changes = profile.changes().unwrap();
    let journal = Journal::prepare(
        profile.root(),
        &profile.state_dir,
        &fixture.output(),
        &changes,
    )
    .unwrap();
    journal.apply(&changes).unwrap();
    let error = native::recover(profile).await.unwrap_err();
    assert!(format!("{error:#}").contains("会话仍在运行"));
    let path = journal_path(&profile.state_dir, profile.root());
    let mut record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    record["owner"]["pid"] = serde_json::json!(u32::MAX);
    record["owner"]["started"] = serde_json::json!(0);
    std::fs::write(&path, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
    native::recover(profile).await.unwrap();
    assert_eq!(before, inventory(&profile.client_profile()).unwrap());
    assert!(!path.exists());
}
