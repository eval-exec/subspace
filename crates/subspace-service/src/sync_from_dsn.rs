use crate::dsn::import_blocks::import_blocks_from_dsn;
use atomic::Atomic;
use futures::channel::mpsc;
use futures::{FutureExt, StreamExt};
use sc_client_api::{BlockBackend, BlockchainEvents};
use sc_consensus::import_queue::ImportQueueService;
use sc_network::config::SyncMode;
use sc_network::{NetworkPeers, NetworkService};
use sp_api::BlockT;
use sp_blockchain::HeaderBackend;
use sp_consensus::BlockOrigin;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use subspace_networking::Node;
use tracing::{info, trace, warn};

/// How much time to wait for new block to be imported before timing out and starting sync from DSN.
const NO_IMPORTED_BLOCKS_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Frequency with which to check whether node is online or not
const CHECK_ONLINE_STATUS_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug)]
enum NotificationReason {
    NoImportedBlocks,
    WentOnlineSubspace,
    WentOnlineSubstrate,
}

/// Create node observer that will track node state and send notifications to worker to start sync
/// from DSN.
pub(super) fn create_observer_and_worker<Block, Client>(
    network_service: Arc<NetworkService<Block, <Block as BlockT>::Hash>>,
    node: Node,
    client: Arc<Client>,
    mut import_queue_service: Box<dyn ImportQueueService<Block>>,
    sync_mode: Arc<Atomic<SyncMode>>,
) -> (
    impl Future<Output = ()> + Send + 'static,
    impl Future<Output = Result<(), sc_service::Error>> + Send + 'static,
)
where
    Block: BlockT,
    Client: HeaderBackend<Block>
        + BlockBackend<Block>
        + BlockchainEvents<Block>
        + Send
        + Sync
        + 'static,
{
    let (tx, rx) = mpsc::channel(0);
    let observer_fut = {
        let node = node.clone();
        let client = Arc::clone(&client);

        async move { create_observer(network_service.as_ref(), &node, client.as_ref(), tx).await }
    };
    let worker_fut = async move {
        create_worker(
            &node,
            client.as_ref(),
            import_queue_service.as_mut(),
            sync_mode,
            rx,
        )
        .await
    };
    (observer_fut, worker_fut)
}

async fn create_observer<Block, Client>(
    network_service: &NetworkService<Block, <Block as BlockT>::Hash>,
    node: &Node,
    client: &Client,
    notifications_sender: mpsc::Sender<NotificationReason>,
) where
    Block: BlockT,
    Client: BlockchainEvents<Block> + Send + Sync + 'static,
{
    // Separate reactive observer for Subspace networking that is not a future
    let _handler_id = node.on_num_established_peer_connections_change({
        // Assuming node is online by default
        let was_online = AtomicBool::new(false);
        let notifications_sender = notifications_sender.clone();

        Arc::new(move |&new_connections| {
            let is_online = new_connections > 0;
            let was_online = was_online.swap(is_online, Ordering::AcqRel);

            if is_online && !was_online {
                // Doesn't matter if sending failed here
                let _ = notifications_sender
                    .clone()
                    .try_send(NotificationReason::WentOnlineSubspace);
            }
        })
    });
    futures::select! {
        _ = create_imported_blocks_observer(client, notifications_sender.clone()).fuse() => {
            // Runs indefinitely
        }
        _ = create_substrate_network_observer(network_service, notifications_sender).fuse() => {
            // Runs indefinitely
        }
        // TODO: More sources
    }
}

async fn create_imported_blocks_observer<Block, Client>(
    client: &Client,
    mut notifications_sender: mpsc::Sender<NotificationReason>,
) where
    Block: BlockT,
    Client: BlockchainEvents<Block> + Send + Sync + 'static,
{
    let mut import_notification_stream = client.every_import_notification_stream();
    loop {
        match tokio::time::timeout(
            NO_IMPORTED_BLOCKS_TIMEOUT,
            import_notification_stream.next(),
        )
        .await
        {
            Ok(Some(_notification)) => {
                // Do nothing
            }
            Ok(None) => {
                // No more notifications
                return;
            }
            Err(_timeout) => {
                if let Err(error) =
                    notifications_sender.try_send(NotificationReason::NoImportedBlocks)
                {
                    if error.is_disconnected() {
                        // Receiving side was closed
                        return;
                    }
                }
            }
        }
    }
}

async fn create_substrate_network_observer<Block>(
    network_service: &NetworkService<Block, <Block as BlockT>::Hash>,
    mut notifications_sender: mpsc::Sender<NotificationReason>,
) where
    Block: BlockT,
{
    // Assuming node is online by default
    let mut was_online = false;

    loop {
        tokio::time::sleep(CHECK_ONLINE_STATUS_INTERVAL).await;

        let is_online = network_service.sync_num_connected() > 0;

        if is_online && !was_online {
            if let Err(error) =
                notifications_sender.try_send(NotificationReason::WentOnlineSubstrate)
            {
                if error.is_disconnected() {
                    // Receiving side was closed
                    return;
                }
            }
        }

        was_online = is_online;
    }
}

async fn create_worker<Block, IQS, Client>(
    node: &Node,
    client: &Client,
    import_queue_service: &mut IQS,
    sync_mode: Arc<Atomic<SyncMode>>,
    mut notifications: mpsc::Receiver<NotificationReason>,
) -> Result<(), sc_service::Error>
where
    Block: BlockT,
    Client: HeaderBackend<Block> + BlockBackend<Block> + Send + Sync + 'static,
    IQS: ImportQueueService<Block> + ?Sized,
{
    while let Some(reason) = notifications.next().await {
        // TODO: Remove this condition once we switch to Subspace networking for everything
        if matches!(reason, NotificationReason::WentOnlineSubspace) {
            trace!("Ignoring Subspace networking for DSN sync for now");
            continue;
        }

        let prev_sync_mode = sync_mode.swap(SyncMode::Paused, Ordering::SeqCst);

        while notifications.try_next().is_ok() {
            // Just drain extra messages if there are any
        }

        info!(?reason, "Received notification to sync from DSN");
        // TODO: Maybe handle failed block imports, additional helpful logging
        if let Err(error) = import_blocks_from_dsn(
            node,
            client,
            import_queue_service,
            BlockOrigin::NetworkBroadcast,
            false,
        )
        .await
        {
            warn!(%error, "Error when syncing blocks from DSN");
        }

        sync_mode.store(prev_sync_mode, Ordering::Release);
    }

    Ok(())
}
