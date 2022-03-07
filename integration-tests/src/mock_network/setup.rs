use crate::mock_network::MockPeerManagerActor;
use actix::{Actor, Addr, Arbiter, Recipient};
use near_chain::{
    Chain, ChainGenesis, ChainStore, ChainStoreAccess, ChainStoreUpdate, DoomslugThresholdMode,
    RuntimeAdapter,
};
use near_chain_configs::GenesisConfig;
#[cfg(feature = "test_features")]
use near_client::AdversarialControls;
use near_client::{start_client, start_view_client, ClientActor, ViewClientActor};
use near_epoch_manager::EpochManager;
use near_network::test_utils::NetworkRecipient;
use near_network::types::NetworkClientMessages;
use near_primitives::network::PeerId;
use near_primitives::syncing::get_num_state_parts;
use near_primitives::types::BlockHeight;
use near_store::create_store;
use near_telemetry::TelemetryActor;
use nearcore::{get_store_path, NearConfig, NightshadeRuntime};
use std::cmp::min;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

fn setup_runtime(home_dir: &Path, config: &NearConfig) -> Arc<NightshadeRuntime> {
    let path = get_store_path(home_dir);
    let store = create_store(&path);

    Arc::new(NightshadeRuntime::with_config(
        home_dir,
        store.clone(),
        config,
        config.client_config.trie_viewer_state_size_limit,
        config.client_config.max_gas_burnt_view,
    ))
}

fn setup_mock_peer_manager_actor(
    runtime: Arc<NightshadeRuntime>,
    client_addr: Recipient<NetworkClientMessages>,
    genesis_config: &GenesisConfig,
    chain_genesis: &ChainGenesis,
    block_production_delay: Duration,
    sync_mode: SyncMode,
    network_delay: Duration,
    target_height: Option<BlockHeight>,
) -> MockPeerManagerActor {
    let chain =
        Chain::new_for_view_client(runtime, chain_genesis, DoomslugThresholdMode::NoApprovals)
            .unwrap();
    let chain_height = chain.head().unwrap().height;
    let target_height = min(target_height.unwrap_or(chain_height), chain_height);

    let peers_start_height = match sync_mode {
        SyncMode::Sync => target_height,
        SyncMode::NoSync => chain.genesis_block().header().height(),
    };
    MockPeerManagerActor::new(
        client_addr,
        genesis_config,
        chain,
        peers_start_height,
        target_height,
        block_production_delay,
        network_delay,
    )
}

pub enum SyncMode {
    /// The mock network will simulate an environment where the peers are already at the latest height,
    Sync,
    /// The peers will start at the genesis block height and new blocks are produced
    NoSync,
}

