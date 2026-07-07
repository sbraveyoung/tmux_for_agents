mod daemon;

#[allow(dead_code)] // some paths helpers are consumed from Task 5+ onward
mod paths;

#[allow(dead_code)] // AgentKind::label is consumed from Task 7 onward
mod event;

mod protocol;

#[allow(dead_code)] // some StateStore methods are consumed from Task 5+ onward
mod state;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "tfa", about = "tmux for agents — AI agent observability")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon in the foreground
    Daemon,
    /// Forward an agent hook event to the daemon (called by agent hooks)
    Hook { agent: String, event: String },
    /// Render current agent states
    Status {
        #[arg(long, default_value = "tmux")]
        format: String,
    },
    /// Dump full state as JSON
    List,
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Daemon => {
            if let Err(e) = daemon::run() {
                eprintln!("tfa daemon: {e}");
                std::process::exit(1);
            }
        }
        Command::Hook { .. } => std::process::exit(0), // hook 纪律：未实现也静默
        Command::Status { .. } => println!("tfa:off"),
        Command::List => println!("[]"),
    }
}
