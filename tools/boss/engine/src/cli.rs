use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "boss-engine")]
pub struct Cli {
    #[arg(long)]
    pub socket_path: Option<String>,
}
