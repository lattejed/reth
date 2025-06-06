//! Command that initializes the node by importing OP Mainnet chain segment below Bedrock, from a
//! file.

use crate::{
    args::{
        utils::{genesis_value_parser, SUPPORTED_CHAINS},
        DatabaseArgs,
    },
    commands::import::{build_import_pipeline, load_config},
    dirs::{DataDirPath, MaybePlatformPath},
    version::SHORT_VERSION,
};
use clap::Parser;
use reth_beacon_consensus::EthBeaconConsensus;
use reth_config::{config::EtlConfig, Config};

use reth_db::{init_db, tables, transaction::DbTx};
use reth_downloaders::file_client::{
    ChunkedFileReader, FileClient, DEFAULT_BYTE_LEN_CHUNK_CHAIN_FILE,
};

use reth_node_core::init::init_genesis;

use reth_primitives::{op_mainnet::is_dup_tx, stage::StageId, PruneModes};
use reth_provider::{ProviderFactory, StageCheckpointReader, StaticFileProviderFactory};
use reth_static_file::StaticFileProducer;
use std::{path::PathBuf, sync::Arc};

use tracing::{debug, error, info};

/// Syncs RLP encoded blocks from a file.
#[derive(Debug, Parser)]
pub struct ImportOpCommand {
    /// The path to the configuration file to use.
    #[arg(long, value_name = "FILE", verbatim_doc_comment)]
    config: Option<PathBuf>,

    /// The path to the data dir for all reth files and subdirectories.
    ///
    /// Defaults to the OS-specific data directory:
    ///
    /// - Linux: `$XDG_DATA_HOME/reth/` or `$HOME/.local/share/reth/`
    /// - Windows: `{FOLDERID_RoamingAppData}/reth/`
    /// - macOS: `$HOME/Library/Application Support/reth/`
    #[arg(long, value_name = "DATA_DIR", verbatim_doc_comment, default_value_t)]
    datadir: MaybePlatformPath<DataDirPath>,

    /// Chunk byte length.
    #[arg(long, value_name = "CHUNK_LEN", verbatim_doc_comment)]
    chunk_len: Option<u64>,

    #[command(flatten)]
    db: DatabaseArgs,

    /// The path to a block file for import.
    ///
    /// The online stages (headers and bodies) are replaced by a file import, after which the
    /// remaining stages are executed.
    #[arg(value_name = "IMPORT_PATH", verbatim_doc_comment)]
    path: PathBuf,
}

impl ImportOpCommand {
    /// Execute `import` command
    pub async fn execute(self) -> eyre::Result<()> {
        info!(target: "reth::cli", "reth {} starting", SHORT_VERSION);

        info!(target: "reth::cli",
            "Disabled stages requiring state, since cannot execute OVM state changes"
        );

        debug!(target: "reth::cli",
            chunk_byte_len=self.chunk_len.unwrap_or(DEFAULT_BYTE_LEN_CHUNK_CHAIN_FILE),
            "Chunking chain import"
        );

        let chain_spec = genesis_value_parser(SUPPORTED_CHAINS[0])?;

        // add network name to data dir
        let data_dir = self.datadir.unwrap_or_chain_default(chain_spec.chain);
        let config_path = self.config.clone().unwrap_or_else(|| data_dir.config());

        let mut config: Config = load_config(config_path.clone())?;
        info!(target: "reth::cli", path = ?config_path, "Configuration loaded");

        // Make sure ETL doesn't default to /tmp/, but to whatever datadir is set to
        if config.stages.etl.dir.is_none() {
            config.stages.etl.dir = Some(EtlConfig::from_datadir(data_dir.data_dir()));
        }

        let db_path = data_dir.db();

        info!(target: "reth::cli", path = ?db_path, "Opening database");
        let db = Arc::new(init_db(db_path, self.db.database_args())?);

        info!(target: "reth::cli", "Database opened");
        let provider_factory =
            ProviderFactory::new(db.clone(), chain_spec.clone(), data_dir.static_files())?;

        debug!(target: "reth::cli", chain=%chain_spec.chain, genesis=?chain_spec.genesis_hash(), "Initializing genesis");

        init_genesis(provider_factory.clone())?;

        let consensus = Arc::new(EthBeaconConsensus::new(chain_spec.clone()));
        info!(target: "reth::cli", "Consensus engine initialized");

        // open file
        let mut reader = ChunkedFileReader::new(&self.path, self.chunk_len).await?;

        let mut total_decoded_blocks = 0;
        let mut total_decoded_txns = 0;
        let mut total_filtered_out_dup_txns = 0;

        while let Some(mut file_client) = reader.next_chunk::<FileClient>().await? {
            // create a new FileClient from chunk read from file
            info!(target: "reth::cli",
                "Importing chain file chunk"
            );

            let tip = file_client.tip().ok_or(eyre::eyre!("file client has no tip"))?;
            info!(target: "reth::cli", "Chain file chunk read");

            total_decoded_blocks += file_client.headers_len();
            total_decoded_txns += file_client.total_transactions();

            for (block_number, body) in file_client.bodies_iter_mut() {
                body.transactions.retain(|_| {
                    if is_dup_tx(block_number) {
                        total_filtered_out_dup_txns += 1;
                        return false
                    }
                    true
                })
            }

            let (mut pipeline, events) = build_import_pipeline(
                &config,
                provider_factory.clone(),
                &consensus,
                Arc::new(file_client),
                StaticFileProducer::new(
                    provider_factory.clone(),
                    provider_factory.static_file_provider(),
                    PruneModes::default(),
                ),
                true,
            )
            .await?;

            // override the tip
            pipeline.set_tip(tip);
            debug!(target: "reth::cli", ?tip, "Tip manually set");

            let provider = provider_factory.provider()?;

            let latest_block_number =
                provider.get_stage_checkpoint(StageId::Finish)?.map(|ch| ch.block_number);
            tokio::spawn(reth_node_events::node::handle_events(
                None,
                latest_block_number,
                events,
                db.clone(),
            ));

            // Run pipeline
            info!(target: "reth::cli", "Starting sync pipeline");
            tokio::select! {
                res = pipeline.run() => res?,
                _ = tokio::signal::ctrl_c() => {},
            }
        }

        let provider = provider_factory.provider()?;

        let total_imported_blocks = provider.tx_ref().entries::<tables::HeaderNumbers>()?;
        let total_imported_txns = provider.tx_ref().entries::<tables::TransactionHashNumbers>()?;

        if total_decoded_blocks != total_imported_blocks ||
            total_decoded_txns != total_imported_txns + total_filtered_out_dup_txns
        {
            error!(target: "reth::cli",
                total_decoded_blocks,
                total_imported_blocks,
                total_decoded_txns,
                total_filtered_out_dup_txns,
                total_imported_txns,
                "Chain was partially imported"
            );
        }

        info!(target: "reth::cli",
            total_imported_blocks,
            total_imported_txns,
            "Chain file imported"
        );

        Ok(())
    }
}
