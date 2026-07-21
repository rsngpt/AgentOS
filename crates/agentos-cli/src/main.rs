//! `agentos` — CLI client for `agentosd`.
//!
//! Example:
//! ```text
//! agentos run --mount ./project:rw --net allowlist:api.openai.com -- python3 agent.py
//! ```

mod args;
mod client;

use clap::Parser;

use args::{Cli, Command};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let result: Result<i32, String> = match cli.command {
        Command::Run(run) => match run.into_spec() {
            Ok(spec) => client::run(spec).await,
            Err(e) => Err(e.to_string()),
        },
        Command::Ps => client::list().await,
        Command::Kill { id, save } => client::kill(&id, save).await,
        Command::Pause { id } => client::pause(&id).await,
        Command::Resume { id } => client::resume(&id).await,
        Command::Snapshot { id } => client::snapshot(&id).await,
        Command::Restore { id } => client::restore(&id).await,
        Command::Policy => match agentos_core::FleetPolicy::load() {
            Ok(p) if p.is_empty() => {
                println!("no fleet policy in effect on this machine");
                println!("(a managed deployment would place one at {})",
                    agentos_core::policy::SYSTEM_POLICY_PATH);
                Ok(0)
            }
            Ok(p) => {
                println!("{}", serde_json::to_string_pretty(&p).unwrap_or_default());
                Ok(0)
            }
            Err(e) => Err(e.to_string()),
        },
        Command::Events => client::events().await,
    };

    match result {
        Ok(code) => std::process::exit(code),
        Err(msg) => {
            eprintln!("error: {msg}");
            std::process::exit(2);
        }
    }
}