/// Setup up a mock network environment, including setting up
/// a MockPeerManagerActor and a ClientActor and a ViewClientActor
/// `client_home_dir`: home dir for the new client
/// `network_home_dir`: home dir that contains the pre-generated chain history, will be used
///                     to construct `MockPeerManagerActor`
/// `config`: config for the new client
/// `sync_mode`: whether the mock network will simulate that the node is syncing or not
/// `network_delay`: delay for getting response from the simulated network
/// `client_start_height`: start height for client
/// `target_height`: height that the simulated peers will produce blocks until. If None, will
///                  use the height from the chain head in storage
pub fn setup_mock_network(
    client_home_dir: &Path,
    network_home_dir: &Path,
    config: &NearConfig,
    sync_mode: SyncMode,
    network_delay: Duration,
    client_start_height: Option<BlockHeight>,
    target_height: Option<BlockHeight>,
) -> (Addr<MockPeerManagerActor>, Addr<ClientActor>, Addr<ViewClientActor>) {
    let client_runtime = setup_runtime(client_home_dir, &config);
    let mock_network_runtime = setup_runtime(network_home_dir, &config);

    let telemetry = TelemetryActor::new(config.telemetry_config.clone()).start();
    let chain_genesis = ChainGenesis::from(&config.genesis);

    let node_id = PeerId::new(config.network_config.public_key.clone().into());
    let network_adapter = Arc::new(NetworkRecipient::default());
    #[cfg(feature = "test_features")]
    let adv = Arc::new(std::sync::RwLock::new(AdversarialControls::default()));

    // set up client dir to be ready to process blocks from client_start_height
    if let Some(start_height) = client_start_height {
        info!(target:"mock_network", "Preparing client data dir to be able to start at the specified start height {}", start_height);
        let mut chain_store =
            ChainStore::new(client_runtime.get_store(), config.genesis.config.genesis_height);
        let mut chain_store_update = ChainStoreUpdate::new(&mut chain_store);
        let mut network_chain_store =
            ChainStore::new(mock_network_runtime.get_store(), config.genesis.config.genesis_height);

        let network_tail_height = network_chain_store.tail().unwrap();
        let network_head_height = network_chain_store.head().unwrap().height;
        info!(target:"mock_network",
              "network data chain tail height {} head height {}",
              network_tail_height,
              network_head_height,
        );
        assert!(
            start_height <= network_head_height && start_height >= network_tail_height,
            "client start height {} is not within the network chain range [{}, {}]",
            start_height,
            network_tail_height,
            network_head_height
        );
        let hash = network_chain_store.get_block_hash_by_height(start_height).unwrap();
        chain_store_update
            .copy_chain_state_as_of_block(
                &hash,
                mock_network_runtime.clone(),
                &mut network_chain_store,
            )
            .unwrap();
        chain_store_update.commit().unwrap();
        info!(target:"mock_network", "Done preparing chain state");

        let mut epoch_manager = EpochManager::new_from_genesis_config(
            client_runtime.get_store(),
            &config.genesis.config,
        )
        .unwrap();
        let mut mock_epoch_manager = EpochManager::new_from_genesis_config(
            mock_network_runtime.get_store(),
            &config.genesis.config,
        )
        .unwrap();
        let header = network_chain_store.get_block(&hash).unwrap().header();
        epoch_manager.copy_epoch_info_as_of_block(header, &mut mock_epoch_manager).unwrap();
        info!(target:"mock_network", "Done preparing epoch info");

        let next_hash = *network_chain_store.get_next_block_hash(&hash).unwrap();
        let next_block = network_chain_store.get_block(&next_hash).unwrap();
        for (shard_id, chunk_header) in next_block.chunks().iter().enumerate() {
            info!(target:"mock_network", "Preparing state for shard {}", shard_id);
            let shard_id = shard_id as u64;
            let state_root = &chunk_header.prev_state_root();
            let state_root_node =
                mock_network_runtime.get_state_root_node(shard_id, &hash, &state_root).unwrap();
            let num_parts = get_num_state_parts(state_root_node.memory_usage);
            for part_id in 0..num_parts {
                info!(target:"mock_network", "Copying part {}/{}", part_id, num_parts);
                let state_part = mock_network_runtime
                    .obtain_state_part(shard_id, &next_hash, state_root, part_id, num_parts)
                    .unwrap();
                client_runtime
                    .apply_state_part(
                        shard_id,
                        state_root,
                        part_id,
                        num_parts,
                        &state_part,
                        &mock_network_runtime.get_epoch_id_from_prev_block(&hash).unwrap(),
                    )
                    .unwrap();
            }
        }
    }

    let block_production_delay = config.client_config.min_block_production_delay;
    let (client_actor, _) = start_client(
        config.client_config.clone(),
        chain_genesis.clone(),
        client_runtime.clone(),
        node_id,
        network_adapter.clone(),
        config.validator_signer.clone(),
        telemetry,
        None,
        #[cfg(feature = "test_features")]
        adv.clone(),
    );

    let view_client = start_view_client(
        None,
        chain_genesis.clone(),
        client_runtime.clone(),
        network_adapter.clone(),
        config.client_config.clone(),
        #[cfg(feature = "test_features")]
        adv.clone(),
    );

    let arbiter = Arbiter::new();
    let client_actor1 = client_actor.clone();
    let genesis_config = config.genesis.config.clone();
    let mock_network_actor =
        MockPeerManagerActor::start_in_arbiter(&arbiter.handle(), move |_ctx| {
            setup_mock_peer_manager_actor(
                mock_network_runtime,
                client_actor1.recipient(),
                &genesis_config,
                &chain_genesis,
                block_production_delay,
                sync_mode,
                network_delay,
                target_height,
            )
        });
    network_adapter.set_recipient(mock_network_actor.clone().recipient());
    (mock_network_actor, client_actor, view_client)
}

#[cfg(test)]
mod test {
    use crate::mock_network::setup::{setup_mock_network, SyncMode};
    use actix::{Actor, System};
    use futures::{future, FutureExt};
    use near_actix_test_utils::{run_actix, spawn_interruptible};
    use near_chain_configs::Genesis;
    use near_client::GetBlock;
    use near_crypto::{InMemorySigner, KeyType};
    use near_logger_utils::init_integration_logger;
    use near_network::test_utils::{open_port, WaitOrTimeoutActor};
    use near_network::types::NetworkClientMessages;
    use near_primitives::hash::CryptoHash;
    use near_primitives::transaction::SignedTransaction;
    use near_store::test_utils::gen_account;
    use nearcore::config::GenesisExt;
    use nearcore::{load_test_config, start_with_config, NEAR_BASE};
    use rand::thread_rng;
    use std::sync::{Arc, RwLock};
    use std::time::Duration;

