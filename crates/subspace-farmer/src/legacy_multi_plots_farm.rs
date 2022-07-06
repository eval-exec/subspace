use crate::archiving::Archiving;
use crate::object_mappings::ObjectMappings;
use crate::rpc_client::RpcClient;
use crate::single_disk_farm::{SingleDiskFarmPieceGetter, SingleDiskSemaphore};
use crate::single_plot_farm::{PlotFactory, SinglePlotFarm, SinglePlotFarmOptions};
use crate::utils::get_plot_sizes;
use anyhow::anyhow;
use futures::stream::{FuturesUnordered, StreamExt};
use parking_lot::Mutex;
use rayon::prelude::*;
use std::path::PathBuf;
use std::sync::Arc;
use subspace_core_primitives::{PublicKey, PIECE_SIZE};
use subspace_networking::libp2p::Multiaddr;
use tokio::runtime::Handle;
use tracing::error;

/// Options for `MultiFarming` creation
pub struct Options<C> {
    pub base_directory: PathBuf,
    /// Client used for archiving subscriptions
    pub archiving_client: C,
    /// Independent client used for farming, such that it is not blocked by archiving
    pub farming_client: C,
    pub object_mappings: ObjectMappings,
    pub reward_address: PublicKey,
    pub bootstrap_nodes: Vec<Multiaddr>,
    pub listen_on: Vec<Multiaddr>,
    /// Enable DSN subscription for archiving segments.
    pub enable_dsn_archiving: bool,
    pub enable_dsn_sync: bool,
    pub enable_farming: bool,
}

/// Abstraction around having multiple `Plot`s, `Farming`s and `Plotting`s.
///
/// It is needed because of the limit of a single plot size from the consensus
/// (`pallet_subspace::MaxPlotSize`) in order to support any amount of disk space from user.
pub struct LegacyMultiPlotsFarm {
    single_plot_farms: Vec<SinglePlotFarm>,
    archiving: Option<Archiving>,
}

impl LegacyMultiPlotsFarm {
    /// Starts multiple farmers with any plot sizes which user gives
    pub async fn new<RC, PF>(
        options: Options<RC>,
        allocated_space: u64,
        max_plot_size: u64,
        plot_factory: PF,
    ) -> anyhow::Result<Self>
    where
        RC: RpcClient,
        PF: PlotFactory,
    {
        let Options {
            base_directory,
            archiving_client,
            farming_client,
            object_mappings,
            reward_address,
            bootstrap_nodes,
            listen_on,
            enable_dsn_archiving,
            enable_dsn_sync,
            enable_farming,
        } = options;
        let plot_sizes = get_plot_sizes(allocated_space, max_plot_size);

        let first_listen_on: Arc<Mutex<Option<Vec<Multiaddr>>>> = Arc::default();

        let farmer_metadata = farming_client
            .farmer_metadata()
            .await
            .map_err(|error| anyhow!(error))?;

        // Somewhat arbitrary number (we don't know if this is RAID or anything), but at least not
        // unbounded.
        let single_disk_semaphore = SingleDiskSemaphore::new(16);

        let single_plot_farms = tokio::task::spawn_blocking(move || {
            let handle = Handle::current();
            plot_sizes
                .par_iter()
                .map(|&plot_size| plot_size / PIECE_SIZE as u64)
                .enumerate()
                .map(move |(plot_index, max_piece_count)| {
                    let _guard = handle.enter();

                    let plot_directory = base_directory.join(format!("plot{plot_index}"));
                    let metadata_directory = base_directory.join(format!("plot{plot_index}"));
                    let farming_client = farming_client.clone();
                    let listen_on = listen_on.clone();
                    let bootstrap_nodes = bootstrap_nodes.clone();
                    let first_listen_on = Arc::clone(&first_listen_on);
                    let single_disk_semaphore = single_disk_semaphore.clone();

                    SinglePlotFarm::new(SinglePlotFarmOptions {
                        id: plot_index.into(),
                        plot_directory,
                        metadata_directory,
                        plot_index,
                        max_piece_count,
                        farmer_metadata,
                        farming_client,
                        plot_factory: &plot_factory,
                        listen_on,
                        bootstrap_nodes,
                        first_listen_on,
                        single_disk_semaphore,
                        enable_farming,
                        reward_address,
                        enable_dsn_archiving,
                        enable_dsn_sync,
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()
        })
        .await
        .expect("Not supposed to panic, crash if it does")?;

        // Start archiving task
        let archiving = if !enable_dsn_archiving {
            let archiving_start_fut =
                Archiving::start(farmer_metadata, object_mappings, archiving_client, {
                    let plotters = single_plot_farms
                        .iter()
                        .map(|single_plot_farm| single_plot_farm.plotter())
                        .collect::<Vec<_>>();

                    move |pieces_to_plot| {
                        if let Some(Err(error)) = plotters
                            .par_iter()
                            .map(|plotter| plotter.plot_pieces(pieces_to_plot.clone()))
                            .find_first(|result| result.is_err())
                        {
                            error!(%error, "Failed to plot pieces");
                            false
                        } else {
                            true
                        }
                    }
                });

            Some(archiving_start_fut.await?)
        } else {
            None
        };

        Ok(Self {
            single_plot_farms,
            archiving,
        })
    }

    pub fn single_plot_farms(&self) -> &[SinglePlotFarm] {
        &self.single_plot_farms
    }

    pub fn piece_getter(&self) -> SingleDiskFarmPieceGetter {
        SingleDiskFarmPieceGetter::new(
            self.single_plot_farms
                .iter()
                .map(|single_plot_farm| single_plot_farm.piece_getter())
                .collect(),
        )
    }

    /// Waits for farming and plotting completion (or errors)
    pub async fn wait(self) -> anyhow::Result<()> {
        let mut single_plot_farms = self
            .single_plot_farms
            .into_iter()
            .map(|mut single_plot_farm| async move { single_plot_farm.run().await })
            .collect::<FuturesUnordered<_>>();

        if let Some(archiving) = self.archiving {
            tokio::select! {
                res = single_plot_farms.select_next_some() => {
                    res?;
                },
                res = archiving.wait() => {
                    res?;
                },
            }
        } else {
            while let Some(result) = single_plot_farms.next().await {
                result?;
            }
        }

        Ok(())
    }
}