use clap::Parser;
use serpula::app::Cli;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    serpula::app::execute(&ureq::Agent::new_with_defaults(), cli.command)
}
