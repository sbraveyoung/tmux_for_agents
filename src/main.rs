#[allow(dead_code)] // paths API is consumed from Task 4 onward
mod paths;

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
        Command::Daemon => todo!("task 5"),
        Command::Hook { .. } => std::process::exit(0), // hook 纪律：未实现也静默
        Command::Status { .. } => println!("tfa:off"),
        Command::List => println!("[]"),
    }
}
