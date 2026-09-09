fn main() {
    match agent_send_cli::parse_args(std::env::args().skip(1)) {
        Ok(command) => {
            if let Err(error) = agent_send_cli::run(command) {
                eprintln!("agent-send: {error}");
                std::process::exit(1);
            }
        }
        Err(error) => {
            eprintln!("agent-send: {error}");
            std::process::exit(2);
        }
    }
}
