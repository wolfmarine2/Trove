use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;
use trove_core::config::Config;

/// Trove — a self-hosted universal artifact repository.
#[derive(Parser)]
#[command(name = "trove", version, about)]
struct Cli {
    /// Path to the configuration file.
    #[arg(short, long, env = "TROVE_CONFIG", default_value = "trove.toml", global = true)]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start the server (default).
    Serve,
    /// Print the effective configuration (secrets redacted) and exit.
    Config,
    /// Exit 0 if the local server answers /healthz (container health checks).
    Healthcheck,
    /// Reset a user's password (run on the server host).
    ResetPassword {
        username: String,
        #[arg(long, env = "TROVE_NEW_PASSWORD")]
        password: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load(Some(&cli.config))?;
    if let Some(Command::Healthcheck) = cli.command {
        return healthcheck(&cfg.server.bind);
    }
    init_tracing(&cfg);
    match cli.command.unwrap_or(Command::Serve) {
        Command::Healthcheck => unreachable!("handled above"),
        Command::Serve => trove_server::serve(cfg).await,
        Command::Config => {
            let mut redacted = cfg.clone();
            if !redacted.security.initial_admin_password.is_empty() {
                redacted.security.initial_admin_password = "<redacted>".into();
            }
            if !redacted.storage.s3.secret_access_key.is_empty() {
                redacted.storage.s3.secret_access_key = "<redacted>".into();
            }
            if let Some(at) = redacted.database.url.find('@')
                && let Some(colon) = redacted.database.url[..at].rfind(':')
                && !redacted.database.url[..colon].ends_with('/')
            {
                redacted.database.url.replace_range(colon + 1..at, "<redacted>");
            }
            println!("{}", toml::to_string_pretty(&redacted)?);
            Ok(())
        }
        Command::ResetPassword { username, password } => {
            trove_server::auth::validate_password(&password)?;
            let state = trove_server::init(cfg).await?;
            let mut user = state
                .db
                .get_user_by_name(&username)
                .await?
                .ok_or_else(|| anyhow::anyhow!("user '{username}' not found"))?;
            user.password_hash = trove_server::auth::hash_password(&password)?;
            user.disabled = false;
            state.db.update_user(&user).await?;
            println!("password for '{username}' updated");
            Ok(())
        }
    }
}

fn init_tracing(cfg: &Config) {
    let filter = EnvFilter::try_from_env("RUST_LOG").unwrap_or_else(|_| EnvFilter::new(&cfg.log.level));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    if cfg.log.json {
        builder.json().init();
    } else {
        builder.init();
    }
}

/// Minimal HTTP probe without proxies or TLS, suitable for HEALTHCHECK.
fn healthcheck(bind: &str) -> anyhow::Result<()> {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
    use std::time::Duration;

    let addr: SocketAddr = bind.to_socket_addrs()?.next().ok_or_else(|| anyhow::anyhow!("cannot resolve {bind}"))?;
    let target = if addr.ip().is_unspecified() {
        let loopback = if addr.is_ipv4() { "127.0.0.1" } else { "::1" };
        SocketAddr::new(loopback.parse()?, addr.port())
    } else {
        addr
    };
    let mut stream = TcpStream::connect_timeout(&target, Duration::from_secs(3))?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.write_all(b"GET /healthz HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    if response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200") {
        Ok(())
    } else {
        anyhow::bail!("unhealthy: {}", response.lines().next().unwrap_or("no response"))
    }
}
