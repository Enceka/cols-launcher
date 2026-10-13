//! Synthetic process for orchestration tests; never used as a game/backend.
use std::net::{TcpListener, UdpSocket};
use std::time::Duration;

fn option(arguments: &[String], flag: &str) -> String {
    let index = arguments.iter().position(|value| value == flag).unwrap();
    arguments[index + 1].clone()
}

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let Some(action) = arguments.first() else {
        return;
    };
    match action.as_str() {
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
            let mode = std::fs::read_to_string(option(&arguments, "--game-state")).unwrap();
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
            std::thread::sleep(Duration::from_secs(60));
        }
        "observe" => {
            let config = std::fs::read_to_string(option(&arguments, "--config")).unwrap();
            let address = config
                .lines()
                .find_map(|line| line.strip_prefix("bind = "))
                .unwrap()
                .trim_matches('"');
            let _listener = TcpListener::bind(address).unwrap();
            println!("TCP listener ready");
            std::thread::sleep(Duration::from_secs(60));
        }
        path if std::path::Path::new(path).is_file() => {
            // A successful detached launcher, analogous to Wine start.exe.
            std::process::Command::new(path).args(&arguments[1..]).spawn().unwrap();
        }
        _ => {}
    }
}
