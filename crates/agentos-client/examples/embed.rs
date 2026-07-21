//! Embedding Agent OS: run untrusted code in a hardware-isolated sandbox and
//! stream its output.
//!
//!     cargo run -p agentos-client --example embed -- python3 -c 'print(2**64)'
//!
//! Everything is deny-by-default — this sandbox gets no host files and no
//! network unless you add them to the spec.

use agentos_client::{Client, RunEvent};
use agentos_core::SandboxSpec;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() {
        eprintln!("usage: embed <command> [args...]");
        std::process::exit(2);
    }

    let mut spec = SandboxSpec::command(argv);
    // Give the agent a modest budget and stop it if it runs away.
    spec.limits.mem_mib = 1024;
    spec.auto_kill.max_runtime_secs = Some(120);
    // To let it reach the network, uncomment:
    // spec.net = NetPolicy::Allowlist(vec!["pypi.org".into()]);

    let client = Client::new();
    let mut run = client.run(&spec).await?;

    let mut exit = 1;
    while let Some(event) = run.next().await? {
        match event {
            RunEvent::Created(id) => eprintln!("[sandbox {id}]"),
            RunEvent::Stdout(bytes) => print!("{}", String::from_utf8_lossy(&bytes)),
            RunEvent::Stderr(bytes) => eprint!("{}", String::from_utf8_lossy(&bytes)),
            RunEvent::Exited { code, .. } => {
                exit = code.unwrap_or(1);
                break;
            }
            RunEvent::Terminated { reason, .. } => {
                eprintln!("[terminated: {reason}]");
                exit = 137;
                break;
            }
            _ => {}
        }
    }
    std::process::exit(exit);
}
