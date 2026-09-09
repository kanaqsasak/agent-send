//! The agent-send background daemon foundation.
//!
//! The local API is deliberately small and transport-independent types are kept
//! public so a different local transport can be added without changing daemon
//! state or configuration.

use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub const API_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    /// The local API must remain loopback-only.
    pub bind_addr: SocketAddr,
    /// File containing the daemon's local identity placeholder.
    pub identity_path: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_addr: SocketAddr::from(([127, 0, 0, 1], 0)),
            identity_path: default_identity_path(),
        }
    }
}

impl Config {
    pub fn with_identity_path(path: impl Into<PathBuf>) -> Self {
        Self {
            identity_path: path.into(),
            ..Self::default()
        }
    }

    fn validate(&self) -> Result<(), DaemonError> {
        if !self.bind_addr.ip().is_loopback() {
            return Err(DaemonError::NonLoopbackBind(self.bind_addr));
        }
        if self.identity_path.as_os_str().is_empty() {
            return Err(DaemonError::MissingIdentityPath);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalIdentity {
    pub id: String,
    /// Placeholder for the future device key. It is persisted now so identity
    /// remains stable across restarts, without pretending to be a key yet.
    pub key_placeholder: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthResponse {
    pub version: u32,
    pub status: HealthStatus,
    pub identity_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    Ok,
}

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("daemon API must bind to loopback, not {0}")]
    NonLoopbackBind(SocketAddr),
    #[error("identity path is required")]
    MissingIdentityPath,
    #[error("identity storage failed: {0}")]
    Identity(#[source] io::Error),
    #[error("failed to start local API: {0}")]
    Bind(#[source] io::Error),
    #[error("daemon thread failed to stop")]
    Shutdown,
}

pub struct Daemon {
    config: Config,
    identity: LocalIdentity,
}

impl Daemon {
    pub fn new(config: Config) -> Result<Self, DaemonError> {
        config.validate()?;
        let identity = load_or_create_identity(&config.identity_path)?;
        Ok(Self { config, identity })
    }

    pub fn identity(&self) -> &LocalIdentity {
        &self.identity
    }

    pub fn start(self) -> Result<RunningDaemon, DaemonError> {
        let listener = TcpListener::bind(self.config.bind_addr).map_err(DaemonError::Bind)?;
        listener.set_nonblocking(true).map_err(DaemonError::Bind)?;
        let local_addr = listener.local_addr().map_err(DaemonError::Bind)?;
        let identity = self.identity;
        let (shutdown_tx, shutdown_rx) = mpsc::channel();

        let thread = thread::Builder::new()
            .name("agent-send-health".into())
            .spawn(move || run_server(listener, identity, shutdown_rx))
            .map_err(DaemonError::Bind)?;

        Ok(RunningDaemon {
            local_addr,
            shutdown_tx: Some(shutdown_tx),
            thread: Some(thread),
        })
    }
}

pub struct RunningDaemon {
    local_addr: SocketAddr,
    shutdown_tx: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl RunningDaemon {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Signals the server and waits for its worker to exit.
    pub fn shutdown(mut self) -> Result<(), DaemonError> {
        self.shutdown_tx
            .take()
            .expect("shutdown sender present")
            .send(())
            .ok();
        self.thread
            .take()
            .expect("daemon thread present")
            .join()
            .map_err(|_| DaemonError::Shutdown)
    }
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        if let Some(sender) = self.shutdown_tx.take() {
            let _ = sender.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run_server(listener: TcpListener, identity: LocalIdentity, shutdown: mpsc::Receiver<()>) {
    loop {
        if shutdown.try_recv().is_ok() {
            return;
        }
        match listener.accept() {
            Ok((stream, _)) => handle_connection(stream, &identity),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return,
        }
    }
}

fn handle_connection(mut stream: TcpStream, identity: &LocalIdentity) {
    let mut request = [0; 1024];
    let Ok(size) = stream.read(&mut request) else {
        return;
    };
    let request = String::from_utf8_lossy(&request[..size]);
    let first_line = request.lines().next().unwrap_or_default();
    let (status, body) =
        if first_line == "GET /v1/health HTTP/1.1" || first_line == "GET /v1/health HTTP/1.0" {
            let response = HealthResponse {
                version: API_VERSION,
                status: HealthStatus::Ok,
                identity_id: identity.id.clone(),
            };
            (
                "200 OK",
                serde_json::to_string(&response).expect("health response is serializable"),
            )
        } else {
            ("404 Not Found", "{\"error\":\"not_found\"}".to_owned())
        };
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body.as_bytes());
}

fn load_or_create_identity(path: &Path) -> Result<LocalIdentity, DaemonError> {
    match fs::read_to_string(path) {
        Ok(contents) => serde_json::from_str(&contents).map_err(|error| {
            DaemonError::Identity(io::Error::new(io::ErrorKind::InvalidData, error))
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                fs::create_dir_all(parent).map_err(DaemonError::Identity)?;
            }
            let identity = new_identity();
            let contents = serde_json::to_vec_pretty(&identity).expect("identity is serializable");
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .map_err(DaemonError::Identity)?;
            file.write_all(&contents).map_err(DaemonError::Identity)?;
            Ok(identity)
        }
        Err(error) => Err(DaemonError::Identity(error)),
    }
}

fn new_identity() -> LocalIdentity {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let value = format!("{nanos:032x}");
    LocalIdentity {
        id: format!("device-{}", &value[..16]),
        key_placeholder: value,
    }
}

fn default_identity_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".agent-send")
        .join("identity.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;
    use std::time::Duration;

    fn config(path: &Path) -> Config {
        Config::with_identity_path(path)
    }

    #[test]
    fn startup_rejects_non_loopback_and_persists_identity() {
        let path =
            std::env::temp_dir().join(format!("agent-send-test-{}-startup", std::process::id()));
        let bad = Config {
            bind_addr: "0.0.0.0:0".parse().unwrap(),
            ..config(&path)
        };
        assert!(matches!(
            Daemon::new(bad),
            Err(DaemonError::NonLoopbackBind(_))
        ));

        let first = Daemon::new(config(&path)).unwrap();
        let id = first.identity().clone();
        let second = Daemon::new(config(&path)).unwrap();
        assert_eq!(second.identity(), &id);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn health_is_versioned_and_shutdown_is_deterministic() {
        let path =
            std::env::temp_dir().join(format!("agent-send-test-{}-health", std::process::id()));
        let running = Daemon::new(config(&path)).unwrap().start().unwrap();
        let mut stream = TcpStream::connect(running.local_addr()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        stream
            .write_all(b"GET /v1/health HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.contains("\"version\":1"));
        assert!(response.contains("\"status\":\"ok\""));
        running.shutdown().unwrap();
        fs::remove_file(path).unwrap();
    }
}
