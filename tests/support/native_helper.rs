//! Synthetic process for orchestration tests; never used as a game/backend.
use std::net::{TcpListener, UdpSocket};
use std::time::Duration;

fn option(arguments: &[String], flag: &str) -> String {
    let index = arguments.iter().position(|value| value == flag).unwrap();
    arguments[index + 1].clone()
}

fn publish_ready(arguments: &[String], body: &str, digest: &str, mode: &str) {
    let pid = std::process::id() + u32::from(mode == "wrong-pid");
    let version = if mode == "wrong-version" { 2 } else { 1 };
    let digest = if mode == "wrong-client" { "unsupported-client" } else { digest };
    let path = std::path::PathBuf::from(option(arguments, "--launcher-ready-file"));
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, format!(
        "{{\"schema_version\":{version},\"pid\":{pid},\"client_sha256\":\"{digest}\",{body}}}"
    )).unwrap();
    std::fs::rename(temporary, path).unwrap();
}

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let Some(action) = arguments.first() else {
        return;
    };
    match action.as_str() {
        "create-local-player" => {
            let parent = std::path::PathBuf::from(option(&arguments, "--output-parent"));
            let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let directory = parent.join(format!("{nonce:032x}"));
            let data = directory.join("server-data");
            std::fs::create_dir_all(data.join("publisher")).unwrap();
            let source = std::path::PathBuf::from(option(&arguments, "--source-data-dir"));
            for name in ["configs_s.info", "mp_playlists_shipRetail_release.info"] {
                std::fs::copy(source.join("publisher").join(name), data.join("publisher").join(name)).unwrap();
            }
            std::fs::write(directory.join("creation-arguments"), arguments.join("\n")).unwrap();
            // Temp test paths contain no JSON control characters.
            println!("{{\"player_directory\":\"{}\"}}", directory.display());
        }
        "fixture-client" => {
            if arguments.get(3).is_some_and(|value| value == "noisy") {
                use std::io::Write;
                let buffer = [b'x'; 8192];
                for _ in 0..640 {
                    std::io::stdout().write_all(&buffer).unwrap();
                    std::io::stderr().write_all(&buffer).unwrap();
                }
                eprintln!("native-output-drained");
            }
            std::fs::write(&arguments[1], b"synthetic native client started").unwrap();
            std::thread::sleep(Duration::from_millis(arguments[2].parse().unwrap()));
        }
        "fixture-fail" => std::process::exit(7),
        "serve-auth" => {
            let state_path = std::path::PathBuf::from(option(&arguments, "--game-state"));
            let mode = std::fs::read_to_string(&state_path).unwrap();
            if mode == "exit" {
                std::process::exit(3);
            }
            let _auth = TcpListener::bind(option(&arguments, "--bind")).unwrap();
            let _lobby = TcpListener::bind(option(&arguments, "--lobby-bind")).unwrap();
            let _stun = UdpSocket::bind(option(&arguments, "--stun-bind")).unwrap();
            let _stun_alt = UdpSocket::bind(option(&arguments, "--stun-alt")).unwrap();
            let _game = UdpSocket::bind(option(&arguments, "--game-bind")).unwrap();
            println!("Rust AUTH listener ready");
            println!("Rust lobby listener ready");
            println!("Rust STUN listener ready");
            if mode != "not-ready" {
                println!("experimental Rust game UDP endpoint ready");
            }
            if mode != "not-ready" && mode != "logs-only" {
                let digest = std::fs::read_to_string(state_path.parent().unwrap().join("client-sha256")).unwrap();
                let game = if mode == "wrong-endpoint" { "127.0.0.1:1".into() } else { option(&arguments, "--game-bind") };
                let protocol = if mode == "wrong-protocol" { 4 } else { 3 };
                let loaded = mode != "missing-maps";
                let body = format!(
                    "\"kind\":\"native_services\",\"lobby_protocol\":210,\"game_protocol\":{protocol},\"title_id\":{},\"auth\":\"{}\",\"lobby\":\"{}\",\"stun\":\"{}\",\"stun_alt\":\"{}\",\"game\":\"{}\",\"continuous_play\":true,\"native_bot_rooms\":false,\"room_bot_fill\":12,\"game_state_loaded\":true,\"map_catalog_loaded\":{loaded}",
                    option(&arguments,"--local-title-id"),option(&arguments,"--bind"),option(&arguments,"--lobby-bind"),option(&arguments,"--stun-bind"),option(&arguments,"--stun-alt"),game
                );
                publish_ready(&arguments, &body, &digest, &mode);
            }
            std::thread::sleep(Duration::from_secs(60));
        }
        "observe" => {
            let config_path = std::path::PathBuf::from(option(&arguments, "--config"));
            let config = std::fs::read_to_string(&config_path).unwrap();
            let address = config
                .lines()
                .find_map(|line| line.strip_prefix("bind = "))
                .unwrap()
                .trim_matches('"');
            let _listener = TcpListener::bind(address).unwrap();
            println!("TCP listener ready");
            let digest_path = config_path.ancestors().map(|path| path.join("client-sha256")).find(|path| path.is_file()).unwrap();
            let digest = std::fs::read_to_string(digest_path).unwrap();
            publish_ready(&arguments, &format!("\"kind\":\"tcp_observer\",\"listeners\":{{\"resources\":\"{address}\"}}"), &digest, "ready");
            std::thread::sleep(Duration::from_secs(60));
        }
        path if std::path::Path::new(path).is_file() => {
            // A successful detached launcher, analogous to Wine start.exe.
            std::process::Command::new(path).args(&arguments[1..]).spawn().unwrap();
        }
        _ => {}
    }
}
