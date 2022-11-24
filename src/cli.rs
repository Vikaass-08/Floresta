use std::path::PathBuf;

use clap::{arg, command, Parser, Subcommand};

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Sets a custom config file
    #[arg(short, long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Turn debugging information on
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub debug: u8,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Starts up your wallet and do nothing else
    Setup {
        /// Your wallet's descriptor
        #[arg(short, long)]
        wallet_desc: String,
        /// Where should we store data
        #[arg(short, long)]
        #[arg(default_value = Some("~/.utreexo_wallet/".into()))]
        data_dir: Option<String>,
    },
    /// Starts your wallet and server
    Run {
        /// Where should we store data
        #[arg(default_value = Some("~/.utreexo_wallet/".into()))]
        data_dir: Option<String>,
        /// Your wallet's descriptor
        #[arg(short, long)]
        wallet_desc: Option<String>,
        /// Your rpc user, as set in Utreexod
        #[arg(long)]
        #[arg(default_value = "")]
        rpc_user: String,
        /// Your rpc password, as set in Utreexod
        #[arg(long)]
        #[arg(default_value = "")]
        rpc_password: String,
        /// The hostname:port of Utreexod
        #[arg(short, long)]
        #[arg(default_value = "localhost:18332")]
        rpc_host: String,
    },
}