    // Just a test to test that the basic mocknet setup works
    // This test first starts a localnet with one validator node that generates 20 blocks
    // to generate a chain history
    // then start a mock network with this chain history and test that
    // the client in the mock network can catch up these 20 blocks
    #[test]
    fn test_mocknet_basic() {
        init_integration_logger();

        // first set up a network with only one validator and generate some blocks
        let mut genesis =
            Genesis::test(vec!["test0".parse().unwrap(), "test1".parse().unwrap()], 1);
        genesis.config.epoch_length = 10;
        let mut near_config = load_test_config("test0", open_port(), genesis.clone());
        near_config.client_config.min_num_peers = 0;

        let dir = tempfile::Builder::new().prefix("test0").tempdir().unwrap();
        let path1 = dir.path().clone();
        let last_block = Arc::new(RwLock::new(CryptoHash::default()));
        let last_block1 = last_block.clone();
        run_actix(async move {
            let nearcore::NearNode { view_client, client, .. } =
                start_with_config(path1, near_config).expect("start_with_config");

            let view_client1 = view_client.clone();
            let nonce = Arc::new(RwLock::new(10));
            WaitOrTimeoutActor::new(
                Box::new(move |_ctx| {
                    let last_block2 = last_block1.clone();
                    let nonce = nonce.clone();
                    let client1 = client.clone();
                    spawn_interruptible(view_client1.send(GetBlock::latest()).then(move |res| {
                        if let Ok(Ok(block)) = res {
                            let next_nonce = *nonce.read().unwrap();
                            if next_nonce < 100 {
                                WaitOrTimeoutActor::new(
                                    Box::new(move |_ctx| {
                                        let signer0 = InMemorySigner::from_seed(
                                            "test1".parse().unwrap(),
                                            KeyType::ED25519,
                                            "test1",
                                        );
                                        let mut rng = thread_rng();
                                        let transaction = SignedTransaction::create_account(
                                            next_nonce,
                                            "test1".parse().unwrap(),
                                            gen_account(&mut rng, b"abcdefghijklmn")
                                                .parse()
                                                .unwrap(),
                                            5 * NEAR_BASE,
                                            signer0.public_key.clone(),
                                            &signer0,
                                            block.header.hash,
                                        );
                                        spawn_interruptible(
                                            client1
                                                .send(NetworkClientMessages::Transaction {
                                                    transaction,
                                                    is_forwarded: false,
                                                    check_only: false,
                                                })
                                                .then(move |_res| future::ready(())),
                                        );
                                    }),
                                    100,
                                    30000,
                                )
                                .start();
                                *nonce.write().unwrap() = next_nonce + 1;
                            }

                            if block.header.height >= 20 {
                                *last_block2.write().unwrap() = block.header.hash;
                                System::current().stop()
                            }
                        }
                        future::ready(())
                    }));
                }),
                100,
                60000,
            )
            .start();
        });

        // start the mock network to simulate a new node "test1" to sync up
        // start the client at height 10 (end of the first epoch)
        let dir1 = tempfile::Builder::new().prefix("test1").tempdir().unwrap();
        let mut near_config1 = load_test_config("", open_port(), genesis);
        near_config1.client_config.min_num_peers = 1;
        near_config1.client_config.tracked_shards =
            (0..near_config1.genesis.config.shard_layout.num_shards()).collect();
        run_actix(async move {
            let (_mock_network, _client, view_client) = setup_mock_network(
                dir1.path().clone(),
                dir.path().clone(),
                &near_config1,
                SyncMode::Sync,
                Duration::from_millis(10),
                Some(10),
                None,
            );
            WaitOrTimeoutActor::new(
                Box::new(move |_ctx| {
                    let last_block1 = last_block.clone();
                    actix::spawn(view_client.send(GetBlock::latest()).then(move |res| {
                        if let Ok(Ok(block)) = res {
                            if block.header.height >= 20 {
                                assert_eq!(*last_block1.read().unwrap(), block.header.hash);
                                System::current().stop()
                            }
                        }
                        future::ready(())
                    }));
                }),
                100,
                60000,
            )
            .start();
        })
    }
}
