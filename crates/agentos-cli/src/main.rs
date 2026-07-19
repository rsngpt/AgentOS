//! `agentos` — CLI client for `agentosd`.
//!
//! Example:
//! ```text
//! agentos run --mount ./project:rw --net allowlist:api.openai.com -- python3 agent.py
//! ```

mod args;
mod client;

use clap::Parser;
use serde_json::json;

use args::{Cli, Command};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let result: Result<i32, String> = match cli.command {
        Command::Run(run) => match run.into_spec() {
            Ok(spec) => client::run(spec).await,
            Err(e) => Err(e.to_string()),
        },
        Command::Ps => client::unary("sandbox.list", json!(null)).await.map(|v| {
            let rows = v.as_array().cloned().unwrap_or_default();
            if rows.is_empty() {
                println!("no sandboxes");
            } else {
                println!("{:<38} {:<16} STATE", "ID", "NAME");
                for r in rows {
                    println!(
                        "{:<38} {:<16} {}",
                        r["id"].as_str().unwrap_or("?"),
                        r["name"].as_str().unwrap_or("?"),
                        r["state"]["state"].as_str().unwrap_or("?")
                    );
                }
            }
            0
        }),
        Command::Kill { id, save } => client::unary("sandbox.kill", json!({ "id": id, "save": save }))
            .await
            .map(|_| {
                println!("killed");
                0
            }),
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
