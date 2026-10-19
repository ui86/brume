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
            println!("配置校验通过");
            return Ok(());
        }
        Command::Serve(config) => config,
    };
    if config.whitelist.is_empty() {
        eprintln!("警告：白名单为空，所有 IP 均可连接");
    }
    let port = config.port;
    let shutdown = CancellationToken::new();
    let signal = shutdown.clone();
    ctrlc::set_handler(move || signal.cancel())?;
    let server = Server::bind(config, shutdown)?;
    eprintln!("Brume 正在监听 TCP/UDP 端口 {port}");
    server.run()?;
    Ok(())
}
