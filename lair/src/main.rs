//! Merged binary that runs either the lair (parent / orchestrator) or agent
//! (child) role. The same `lair` binary is invoked by the operator (as
//! `--role lair`) and re-spawned by lair to start each child (`--role agent`).

use clap::{Parser, ValueEnum};

mod agent;
mod agent_proc;
mod agent_tokens;
mod bootstrap;
mod lair;
mod ssh;

#[derive(Clone, Copy, ValueEnum)]
pub enum Role {
    Lair,
    Agent,
}

#[derive(Parser)]
#[command(version, about = "okto merged lair binary — pick role with --role")]
struct Args {
    /// Which role to run.
    #[arg(long, value_enum)]
    role: Role,

    /// Print the Noise static pubkey (base32) for the lair role and exit.
    /// Used internally during boot to embed the pubkey in the QR code before
    /// the HTTP listener binds. Only meaningful with `--role lair`.
    #[arg(long)]
    print_pubkey: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    match args.role {
        Role::Lair => {
            let result = lair::run(args.print_pubkey).await;
            if let Err(e) = &result {
                tracing::error!("[main] lair role exited with error: {e:#}");
            }
            result
        }
        Role::Agent => {
            let result = agent::run().await;
            if let Err(e) = &result {
                tracing::error!("[main] agent role exited with error: {e:#}");
            }
            result
        }
    }
}
