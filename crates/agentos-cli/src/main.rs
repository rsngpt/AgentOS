//! `agentos` — CLI client for `agentosd`.
//!
//! Example:
//! ```text
//! agentos run --mount ./project:rw --net allowlist:api.openai.com -- python3 agent.py
//! ```

mod args;

use clap::Parser;

use args::{Cli, Command};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Run(run) => match run.into_spec() {
            Ok(spec) => {
                // M1: sandbox.create over the daemon socket, then attach stdio.
                println!("would create sandbox with spec:");
                println!("{}", serde_json::to_string_pretty(&spec).unwrap());
                eprintln!("(daemon integration lands in milestone M1)");
                Ok(())
            }
            Err(e) => Err(e.to_string()),
        },
        Command::Ps => {
            // M1: sandbox.list over the daemon socket.
            eprintln!("(daemon integration lands in milestone M1)");
            Ok(())
        }
        Command::Kill { id, save } => {
            // M1: sandbox.kill over the daemon socket.
            let disposition = if save { "save" } else { "wipe" };
            eprintln!("would kill sandbox {id} ({disposition}); daemon integration lands in M1");
            Ok(())
        }
    };

    if let Err(msg) = result {
        eprintln!("error: {msg}");
        std::process::exit(2);
    }
}
