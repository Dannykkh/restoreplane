// crates/dbbackup-cli/src/main.rs
mod cli;
mod commands;

use clap::Parser;
use cli::{Cli, Command};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.command {
        Command::Init(args) => commands::init::run(&cli.config, args),
        Command::Doctor => commands::doctor::run(&cli.config),
        Command::TestConnection(args) => commands::test_connection::run(&cli.config, args),
        Command::AddDb(args) => commands::add_db::run(&cli.config, args),
        Command::AddDest(args) => commands::add_dest::run(&cli.config, args),
        Command::Run(args) => commands::run::run(&cli.config, args),
        Command::Verify(args) => commands::verify::run(&cli.config, args),
        Command::Restore(args) => commands::restore::run(&cli.config, args),
        Command::RotateKey(args) => commands::rotate_key::run(&cli.config, args),
        Command::RegisterDpapi => commands::register_dpapi::run(&cli.config),
        Command::Keys { action } => commands::keys::run(&cli.config, action),
        Command::Catalog { action } => commands::catalog::run(&cli.config, action),
        // 주의: PruneArgs.prune_config만 넘어간다 — cli.config(dbbackup.toml)는
        // prune 경로에 절대 도달하지 않는다(H1/H2 구조적 강제).
        Command::Prune(args) => commands::prune::run(args),
        Command::Schedule(args) => commands::schedule::run(&cli.config, args),
    }
}

fn init_tracing(verbose: u8) {
    let level = match verbose {
        0 => tracing::Level::INFO,
        1 => tracing::Level::DEBUG,
        _ => tracing::Level::TRACE,
    };
    tracing_subscriber::fmt().with_max_level(level).init();
}
