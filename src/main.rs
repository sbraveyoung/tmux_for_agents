mod client;
mod commands;
mod daemon;
mod notify;
mod paths;

#[allow(dead_code)] // remaining discipline-only fields consumed by Task 6
mod config;

mod event;

mod protocol;
mod render;
mod state;

mod sources;

mod scanner;

mod quota;

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
    /// Send or test notifications
    Notify {
        /// "test" | "send"
        #[arg(default_value = "test")]
        action: String,
    },
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
        Command::Hook { agent, event } => commands::hook::run(&agent, &event),
        Command::Status { format } => commands::status::run(&format),
        Command::List => match client::request(&protocol::Request::Snapshot) {
            Ok(protocol::Response::Snapshot { sessions, quota, .. }) => {
                let out = serde_json::json!({ "sessions": sessions, "quota": quota });
                println!("{}", serde_json::to_string(&out).unwrap_or_default());
            }
            _ => println!("{{\"sessions\":[],\"quota\":[]}}"),
        },
        Command::Notify { action } => commands::notify::run(&action),
    }
}
