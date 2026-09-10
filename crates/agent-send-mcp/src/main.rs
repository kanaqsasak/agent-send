use agent_send_mcp::{run_stdio, AdapterConfig, TcpDaemonClient};
use std::io::{self, BufReader};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = AdapterConfig::from_env()?;
    let mut client = TcpDaemonClient::new(config)?;
    run_stdio(BufReader::new(io::stdin()), io::stdout(), &mut client)?;
    Ok(())
}
