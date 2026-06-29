//! fs2 CLI entry point.

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "fs2", version, about = "Developer-focused cross-machine code sync", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, clap::Subcommand)]
enum Commands {
    /// Show sync and workspace status.
    Status,
    /// Log in to a backend.
    Login,
    /// Log out and clear local tokens.
    Logout,
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Status) => println!("fs2: no workspace mounted"),
        Some(Commands::Login) => println!("fs2: login not yet implemented"),
        Some(Commands::Logout) => println!("fs2: logged out"),
        None => println!("fs2: see `fs2 --help`"),
    }
}
