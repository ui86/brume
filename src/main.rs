use brume::config::{Command, Config};
use brume::server::Server;
use std::error::Error;
use tokio_util::sync::CancellationToken;

fn main() -> Result<(), Box<dyn Error>> {
    let Some(command) = Config::parse_command(std::env::args().skip(1))? else {
        return Ok(());
    };
    let config = match command {
        Command::CheckConfig(_) => {
            println!("Configuration is valid");
            return Ok(());
        }
        Command::Serve(config) => config,
    };
    if config.whitelist.is_empty() {
        eprintln!("Warning: The IP allowlist is empty. Connections from any IP are allowed");
    }
    let port = config.port;
    let shutdown = CancellationToken::new();
    let signal = shutdown.clone();
    ctrlc::set_handler(move || signal.cancel())?;
    let server = Server::bind(config, shutdown)?;
    eprintln!("Brume is listening on TCP/UDP port {port}");
    server.run()?;
    Ok(())
}
