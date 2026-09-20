//! Scratch tool: exercises the real `session.rs` connect/auth code path
//! (including known_hosts verification and the key/agent-first password
//! fallback) against a live SSH server, without going through the gpui UI.
//! Not part of the shipped app; kept for manual testing.
//!
//! Usage: cargo run --bin test_connect -- <host> <port> <username> [password]
//! If password is omitted, it is read from the AETHERIUM_TEST_PASSWORD env
//! var instead, to keep it out of shell history and `ps` output.

#[path = "../crypto.rs"]
mod crypto;
#[path = "../profiles.rs"]
mod profiles;
#[path = "../session.rs"]
mod session;
#[path = "../terminal_model.rs"]
mod terminal_model;

use std::time::Duration;

use profiles::{AuthMethod, Profile};
use session::{Command, Event, SessionHandle};
use terminal_model::TerminalModel;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (host, port, username, password) = match args.as_slice() {
        [_, host, port, username, password] => (host.clone(), port.clone(), username.clone(), password.clone()),
        [_, host, port, username] => {
            let password = std::env::var("AETHERIUM_TEST_PASSWORD")
                .expect("pass a password argument or set AETHERIUM_TEST_PASSWORD");
            (host.clone(), port.clone(), username.clone(), password)
        }
        _ => {
            eprintln!("usage: test_connect <host> <port> <username> [password]");
            std::process::exit(1);
        }
    };
    let port: u16 = port.parse().expect("port must be a number");

    let profile = Profile {
        name: "test".into(),
        host: host.clone(),
        port,
        username: username.clone(),
        auth: AuthMethod::Password {
            password: password.clone(),
        },
    };

    let terminal = TerminalModel::new(80, 24);
    let session = SessionHandle::spawn(terminal.clone());
    session.connect(profile);

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut connected = false;
    while std::time::Instant::now() < deadline {
        if let Some(event) = session.try_recv_event() {
            match event {
                Event::Connected { home_dir } => {
                    println!("CONNECTED home={}", home_dir.display());
                    connected = true;
                    session.list_dir(1, home_dir);
                }
                Event::DirListing { path, entries, .. } => {
                    println!("LISTING {}: {} entries", path.display(), entries.len());
                    for entry in entries.iter().take(10) {
                        println!("  {} dir={} size={}", entry.name, entry.is_dir, entry.size);
                    }
                    session.send(Command::Disconnect);
                }
                Event::Error(message) => {
                    println!("ERROR {message}");
                }
                Event::Disconnected => {
                    println!("DISCONNECTED");
                    break;
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    if !connected {
        eprintln!("FAILED: did not connect within timeout");
        std::process::exit(1);
    }
}
