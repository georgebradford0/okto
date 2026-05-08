//! Merged binary that runs either the lair (parent / orchestrator) or agent
//! (child / per-pod agentic loop) role. The Docker image ships one binary;
//! the image's ENTRYPOINT runs the lair role, and child Deployments override
//! `command:` to flip the role to `agent`.

use clap::{Parser, ValueEnum};

mod bootstrap;
mod lair;
mod agent;

#[derive(Clone, Copy, ValueEnum)]
pub enum Role {
    Lair,
    Agent,
}

#[derive(Parser)]
#[command(version, about = "octo merged app — pick role with --role")]
struct Args {
    /// Which role to run.
    #[arg(long, value_enum)]
    role: Role,

    /// Print the Noise static pubkey (base32) for the picked role and exit.
    /// Used internally during boot to embed the pubkey in the QR code before
    /// the HTTP listener binds.
    #[arg(long)]
    print_pubkey: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    match args.role {
        Role::Lair  => lair::run(args.print_pubkey).await,
        Role::Agent => agent::run(args.print_pubkey).await,
    }
}
