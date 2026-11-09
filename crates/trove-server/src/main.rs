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
    init_tracing(&cfg);
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => trove_server::serve(cfg).await,
        Command::Config => {
            let mut redacted = cfg.clone();
            if !redacted.security.initial_admin_password.is_empty() {
                redacted.security.initial_admin_password = "<redacted>".into();
            }
            if !redacted.storage.s3.secret_access_key.is_empty() {
                redacted.storage.s3.secret_access_key = "<redacted>".into();
            }
            if let Some(at) = redacted.database.url.find('@') {
                if let Some(colon) = redacted.database.url[..at].rfind(':') {
                    if !redacted.database.url[..colon].ends_with('/') {
                        redacted.database.url.replace_range(colon + 1..at, "<redacted>");
                    }
                }
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
