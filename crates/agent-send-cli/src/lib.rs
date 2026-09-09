use agent_send_core::{FolderDirection, TransferRequest};
use agent_send_daemon::{Cancellation, Config, Daemon};
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const HELP: &str = "agent-send\n\nUSAGE:\n    agent-send --help\n    agent-send health --addr HOST:PORT\n    agent-send peers --addr HOST:PORT\n    agent-send demo\n";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Help,
    Health { addr: SocketAddr },
    Peers { addr: SocketAddr },
    Demo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError(String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl Error for ParseError {}

pub fn parse_args<I, S>(args: I) -> Result<Command, ParseError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let args: Vec<String> = args.into_iter().map(Into::into).collect();
    match args.as_slice() {
        [] => Ok(Command::Help),
        [a] if a == "--help" || a == "-h" => Ok(Command::Help),
        [command, flag, address]
            if (command == "health" || command == "peers") && flag == "--addr" =>
        {
            let addr = address
                .parse::<SocketAddr>()
                .map_err(|_| ParseError("--addr must be HOST:PORT".into()))?;
            if !addr.ip().is_loopback() {
                return Err(ParseError("--addr must be a loopback address".into()));
            }
            if command == "health" {
                Ok(Command::Health { addr })
            } else {
                Ok(Command::Peers { addr })
            }
        }
        [command] if command == "demo" => Ok(Command::Demo),
        _ => Err(ParseError(
            "usage: agent-send {--help|health|peers} --addr HOST:PORT, or demo".into(),
        )),
    }
}

pub fn help() -> &'static str {
    HELP
}

fn request(addr: SocketAddr, path: &str) -> Result<String, Box<dyn Error>> {
    if !addr.ip().is_loopback() {
        return Err(ParseError("--addr must be a loopback address".into()).into());
    }
    let mut stream = TcpStream::connect(addr)?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    let (header, body) = response
        .split_once("\r\n\r\n")
        .ok_or("invalid HTTP response")?;
    if !header.starts_with("HTTP/1.1 200 ") {
        return Err(format!(
            "daemon returned {}",
            header.lines().next().unwrap_or("error")
        )
        .into());
    }
    Ok(body.to_owned())
}

pub fn run(command: Command) -> Result<(), Box<dyn Error>> {
    match command {
        Command::Help => print!("{HELP}"),
        Command::Health { addr } => println!("{}", request(addr, "/v1/health")?),
        Command::Peers { addr } => println!("{}", request(addr, "/v1/peers")?),
        Command::Demo => {
            let result = run_demo()?;
            println!(
                "destination: {}\nbytes: {}\nsha256: {}",
                result.destination.display(),
                result.bytes,
                result.sha256
            );
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DemoResult {
    pub destination: PathBuf,
    pub bytes: u64,
    pub sha256: String,
}

pub fn run_demo() -> Result<DemoResult, Box<dyn Error>> {
    let root = std::env::temp_dir().join(format!(
        "agent-send-cli-demo-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    let source_root = root.join("source");
    let destination_root = root.join("destination");
    fs::create_dir_all(&source_root)?;
    fs::create_dir_all(&destination_root)?;
    let sample = b"agent-send deterministic demo\n";
    fs::write(source_root.join("sample.txt"), sample)?;

    let sender = Daemon::new(Config::with_identity_path(
        root.join("sender-identity.json"),
    ))?;
    let receiver = Daemon::new(Config::with_identity_path(
        root.join("receiver-identity.json"),
    ))?;
    sender.add_shared_folder("source", &source_root, FolderDirection::Read);
    receiver.add_shared_folder("destination", &destination_root, FolderDirection::Write);
    let request = TransferRequest {
        peer_id: receiver.identity().id.clone(),
        source_folder_id: "source".into(),
        source_paths: vec!["sample.txt".into()],
        destination_folder_id: "destination".into(),
        idempotency_key: "demo-sample-v1".into(),
    };
    let outcome = sender.send_to(&receiver, &request, &Cancellation::new(), |_| {})?;
    let destination = destination_root.join("sample.txt");
    let expected = hex_digest(sample);
    debug_assert_eq!(outcome.sha256, expected);

    // Start both daemon instances as part of the demo, without exposing a
    // non-loopback listener. The transfer itself uses their existing
    // in-process loopback engines.
    let first = sender.start()?;
    let second = receiver.start()?;
    first.shutdown()?;
    second.shutdown()?;
    let result = DemoResult {
        destination,
        bytes: outcome.bytes,
        sha256: outcome.sha256,
    };
    fs::remove_dir_all(root)?;
    Ok(result)
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands_and_rejects_public_addresses() {
        assert_eq!(parse_args(["--help"]).unwrap(), Command::Help);
        assert_eq!(parse_args(["demo"]).unwrap(), Command::Demo);
        assert_eq!(
            parse_args(["health", "--addr", "127.0.0.1:1234"]).unwrap(),
            Command::Health {
                addr: "127.0.0.1:1234".parse().unwrap()
            }
        );
        assert!(parse_args(["peers", "--addr", "8.8.8.8:53"]).is_err());
    }

    #[test]
    fn demo_transfers_the_fixed_sample() {
        let result = run_demo().unwrap();
        assert_eq!(
            result.bytes,
            b"agent-send deterministic demo\n".len() as u64
        );
        assert_eq!(
            result.sha256,
            hex_digest(b"agent-send deterministic demo\n")
        );
        assert!(!result.destination.exists()); // demo cleans up its temporary capability roots
    }
}
