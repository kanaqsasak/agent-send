//! Repeatable local measurement harness; it does not contact the LAN.

use agent_send_core::{FolderDirection, TransferRequest};
use agent_send_daemon::{Cancellation, Config, Daemon};
use std::{
    error::Error,
    fs,
    io::Write,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const SAMPLE_BYTES: usize = 4 * 1024 * 1024;
const IDLE_WINDOW: Duration = Duration::from_millis(250);

fn main() -> Result<(), Box<dyn Error>> {
    let root = std::env::temp_dir().join(format!(
        "agent-send-measure-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    let result = measure(&root);
    let _ = fs::remove_dir_all(&root);
    result
}

fn measure(root: &std::path::Path) -> Result<(), Box<dyn Error>> {
    let source_root = root.join("source");
    let destination_root = root.join("destination");
    fs::create_dir_all(&source_root)?;
    fs::create_dir_all(&destination_root)?;
    let mut sample = fs::File::create(source_root.join("sample.bin"))?;
    for _ in 0..(SAMPLE_BYTES / 4096) {
        sample.write_all(&[0x5a; 4096])?;
    }
    sample.sync_all()?;

    let mut sender_config = Config::with_identity_path(root.join("sender-identity.json"));
    sender_config.peer_bind_addr = "127.0.0.1:0".parse()?;
    sender_config.discovery_enabled = false;
    let mut receiver_config = Config::with_identity_path(root.join("receiver-identity.json"));
    receiver_config.peer_bind_addr = "127.0.0.1:0".parse()?;
    receiver_config.discovery_enabled = false;
    let sender = Daemon::new(sender_config)?;
    let receiver = Daemon::new(receiver_config)?;
    sender.add_shared_folder("source", &source_root, FolderDirection::Read);
    receiver.add_shared_folder("destination", &destination_root, FolderDirection::Write);

    let request = TransferRequest {
        peer_id: receiver.identity().id.clone(),
        source_folder_id: "source".into(),
        source_paths: vec!["sample.bin".into()],
        destination_folder_id: "destination".into(),
        idempotency_key: "local-measure-v1".into(),
    };
    let transfer_started = Instant::now();
    let outcome = sender.send_to(&receiver, &request, &Cancellation::new(), |_| {})?;
    let transfer_elapsed = transfer_started.elapsed();
    let throughput_mib_s =
        (outcome.bytes as f64 / (1024.0 * 1024.0)) / transfer_elapsed.as_secs_f64();

    let running = sender.start()?;
    let idle_started = Instant::now();
    std::thread::sleep(IDLE_WINDOW);
    running.shutdown()?;
    let idle_elapsed = idle_started.elapsed();

    println!("bytes={}", outcome.bytes);
    println!("throughput_mib_per_sec={throughput_mib_s:.2}");
    println!("idle_wall_ms={}", idle_elapsed.as_millis());
    println!("idle_cpu_not_measured=true");
    Ok(())
}
