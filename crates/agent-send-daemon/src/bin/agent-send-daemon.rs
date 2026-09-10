use agent_send_daemon::{Config, Daemon, MdnsDiscovery};
use std::error::Error;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

const HELP: &str = "agent-send-daemon\n\nUSAGE:\n    agent-send-daemon [OPTIONS]\n\nOPTIONS:\n    --bind HOST:PORT          Local loopback API address (default: 127.0.0.1:0)\n    --identity-path PATH      Persistent per-user identity file\n    --hidden                  Do not start LAN discovery\n    -h, --help                Print this help\n";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Arguments {
    bind: Option<SocketAddr>,
    identity_path: Option<PathBuf>,
    hidden: bool,
}

fn parse_args<I, S>(args: I) -> Result<Arguments, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let args: Vec<String> = args.into_iter().map(Into::into).collect();
    let mut result = Arguments {
        bind: None,
        identity_path: None,
        hidden: false,
    };
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--hidden" => result.hidden = true,
            "--bind" | "--identity-path" => {
                let flag = args[index].clone();
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| format!("{flag} requires a value"))?;
                if flag == "--bind" {
                    let address = value
                        .parse::<SocketAddr>()
                        .map_err(|_| "--bind must be HOST:PORT".to_owned())?;
                    if !address.ip().is_loopback() {
                        return Err("--bind must be a loopback address".into());
                    }
                    result.bind = Some(address);
                } else {
                    result.identity_path = Some(PathBuf::from(value));
                }
            }
            "--help" | "-h" => return Err(HELP.to_owned()),
            option => return Err(format!("unknown option: {option}\n\n{HELP}")),
        }
        index += 1;
    }
    Ok(result)
}

fn main() -> Result<(), Box<dyn Error>> {
    let arguments = match parse_args(std::env::args().skip(1)) {
        Ok(arguments) => arguments,
        Err(message) if message == HELP => {
            print!("{HELP}");
            return Ok(());
        }
        Err(message) => {
            eprintln!("agent-send-daemon: {message}");
            std::process::exit(2);
        }
    };

    let mut config = Config::load_user()?;
    if let Some(bind) = arguments.bind {
        config.bind_addr = bind;
    }
    if let Some(identity_path) = arguments.identity_path {
        config.identity_path = identity_path;
    }
    // Daemon::new validates this again, including values supplied by config files.
    let daemon = Daemon::new(config)?;
    let running = daemon.start()?;
    eprintln!("agent-send-daemon listening on {}", running.local_addr());

    // Browsing is intentionally best-effort: mDNS is optional on platforms
    // without a usable LAN service. The local API remains the only socket owned
    // by this process, and is always loopback-only.
    let _discovery = if arguments.hidden {
        None
    } else {
        match MdnsDiscovery::new() {
            Ok(discovery) => Some(discovery),
            Err(error) => {
                eprintln!("agent-send-daemon: discovery unavailable: {error}");
                None
            }
        }
    };

    let stopped = Arc::new(AtomicBool::new(false));
    let signal = stopped.clone();
    ctrlc::set_handler(move || signal.store(true, Ordering::Release))?;
    while !stopped.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(100));
    }
    running.shutdown()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_defaults_and_daemon_flags() {
        assert_eq!(
            parse_args(std::iter::empty::<String>()).unwrap(),
            Arguments {
                bind: None,
                identity_path: None,
                hidden: false,
            }
        );
        assert_eq!(
            parse_args([
                "--bind",
                "127.0.0.1:8123",
                "--identity-path",
                "/tmp/id",
                "--hidden"
            ])
            .unwrap(),
            Arguments {
                bind: Some("127.0.0.1:8123".parse().unwrap()),
                identity_path: Some(PathBuf::from("/tmp/id")),
                hidden: true,
            }
        );
    }

    #[test]
    fn rejects_non_loopback_bind() {
        assert!(parse_args(["--bind", "0.0.0.0:8123"]).is_err());
        assert!(parse_args(["--bind", "8.8.8.8:53"]).is_err());
    }
}
