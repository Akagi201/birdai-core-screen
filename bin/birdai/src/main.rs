//! `birdai` — decode, classify and reproduce against Sui mainnet.
//!
//! ```text
//! cargo run -- decode      # field-by-field dump of objects A, B, C and a tick child of A
//! cargo run -- classify    # apply the on-chain price-discovery test
//! cargo run -- reproduce   # recompute transaction T's output from the pool state it consumed
//! cargo run -- calibrate   # derive the pool's fixed-point format from its own state
//! cargo run -- follow      # stream checkpoints and keep venue state current
//! ```

// A CLI's whole job is to write to standard output, which is what the workspace otherwise forbids
// so that library code goes through `tracing` instead.
#![expect(clippy::print_stdout)]

mod commands;
mod constants;
mod session;
mod ticks;

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use session::Session;

/// Decode, classify and reproduce against Sui mainnet.
#[derive(Debug, Parser)]
#[command(name = "birdai", version, about, long_about = None)]
struct Cli {
    /// Sui fullnode gRPC v2 endpoint.
    #[arg(long, env = "BIRDAI_RPC_URL", default_value = constants::MAINNET_RPC, global = true)]
    rpc_url: String,

    /// API key, sent as an `x-api-key` header. Needed by hosted providers that put the key in the
    /// URL path, which a gRPC client cannot use.
    #[arg(long, env = "BIRDAI_API_KEY", global = true)]
    api_key: Option<String>,

    /// Archival endpoint used for checkpoint and historical-object reads.
    ///
    /// Sui's public fullnode keeps only a bounded window of history, while
    /// `archive.mainnet.sui.io` keeps the whole history — but the archival node does not
    /// implement `StateService`, so `ListDynamicFields` is unavailable there. A run that needs
    /// old state as well as the current dynamic-field index has to talk to both: versioned
    /// object reads prefer the archive, latest reads and dynamic fields stay on the fullnode.
    #[arg(
        long,
        env = "BIRDAI_ARCHIVE_URL",
        default_value = constants::MAINNET_ARCHIVE,
        global = true
    )]
    archive_url: Option<String>,

    /// Replay from a captured fixture directory instead of the network.
    #[arg(long, env = "BIRDAI_FIXTURES", value_name = "DIR", global = true)]
    fixtures: Option<PathBuf>,

    /// Verbose logging.
    #[arg(long, short, global = true)]
    verbose: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Decode objects A, B, C and one tick child of A, field by field.
    Decode,
    /// Apply the on-chain price-discovery test to A, B and C.
    Classify,
    /// Recompute transaction T's output from the pool state it consumed.
    Reproduce,
    /// Derive the pool's fixed-point format from its own state.
    Calibrate,
    /// Stream checkpoints and keep in-memory venue state current.
    Follow {
        /// First checkpoint to apply.
        #[arg(long, default_value_t = constants::TX_T_CHECKPOINT)]
        from: u64,
        /// Stop after this many checkpoints that touched a venue.
        #[arg(long, default_value_t = 5)]
        count: u64,
    },
    /// Capture everything the other commands need into a fixture directory, for offline replay.
    Fetch {
        /// Where to write the fixture set.
        #[arg(long, default_value = "fixtures")]
        out: PathBuf,
    },
}

#[tokio::main]
async fn main() -> eyre::Result<()> {
    let cli = Cli::parse();

    let level = if cli.verbose { tracing::Level::DEBUG } else { tracing::Level::WARN };
    tracing_subscriber::fmt().with_max_level(level).with_target(false).init();

    // `fetch` is the one command that must run online, because it is what produces the fixture set
    // every other command can then replay without a node.
    let session = if let Some(directory) = cli.fixtures.clone() {
        if matches!(cli.command, Command::Fetch { .. }) {
            return Err(eyre::eyre!(
                "`fetch` writes fixtures; leaving --fixtures set would read them instead"
            ));
        }
        println!("offline replay from {}", directory.display());
        Session::offline(&directory)?
    } else {
        // An empty archive URL opts out of the split.
        let archive = cli.archive_url.as_deref().filter(|url| !url.is_empty());
        println!("rpc {}    checkpoints {}", cli.rpc_url, archive.unwrap_or("(same endpoint)"));
        Session::connect(&cli.rpc_url, cli.api_key.as_deref(), archive)?
    };

    match cli.command {
        Command::Decode => commands::decode(&session).await,
        Command::Classify => commands::classify(&session).await,
        Command::Reproduce => commands::reproduce(&session).await,
        Command::Calibrate => commands::calibrate(&session).await,
        Command::Follow { from, count } => {
            commands::follow(&session, &cli.rpc_url, from, count).await
        }
        Command::Fetch { out } => commands::fetch(&session, &out).await,
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::Cli;

    #[test]
    fn the_cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
