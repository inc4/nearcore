//! Client is responsible for tracking the chain and related pieces of infrastructure.
//! Block production is done in done in this actor as well (at the moment).

use std::collections::HashMap;
use std::ops::Sub;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use actix::{
    Actor, ActorFuture, Addr, AsyncContext, Context, ContextFutureSpawner, Handler, Recipient,
    WrapFuture,
};
use chrono::{DateTime, Utc};
use futures::Future;
use log::{debug, error, info, warn};

use near_chain::{
    Block, BlockApproval, BlockHeader, BlockStatus, Chain, ChainGenesis, ErrorKind, Provenance,
    RuntimeAdapter,
};
use near_chunks::ShardsManager;
use near_network::types::{
    AnnounceAccount, AnnounceAccountRoute, NetworkInfo, PeerId, ReasonForBan,
};
use near_network::{
    NetworkClientMessages, NetworkClientResponses, NetworkRequests, NetworkResponses,
};
use near_primitives::crypto::signature::{verify, Signature};
use near_primitives::hash::{hash, CryptoHash};
use near_primitives::sharding::ShardChunkHeader;
use near_primitives::transaction::ReceiptTransaction;
use near_primitives::types::{AccountId, BlockIndex, ShardId};
use near_primitives::unwrap_or_return;
use near_store::Store;
use near_telemetry::TelemetryActor;

use crate::info::InfoHelper;
use crate::sync::{most_weight_peer, BlockSync, HeaderSync, StateSync, StateSyncResult};
use crate::types::{
    BlockProducer, ClientConfig, Error, ShardSyncStatus, Status, StatusSyncInfo, SyncStatus,
};
use crate::{sync, StatusResponse};

/// Macro to either return value if the result is Ok, or exit function logging error.
macro_rules! unwrap_or_return(($obj: expr, $ret: expr) => (match $obj {
    Ok(value) => value,
    Err(err) => {
        error!(target: "client", "Error: {:?}", err);
        return $ret;
    }
}));

pub struct ClientActor {
    config: ClientConfig,
    sync_status: SyncStatus,
    chain: Chain,
    runtime_adapter: Arc<dyn RuntimeAdapter>,
    shards_mgr: ShardsManager,
    /// A mapping from a block for which a state sync is underway for the next epoch, and the object
    /// storing the current status of the state sync
    catchup_state_syncs: HashMap<CryptoHash, (StateSync, HashMap<u64, ShardSyncStatus>)>,
    block_producer: Option<BlockProducer>,
    network_actor: Recipient<NetworkRequests>,
    network_info: NetworkInfo,
    /// Identity that represents this Client at the network level.
    /// It is used as part of the messages that identify this client.
    node_id: PeerId,
    /// Set of approvals for the next block.
    approvals: HashMap<usize, Signature>,
    /// Timestamp when last block was received / processed. Used to timeout block production.
    last_block_processed: Instant,
    /// Keeps track of syncing headers.
    header_sync: HeaderSync,
    /// Keeps track of syncing block.
    block_sync: BlockSync,
    /// Keeps track of syncing state.
    state_sync: StateSync,
    /// Last time we announced our accounts as validators.
    last_val_announce_height: Option<BlockIndex>,
    /// Info helper.
    info_helper: InfoHelper,
}

fn wait_until_genesis(genesis_time: &DateTime<Utc>) {
    let now = Utc::now();
    //get chrono::Duration::num_seconds() by deducting genesis_time from now
    let chrono_seconds = genesis_time.signed_duration_since(now).num_seconds();
    //check if number of seconds in chrono::Duration larger than zero
    if chrono_seconds > 0 {
        info!(target: "chain", "Waiting until genesis: {}", chrono_seconds);
        let seconds = Duration::from_secs(chrono_seconds as u64);
        thread::sleep(seconds);
    }
}

impl ClientActor {
    pub fn new(
        config: ClientConfig,
        store: Arc<Store>,
        chain_genesis: ChainGenesis,
        runtime_adapter: Arc<dyn RuntimeAdapter>,
        node_id: PeerId,
        network_actor: Recipient<NetworkRequests>,
        block_producer: Option<BlockProducer>,
        telemtetry_actor: Addr<TelemetryActor>,
    ) -> Result<Self, Error> {
        wait_until_genesis(&chain_genesis.time);
        let chain = Chain::new(store.clone(), runtime_adapter.clone(), chain_genesis)?;
        let shards_mgr = ShardsManager::new(
            block_producer.as_ref().map(|x| x.account_id.clone()),
            runtime_adapter.clone(),
            network_actor.clone(),
            store.clone(),
        );
        let sync_status = SyncStatus::AwaitingPeers;
        let header_sync = HeaderSync::new(network_actor.clone());
        let block_sync = BlockSync::new(network_actor.clone(), config.block_fetch_horizon);
        let state_sync = StateSync::new(network_actor.clone());
        if let Some(bp) = &block_producer {
            info!(target: "client", "Starting validator node: {}", bp.account_id);
        }

        let info_helper = InfoHelper::new(telemtetry_actor, block_producer.clone());

        Ok(ClientActor {
            config,
            sync_status,
            chain,
            runtime_adapter,
            shards_mgr,
            catchup_state_syncs: HashMap::new(),
            network_actor,
            node_id,
            block_producer,
            network_info: NetworkInfo {
                num_active_peers: 0,
                peer_max_count: 0,
                most_weight_peers: vec![],
                received_bytes_per_sec: 0,
                sent_bytes_per_sec: 0,
                routes: None,
            },
            approvals: HashMap::default(),
            last_block_processed: Instant::now(),
            header_sync,
            block_sync,
            state_sync,
            last_val_announce_height: None,
            info_helper,
        })
    }

    fn check_signature_account_announce(
        &self,
        announce_account: &AnnounceAccount,
    ) -> Result<(), ReasonForBan> {
        debug!(target: "client", "Received account announce: {:?}", announce_account);
        // Check header is correct.
        let header_hash = announce_account.header_hash();
        let header = announce_account.header();

        // hash must match announcement hash ...
        if header_hash != header.hash {
            return Err(ReasonForBan::InvalidHash);
        }

        // ... and signature should be valid.
        if !self.runtime_adapter.verify_validator_signature(
            &announce_account.epoch,
            &announce_account.account_id,
            header_hash.as_ref(),
            &header.signature,
        ) {
            return Err(ReasonForBan::InvalidSignature);
        }

        // Check intermediates hops are correct.
        // Skip first element (header)
        announce_account
            .route
            .iter()
            .skip(1)
            .fold(Ok(header_hash), |previous_hash, hop| {
                // Folding function will return None if at least one hop checking fail,
                // otherwise it will return hash from last hop.
                if let Ok(previous_hash) = previous_hash {
                    let AnnounceAccountRoute { peer_id, hash: current_hash, signature } = hop;

                    let real_current_hash =
                        &hash([previous_hash.as_ref(), peer_id.as_ref()].concat().as_slice());

                    if real_current_hash != current_hash {
                        return Err(ReasonForBan::InvalidHash);
                    }

                    if verify(current_hash.as_ref(), signature, &peer_id.public_key()) {
                        Ok(previous_hash)
                    } else {
                        Err(ReasonForBan::InvalidSignature)
                    }
                } else {
                    previous_hash
                }
            })
            .map(|_hash| ())
    }
}

impl Actor for ClientActor {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        // Start syncing job.
        self.start_sync(ctx);

        // Start fetching information from network.
        self.fetch_network_info(ctx);

        // Start periodic logging of current state of the client.
        self.log_summary(ctx);
    }
}

impl Handler<NetworkClientMessages> for ClientActor {
    type Result = NetworkClientResponses;

    fn handle(&mut self, msg: NetworkClientMessages, ctx: &mut Context<Self>) -> Self::Result {
        match msg {
            NetworkClientMessages::Transaction(tx) => {
                let runtime_adapter = self.runtime_adapter.clone();
                let shard_id = runtime_adapter.account_id_to_shard_id(&tx.body.get_originator());
                if let Ok(chunk_extra) = self.chain.get_latest_chunk_extra(shard_id) {
                    match self.runtime_adapter.validate_tx(shard_id, chunk_extra.state_root, tx) {
                        Ok(valid_transaction) => {
                            debug!(
                                "MOO recording a transaction. I'm {:?}, {}",
                                self.block_producer.clone().unwrap().account_id,
                                shard_id
                            );
                            self.shards_mgr.insert_transaction(shard_id, valid_transaction);
                            NetworkClientResponses::ValidTx
                        }
                        Err(err) => NetworkClientResponses::InvalidTx(err),
                    }
                } else {
                    NetworkClientResponses::NoResponse
                }
            }
            NetworkClientMessages::BlockHeader(header, peer_id) => {
                self.receive_header(header, peer_id)
            }
            NetworkClientMessages::Block(block, peer_id, was_requested) => {
                self.receive_block(ctx, block, peer_id, was_requested)
            }
            NetworkClientMessages::BlockRequest(hash) => {
                if let Ok(block) = self.chain.get_block(&hash) {
                    NetworkClientResponses::Block(block.clone())
                } else {
                    NetworkClientResponses::NoResponse
                }
            }
            NetworkClientMessages::BlockHeadersRequest(hashes) => {
                if let Ok(headers) = self.retrieve_headers(hashes) {
                    NetworkClientResponses::BlockHeaders(headers)
                } else {
                    NetworkClientResponses::NoResponse
                }
            }
            NetworkClientMessages::GetChainInfo => match self.chain.head() {
                Ok(head) => NetworkClientResponses::ChainInfo {
                    genesis: self.chain.genesis().hash(),
                    height: head.height,
                    total_weight: head.total_weight,
                },
                Err(err) => {
                    error!(target: "client", "{}", err);
                    NetworkClientResponses::NoResponse
                }
            },
            NetworkClientMessages::BlockHeaders(headers, peer_id) => {
                if self.receive_headers(headers, peer_id) {
                    NetworkClientResponses::NoResponse
                } else {
                    warn!(target: "client", "Banning node for sending invalid block headers");
                    NetworkClientResponses::Ban { ban_reason: ReasonForBan::BadBlockHeader }
                }
            }
            NetworkClientMessages::BlockApproval(account_id, hash, signature) => {
                if self.collect_block_approval(&account_id, &hash, &signature) {
                    NetworkClientResponses::NoResponse
                } else {
                    warn!(target: "client", "Banning node for sending invalid block approval: {} {} {}", account_id, hash, signature);
                    NetworkClientResponses::Ban { ban_reason: ReasonForBan::BadBlockApproval }
                }
            }
            NetworkClientMessages::StateRequest(shard_id, hash) => {
                debug!(
                    "MOO received state request for hash {:?}, I'm {:?}",
                    hash,
                    self.block_producer.clone().map(|x| x.account_id)
                );
                if let Ok((payload, receipts)) = self.state_request(shard_id, hash) {
                    return NetworkClientResponses::StateResponse {
                        shard_id,
                        hash,
                        payload,
                        receipts,
                    };
                }
                NetworkClientResponses::NoResponse
            }
            NetworkClientMessages::StateResponse(shard_id, hash, payload, receipts) => {
                debug!(
                    "MOO received state response for hash {:?}, I'm {:?}",
                    hash,
                    self.block_producer.clone().map(|x| x.account_id)
                );
                // Populate the hashmaps with shard statuses that might be interested in this state
                //     (naturally, the plural of statuses is statuseses)
                let mut shard_statuseses = vec![];

                // ... It could be that the state was requested by the state sync
                if let SyncStatus::StateSync(sync_hash, shard_statuses) = &mut self.sync_status {
                    if hash == *sync_hash {
                        shard_statuseses.push(shard_statuses);
                    }
                }

                // ... Or one of the catchups
                for (sync_hash, state_sync_info) in self.chain.store().iterate_state_sync_infos() {
                    if hash == sync_hash {
                        assert_eq!(sync_hash, state_sync_info.epoch_tail_hash);
                        if let Some((_, shard_statuses)) =
                            self.catchup_state_syncs.get_mut(&sync_hash)
                        {
                            debug!("MOO there's a status to update for {:?}", hash);
                            shard_statuseses.push(shard_statuses);
                        }
                        // We should not be requesting the same state twice.
                        break;
                    }
                }

                if !shard_statuseses.is_empty() {
                    match self.chain.set_shard_state(shard_id, hash, payload, receipts) {
                        Ok(()) => {
                            for shard_statuses in shard_statuseses {
                                shard_statuses.insert(shard_id, ShardSyncStatus::StateDone);
                            }
                        }
                        Err(err) => {
                            for shard_statuses in shard_statuseses {
                                shard_statuses.insert(
                                    shard_id,
                                    ShardSyncStatus::Error(format!(
                                        "Failed to set state for {} @ {}: {}",
                                        shard_id, hash, err
                                    )),
                                );
                            }
                        }
                    }
                }

                NetworkClientResponses::NoResponse
            }
            NetworkClientMessages::ChunkPartRequest(part_request_msg, peer_id) => {
                let _ = self.shards_mgr.process_chunk_part_request(part_request_msg, peer_id);
                NetworkClientResponses::NoResponse
            }
            NetworkClientMessages::ChunkOnePartRequest(part_request_msg, peer_id) => {
                let _ = self.shards_mgr.process_chunk_one_part_request(part_request_msg, peer_id);
                NetworkClientResponses::NoResponse
            }
            NetworkClientMessages::ChunkPart(part_msg) => {
                if let Ok(Some(height)) = self.shards_mgr.process_chunk_part(part_msg) {
                    self.process_blocks_with_missing_chunks(ctx, height);
                }
                NetworkClientResponses::NoResponse
            }
            NetworkClientMessages::ChunkOnePart(one_part_msg) => {
                let prev_block_hash = one_part_msg.header.prev_block_hash;
                let height = one_part_msg.header.height_created;
                if let Ok(ret) = self.shards_mgr.process_chunk_one_part(one_part_msg.clone()) {
                    if ret {
                        // If the chunk builds on top of the current head, get all the remaining parts
                        // TODO: if the bp receives the chunk before they receive the block, they will
                        //     not collect the parts currently. It will result in chunk not included
                        //     in the next block.
                        if self.block_producer.as_ref().map_or_else(
                            || false,
                            |bp| {
                                self.shards_mgr.cares_about_shard_this_or_next_epoch(
                                    &bp.account_id,
                                    prev_block_hash,
                                    one_part_msg.shard_id,
                                )
                            },
                        ) && self
                            .chain
                            .head()
                            .map(|head| head.last_block_hash == one_part_msg.header.prev_block_hash)
                            .unwrap_or(false)
                        {
                            self.shards_mgr.request_chunks(vec![one_part_msg.header]);
                        } else {
                            // We are getting here either because we don't care about the shard, or
                            //    because we see the one part before we see the block.
                            // In the latter case we will request parts once the block is received
                        }
                        self.process_blocks_with_missing_chunks(ctx, height);
                    }
                }
                NetworkClientResponses::NoResponse
            }
            NetworkClientMessages::AnnounceAccount(announce_account) => {
                match self.check_signature_account_announce(&announce_account) {
                    Ok(_) => {
                        actix::spawn(
                            self.network_actor
                                .send(NetworkRequests::AnnounceAccount(announce_account))
                                .map_err(|e| error!(target: "client", "{}", e))
                                .map(|_| ()),
                        );
                        NetworkClientResponses::NoResponse
                    }
                    Err(ban_reason) => NetworkClientResponses::Ban { ban_reason },
                }
            }
        }
    }
}

impl Handler<Status> for ClientActor {
    type Result = Result<StatusResponse, String>;

    fn handle(&mut self, _: Status, _: &mut Context<Self>) -> Self::Result {
        let head = self.chain.head().map_err(|err| err.to_string())?;
        let prev_header =
            self.chain.get_block_header(&head.last_block_hash).map_err(|err| err.to_string())?;
        let latest_block_time = prev_header.timestamp.clone();
        let validators = self
            .runtime_adapter
            .get_epoch_block_proposers(head.epoch_hash)
            .map_err(|err| err.to_string())?;
        Ok(StatusResponse {
            version: self.config.version.clone(),
            chain_id: self.config.chain_id.clone(),
            rpc_addr: self.config.rpc_addr.clone(),
            validators,
            sync_info: StatusSyncInfo {
                latest_block_hash: head.last_block_hash,
                latest_block_height: head.height,
                latest_state_root: prev_header.prev_state_root.clone(),
                latest_block_time,
                syncing: self.sync_status.is_syncing(),
            },
        })
    }
}

impl ClientActor {
    /// Gets called when block got accepted.
    /// Send updates over network, update tx pool and notify ourselves if it's time to produce next block.
    fn on_block_accepted(
        &mut self,
        ctx: &mut Context<ClientActor>,
        block_hash: CryptoHash,
        status: BlockStatus,
        provenance: Provenance,
    ) {
        let block = match self.chain.get_block(&block_hash) {
            Ok(block) => block.clone(),
            Err(err) => {
                error!(target: "client", "Failed to find block {} that was just accepted: {}", block_hash, err);
                return;
            }
        };

        // Update when last block was processed.
        self.last_block_processed = Instant::now();

        // Count blocks and transactions processed both in SYNC and regular modes.
        self.info_helper.block_processed(block.transactions.len() as u64);

        // Process orphaned chunk_one_parts
        if self.shards_mgr.process_orphaned_one_parts(block_hash) {
            // process_orphaned_one_parts returns true if some of the one parts were not known before
            //    generally in this case we would check blocks with missing chunks, but since the
            //    block that unbloked the one parts was just processed, any block that would actually
            //    depend on those one parts would have been an orphan, not a block with missing chunks,
            //    so no need to process anything here
            error!("MOO unlocked some orphaned one parts");
            self.process_blocks_with_missing_chunks(ctx, block.header.height + 1);
        }

        if provenance != Provenance::SYNC {
            // If we produced the block, then we want to broadcast it.
            // If received the block from another node then broadcast "header first" to minimise network traffic.
            if provenance == Provenance::PRODUCED {
                let _ = self.network_actor.do_send(NetworkRequests::Block { block: block.clone() });
            } else {
                let approval = self.get_block_approval(&block);
                let _ = self.network_actor.do_send(NetworkRequests::BlockHeaderAnnounce {
                    header: block.header.clone(),
                    approval,
                });
            }

            // If this is block producing node and next block is produced by us, schedule to produce a block after a delay.
            self.handle_scheduling_block_production(
                ctx,
                block.hash(),
                block.header.height,
                block.header.height,
            );
        }

        if let Some(bp) = self.block_producer.clone() {
            // Reconcile the txpool against the new block *after* we have broadcast it too our peers.
            // This may be slow and we do not want to delay block propagation.
            match status {
                BlockStatus::Next => {
                    // If this block immediately follows the current tip, remove transactions
                    //    from the txpool
                    self.remove_transactions_for_block(bp.account_id.clone(), &block);
                }
                BlockStatus::Fork => {
                    // If it's a fork, no need to reconcile transactions or produce chunks
                    return;
                }
                BlockStatus::Reorg(prev_head) => {
                    // If a reorg happened, reintroduce transactions from the previous chain and
                    //    remove transactions from the new chain
                    let mut reintroduce_head =
                        self.chain.get_block_header(&prev_head).unwrap().clone();
                    let mut remove_head = block.header.clone();
                    assert_ne!(remove_head.hash(), reintroduce_head.hash());

                    let mut to_remove = vec![];
                    let mut to_reintroduce = vec![];

                    while remove_head.hash() != reintroduce_head.hash() {
                        while remove_head.height > reintroduce_head.height {
                            to_remove.push(remove_head.hash());
                            remove_head = self
                                .chain
                                .get_block_header(&remove_head.prev_hash)
                                .unwrap()
                                .clone();
                        }
                        while reintroduce_head.height > remove_head.height
                            || reintroduce_head.height == remove_head.height
                                && reintroduce_head.hash() != remove_head.hash()
                        {
                            to_reintroduce.push(reintroduce_head.hash());
                            reintroduce_head = self
                                .chain
                                .get_block_header(&reintroduce_head.prev_hash)
                                .unwrap()
                                .clone();
                        }
                    }

                    for to_reintroduce_hash in to_reintroduce {
                        let block = self.chain.get_block(&to_reintroduce_hash).unwrap().clone();
                        self.reintroduce_transactions_for_block(bp.account_id.clone(), &block);
                    }

                    for to_remove_hash in to_remove {
                        let block = self.chain.get_block(&to_remove_hash).unwrap().clone();
                        self.remove_transactions_for_block(bp.account_id.clone(), &block);
                    }
                }
            };

            if provenance != Provenance::SYNC {
                // Produce new chunks
                for shard_id in 0..self.runtime_adapter.num_shards() {
                    let next_epoch_hash =
                        self.runtime_adapter.get_epoch_hash(block.header.hash()).unwrap();
                    let chunk_proposer = self
                        .runtime_adapter
                        .get_chunk_proposer(next_epoch_hash, block.header.height + 1, shard_id)
                        .unwrap();

                    if chunk_proposer == *bp.account_id {
                        if let Err(err) = self.produce_chunk(
                            ctx,
                            block.hash(),
                            next_epoch_hash,
                            block.chunks[shard_id as usize].clone(),
                            block.header.height + 1,
                            shard_id,
                        ) {
                            error!(target: "client", "Error producing chunk {:?}", err);
                        }
                    }
                }
            }
        }

        self.check_send_announce_account(block.header.epoch_hash);
    }

    fn remove_transactions_for_block(&mut self, me: AccountId, block: &Block) {
        for (shard_id, chunk_header) in block.chunks.iter().enumerate() {
            let shard_id = shard_id as ShardId;
            if block.header.height == chunk_header.height_included {
                if self.shards_mgr.cares_about_shard_this_or_next_epoch(
                    &me,
                    block.header.prev_hash,
                    shard_id,
                ) {
                    self.shards_mgr.remove_transactions(
                        shard_id,
                        // By now the chunk must be in store, otherwise the block would have been orphaned
                        &self.chain.get_chunk(&chunk_header).unwrap().transactions,
                    );
                }
            }
        }
    }

    fn reintroduce_transactions_for_block(&mut self, me: AccountId, block: &Block) {
        for (shard_id, chunk_header) in block.chunks.iter().enumerate() {
            let shard_id = shard_id as ShardId;
            if block.header.height == chunk_header.height_included {
                if self.shards_mgr.cares_about_shard_this_or_next_epoch(
                    &me,
                    block.header.prev_hash,
                    shard_id,
                ) {
                    self.shards_mgr.reintroduce_transactions(
                        shard_id,
                        // By now the chunk must be in store, otherwise the block would have been orphaned
                        &self.chain.get_chunk(&chunk_header).unwrap().transactions,
                    );
                }
            }
        }
    }

    /// Check if client Account Id should be sent and send it.
    /// Account Id is sent when is not current a validator but are becoming a validator soon.
    fn check_send_announce_account(&mut self, epoch_hash: CryptoHash) {
        // Announce AccountId if client is becoming a validator soon.

        // First check that we currently have an AccountId
        if self.block_producer.is_none() {
            // There is no account id associated with this client
            return;
        }

        let block_producer = self.block_producer.as_ref().unwrap();
        debug!(target: "client", "Check announce account for {}", block_producer.account_id);

        // TODO MOO XXX: only announce if become a validator soon

        if let Ok(epoch_block) = self.chain.get_block(&epoch_hash) {
            let epoch_height = epoch_block.header.height;

            if let Some(last_val_announce_height) = self.last_val_announce_height {
                if last_val_announce_height >= epoch_height {
                    // This announcement was already done!
                    return;
                }
            }

            // Check client is part of the futures validators
            if let Ok(validators) = self.runtime_adapter.get_epoch_block_proposers(epoch_hash) {
                // TODO(MarX): Use HashSet in validator manager to do fast searching.
                if validators.iter().any(|account_id| (account_id == &block_producer.account_id)) {
                    debug!(target: "client", "Sending announce account for {}", block_producer.account_id);
                    self.last_val_announce_height = Some(epoch_height);
                    let (hash, signature) = self.sign_announce_account(epoch_hash).unwrap();

                    actix::spawn(
                        self.network_actor
                            .send(NetworkRequests::AnnounceAccount(AnnounceAccount::new(
                                block_producer.account_id.clone(),
                                epoch_hash,
                                self.node_id,
                                hash,
                                signature,
                            )))
                            .map_err(|e| error!(target: "client", "{:?}", e))
                            .map(|_| ()),
                    );
                }
            }
        }
    }

    fn sign_announce_account(&self, epoch_hash: CryptoHash) -> Result<(CryptoHash, Signature), ()> {
        if let Some(block_producer) = self.block_producer.as_ref() {
            let hash = AnnounceAccount::build_header_hash(
                &block_producer.account_id,
                &self.node_id,
                epoch_hash,
            );
            let signature = block_producer.signer.sign(hash.as_ref());
            Ok((hash, signature))
        } else {
            Err(())
        }
    }

    fn get_block_proposer(
        &self,
        epoch_hash: CryptoHash,
        height: BlockIndex,
    ) -> Result<AccountId, Error> {
        self.runtime_adapter
            .get_block_proposer(epoch_hash, height)
            .map_err(|err| Error::Other(err.to_string()))
    }

    fn get_epoch_block_proposers(&self, epoch_hash: CryptoHash) -> Result<Vec<AccountId>, Error> {
        self.runtime_adapter
            .get_epoch_block_proposers(epoch_hash)
            .map_err(|err| Error::Other(err.to_string()))
    }

    /// Create approval for given block or return none if not a block producer.
    fn get_block_approval(&mut self, block: &Block) -> Option<BlockApproval> {
        let mut epoch_hash = self.runtime_adapter.get_epoch_hash(block.hash()).ok()?;
        let next_block_producer_account =
            self.get_block_proposer(epoch_hash, block.header.height + 1);
        if let (Some(block_producer), Ok(next_block_producer_account)) =
            (&self.block_producer, &next_block_producer_account)
        {
            if &block_producer.account_id != next_block_producer_account {
                epoch_hash = block.header.epoch_hash;
                if let Ok(validators) = self.runtime_adapter.get_epoch_block_proposers(epoch_hash) {
                    if validators.contains(&block_producer.account_id) {
                        return Some(BlockApproval::new(
                            block.hash(),
                            &*block_producer.signer,
                            next_block_producer_account.clone(),
                        ));
                    }
                }
            }
        }
        None
    }

    /// Checks if we are block producer and if we are next block producer schedules calling `produce_block`.
    /// If we are not next block producer, schedule to check timeout.
    fn handle_scheduling_block_production(
        &mut self,
        ctx: &mut Context<ClientActor>,
        block_hash: CryptoHash,
        last_height: BlockIndex,
        check_height: BlockIndex,
    ) {
        let epoch_hash = unwrap_or_return!(self.runtime_adapter.get_epoch_hash(block_hash), ());
        let next_block_producer_account =
            unwrap_or_return!(self.get_block_proposer(epoch_hash, check_height + 1), ());
        if let Some(block_producer) = &self.block_producer {
            if block_producer.account_id.clone() == next_block_producer_account {
                ctx.run_later(self.config.min_block_production_delay, move |act, ctx| {
                    act.produce_block(ctx, block_hash, last_height, check_height + 1);
                });
            } else {
                // Otherwise, schedule timeout to check if the next block was produced.
                ctx.run_later(self.config.max_block_production_delay, move |act, ctx| {
                    act.check_block_timeout(ctx, last_height, check_height);
                });
            }
        }
    }

    /// Checks if next block was produced within timeout, if not check if we should produce next block.
    /// `last_height` is the height of the `head` at the point of scheduling,
    /// `check_height` is the height at which to call `handle_scheduling_block_production` to skip non received blocks.
    /// TODO: should we send approvals for `last_height` block to next block producer?
    fn check_block_timeout(
        &mut self,
        ctx: &mut Context<ClientActor>,
        last_height: BlockIndex,
        check_height: BlockIndex,
    ) {
        let head = unwrap_or_return!(self.chain.head(), ());
        // If height changed since we scheduled this, exit.
        if head.height != last_height {
            return;
        }
        debug!(target: "client", "Timeout for {}, current head {}, suggesting to skip", last_height, head.height);
        // Update how long ago last block arrived to reset block production timer.
        self.last_block_processed = Instant::now();
        self.handle_scheduling_block_production(
            ctx,
            head.last_block_hash,
            last_height,
            check_height + 1,
        );
    }

    /// Produce block if we are block producer for given block. If error happens, retry.
    fn produce_block(
        &mut self,
        ctx: &mut Context<ClientActor>,
        block_hash: CryptoHash,
        last_height: BlockIndex,
        next_height: BlockIndex,
    ) {
        if let Err(err) = self.produce_block_err(ctx, last_height, next_height) {
            error!(target: "client", "Block production failed: {:?}", err);
            self.handle_scheduling_block_production(ctx, block_hash, last_height, next_height - 1);
        }
    }

    fn produce_chunk(
        &mut self,
        _ctx: &mut Context<ClientActor>, // TODO: remove?
        prev_block_hash: CryptoHash,
        epoch_hash: CryptoHash,
        last_header: ShardChunkHeader,
        next_height: BlockIndex,
        shard_id: ShardId,
    ) -> Result<(), Error> {
        let block_producer = self.block_producer.as_ref().ok_or_else(|| {
            Error::ChunkProducer("Called without block producer info.".to_string())
        })?;

        let chunk_proposer = self
            .runtime_adapter
            .get_chunk_proposer(epoch_hash, next_height, shard_id)
            .map_err(|err| Error::Other(err.to_string()))
            .unwrap();
        if block_producer.account_id != chunk_proposer {
            debug!(target: "client", "Not producing chunk for shard {}: chain at {}, not block producer for next block. Me: {}, proposer: {}", shard_id, next_height, block_producer.account_id, chunk_proposer);
            return Ok(());
        }

        debug!(
            target: "client",
            "Producing chunk at height {} for shard {}, I'm {}",
            next_height,
            shard_id,
            block_producer.account_id
        );

        let chunk_extra = self
            .chain
            .get_latest_chunk_extra(shard_id)
            .map_err(|err| Error::ChunkProducer(format!("No chunk extra available: {}", err)))?
            .clone();

        let transactions =
            self.shards_mgr.prepare_transactions(shard_id, self.config.block_expected_weight)?;
        info!("Creating a chunk with {} transactions for shard {}", transactions.len(), shard_id);

        let mut receipts = vec![];
        let mut receipts_block_hash = prev_block_hash.clone();
        loop {
            let block_header = self.chain.get_block_header(&receipts_block_hash)?;

            assert!(
                block_header.height < last_header.height_created
                    || block_header.height >= last_header.height_included
            );

            if block_header.height == last_header.height_included {
                if let Ok(cur_receipts) = self.chain.get_receipts(&receipts_block_hash, shard_id) {
                    receipts.extend_from_slice(cur_receipts);
                }
                break;
            } else {
                receipts_block_hash = block_header.prev_hash.clone();
            }
        }

        let encoded_chunk = self
            .shards_mgr
            .create_encoded_shard_chunk(
                prev_block_hash,
                chunk_extra.state_root,
                next_height,
                shard_id,
                chunk_extra.gas_used,
                chunk_extra.gas_limit,
                chunk_extra.validator_proposals.clone(),
                &transactions,
                &receipts,
                block_producer.signer.clone(),
            )
            .map_err(|_e| {
                Error::ChunkProducer("Can't create encoded chunk, serialization error.".to_string())
            })?;

        debug!(
            target: "client",
            "Produced chunk at height {} for shard {} with {} txs and {} receipts, I'm {}, chunk_hash: {}",
            next_height,
            shard_id,
            transactions.len(),
            receipts.len(),
            block_producer.account_id,
            encoded_chunk.chunk_hash().0,
        );

        self.shards_mgr.distribute_encoded_chunk(encoded_chunk, receipts);

        Ok(())
    }

    /// Produce block if we are block producer for given `next_height` index.
    /// Can return error, should be called with `produce_block` to handle errors and reschedule.
    fn produce_block_err(
        &mut self,
        ctx: &mut Context<ClientActor>,
        last_height: BlockIndex,
        next_height: BlockIndex,
    ) -> Result<(), Error> {
        let block_producer = self.block_producer.as_ref().ok_or_else(|| {
            Error::BlockProducer("Called without block producer info.".to_string())
        })?;
        let head = self.chain.head()?;
        assert_eq!(
            head.epoch_hash,
            self.runtime_adapter.get_epoch_hash(head.prev_block_hash).unwrap()
        );
        // If last height changed, this process should stop as we spun up another one.
        if head.height != last_height {
            return Ok(());
        }

        // Check that we are were called at the block that we are producer for.
        let next_block_proposer = self.get_block_proposer(
            self.runtime_adapter.get_epoch_hash(head.last_block_hash).unwrap(),
            next_height,
        )?;
        if block_producer.account_id != next_block_proposer {
            info!(target: "client", "Produce block: chain at {}, not block producer for next block.", next_height);
            return Ok(());
        }
        let prev = self.chain.get_block_header(&head.last_block_hash)?;
        let prev_hash = prev.hash();
        let prev_prev_hash = prev.prev_hash;

        if self
            .runtime_adapter
            .is_epoch_start(head.last_block_hash, next_height)
            .map_err(|err| ErrorKind::Other(err.to_string()))?
        {
            if !self.chain.prev_block_is_caught_up(&prev_prev_hash, &prev_hash)? {
                // The previous block is not caught up for the next epoch relative to the previous
                // block, which is the current epoch for this block, so this block cannot be applied
                // at all yet, needs to be orphaned
                return Err(ErrorKind::Orphan.into());
            }
        }

        // Wait until we have all approvals or timeouts per max block production delay.
        let validators = self
            .runtime_adapter
            .get_epoch_block_proposers(head.epoch_hash)
            .map_err(|err| Error::Other(err.to_string()))?;
        let total_validators = validators.len();
        let prev_same_bp = self
            .runtime_adapter
            .get_block_proposer(head.epoch_hash, last_height)
            .map_err(|err| Error::Other(err.to_string()))?
            == block_producer.account_id.clone();
        // If epoch changed, and before there was 2 validators and now there is 1 - prev_same_bp is false, but total validators right now is 1.
        let total_approvals =
            total_validators - if prev_same_bp || total_validators < 2 { 1 } else { 2 };
        let elapsed = self.last_block_processed.elapsed();
        if self.approvals.len() < total_approvals
            && elapsed < self.config.max_block_production_delay
        {
            // Schedule itself for (max BP delay - how much time passed).
            ctx.run_later(self.config.max_block_production_delay.sub(elapsed), move |act, ctx| {
                act.produce_block(ctx, head.last_block_hash, last_height, next_height);
            });
            return Ok(());
        }

        // If we are not producing empty blocks, skip this and call handle scheduling for the next block.
        let new_chunks = self.shards_mgr.prepare_chunks(prev_hash);

        if !self.config.produce_empty_blocks && new_chunks.is_empty() {
            self.handle_scheduling_block_production(
                ctx,
                head.last_block_hash,
                head.height,
                next_height,
            );
            return Ok(());
        }

        let prev_block = self.chain.get_block(&head.last_block_hash)?;
        let mut chunks = prev_block.chunks.clone();

        // Collect aggregate of validators and gas usage/limits from chunks.
        let mut validator_proposals = vec![];
        let mut gas_used = 0;
        let mut gas_limit = 0;
        for chunk in chunks.iter() {
            validator_proposals.extend_from_slice(&chunk.validator_proposal);
            gas_used += chunk.gas_used;
            gas_limit += chunk.gas_limit;
        }
        gas_used /= chunks.len() as u64;
        gas_limit /= chunks.len() as u64;

        // Collect new chunks.
        for (shard_id, mut chunk_header) in new_chunks {
            chunk_header.height_included = next_height;
            chunks[shard_id as usize] = chunk_header;
        }

        let prev_header = self.chain.get_block_header(&head.last_block_hash)?;

        // TODO XXX MOO: this is kept here in case we want to revive txs on the block level.
        let transactions = vec![];

        // At this point, the previous epoch hash must be available
        let epoch_hash = self
            .runtime_adapter
            .get_epoch_hash(head.last_block_hash)
            .expect("Epoch hash should exist at this point");

        // TODO: current gas used and gas limit.
        let block = Block::produce(
            &prev_header,
            next_height,
            chunks,
            epoch_hash,
            gas_used,
            gas_limit,
            transactions,
            self.approvals.drain().collect(),
            validator_proposals,
            block_producer.signer.clone(),
        );

        let ret = self.process_block(ctx, block, Provenance::PRODUCED).map_err(|err| err.into());
        assert!(ret.is_ok());
        ret
    }

    /// Check if any block with missing chunks is ready to be processed
    fn process_blocks_with_missing_chunks(&mut self, ctx: &mut Context<ClientActor>, height: u64) {
        // We need to process all heights after and including the height, since the
        //    chunk could have been included in a later block
        for height_key in self.chain.all_heights_with_missing_chunks() {
            if height_key >= height {
                let height = height_key;

                let accepted_blocks = Arc::new(RwLock::new(vec![]));
                let blocks_missing_chunks = Arc::new(RwLock::new(vec![]));
                let me = self
                    .block_producer
                    .as_ref()
                    .map(|block_producer| block_producer.account_id.clone());
                self.chain.check_blocks_with_missing_chunks(&me, height, |block, status, provenance| {
                    debug!(target: "client", "Block {} was missing chunks but now is ready to be processed", block.hash());
                    accepted_blocks.write().unwrap().push((block.hash(), status, provenance));
                }, |missing_chunks| blocks_missing_chunks.write().unwrap().push(missing_chunks));
                for (hash, status, provenance) in accepted_blocks.write().unwrap().drain(..) {
                    self.on_block_accepted(ctx, hash, status, provenance);
                }
                for missing_chunks in blocks_missing_chunks.write().unwrap().drain(..) {
                    self.shards_mgr.request_chunks(missing_chunks);
                }
            }
        }
    }

    /// Process block and execute callbacks.
    fn process_block(
        &mut self,
        ctx: &mut Context<ClientActor>,
        block: Block,
        provenance: Provenance,
    ) -> Result<(), near_chain::Error> {
        // XXX: this is bad, there is no multithreading here, what is the better way to handle this callback?
        // TODO: replace to channels or cross beams here?
        let accepted_blocks = Arc::new(RwLock::new(vec![]));
        let blocks_missing_chunks = Arc::new(RwLock::new(vec![]));
        let result = {
            let me = self
                .block_producer
                .as_ref()
                .map(|block_producer| block_producer.account_id.clone());
            self.chain.process_block(
                &me,
                block,
                provenance,
                |block, status, provenance| {
                    accepted_blocks.write().unwrap().push((block.hash(), status, provenance));
                },
                |missing_chunks| blocks_missing_chunks.write().unwrap().push(missing_chunks),
            )
        };
        // Process all blocks that were accepted.
        for (hash, status, provenance) in accepted_blocks.write().unwrap().drain(..) {
            self.on_block_accepted(ctx, hash, status, provenance);
        }
        for missing_chunks in blocks_missing_chunks.write().unwrap().drain(..) {
            self.shards_mgr.request_chunks(missing_chunks);
        }
        result.map(|_| ())
    }

    /// Processes received block, returns boolean if block was reasonable or malicious.
    fn receive_block(
        &mut self,
        ctx: &mut Context<ClientActor>,
        block: Block,
        peer_id: PeerId,
        was_requested: bool,
    ) -> NetworkClientResponses {
        let hash = block.hash();
        debug!(target: "client", "Received block {} <- {} at {} from {}", hash, block.header.prev_hash, block.header.height, peer_id);
        let prev_hash = block.header.prev_hash;
        let provenance =
            if was_requested { near_chain::Provenance::SYNC } else { near_chain::Provenance::NONE };
        match self.process_block(ctx, block, provenance) {
            Ok(_) => NetworkClientResponses::NoResponse,
            Err(ref err) if err.is_bad_data() => {
                NetworkClientResponses::Ban { ban_reason: ReasonForBan::BadBlock }
            }
            Err(ref err) if err.is_error() => {
                if self.sync_status.is_syncing() {
                    // While syncing, we may receive blocks that are older or from next epochs.
                    // This leads to Old Block or EpochOutOfBounds errors.
                    info!(target: "client", "Error on receival of block: {}", err);
                } else {
                    error!(target: "client", "Error on receival of block: {}", err);
                }
                NetworkClientResponses::NoResponse
            }
            Err(e) => match e.kind() {
                near_chain::ErrorKind::Orphan => {
                    if !self.chain.is_orphan(&prev_hash) && !self.sync_status.is_syncing() {
                        self.request_block_by_hash(prev_hash, peer_id)
                    }
                    NetworkClientResponses::NoResponse
                }
                near_chain::ErrorKind::ChunksMissing(missing_chunks) => {
                    debug!(
                        "Chunks were missing for block {}, I'm {}, requesting. Missing: {:?}, ({:?})",
                        hash.clone(),
                        self.block_producer.as_ref().unwrap().account_id.clone(),
                        missing_chunks.clone(),
                        missing_chunks.iter().map(|header| header.chunk_hash()).collect::<Vec<_>>()
                    );
                    self.shards_mgr.request_chunks(missing_chunks);
                    NetworkClientResponses::NoResponse
                }
                _ => {
                    debug!("Process block: block {} refused by chain: {}", hash, e.kind());
                    NetworkClientResponses::NoResponse
                }
            },
        }
    }

    fn receive_header(&mut self, header: BlockHeader, peer_info: PeerId) -> NetworkClientResponses {
        let hash = header.hash();
        debug!(target: "client", "Received block header {} at {} from {}", hash, header.height, peer_info);

        // Process block by chain, if it's valid header ask for the block.
        let result = self.chain.process_block_header(&header);

        match result {
            Err(ref e) if e.is_bad_data() => {
                return NetworkClientResponses::Ban { ban_reason: ReasonForBan::BadBlockHeader }
            }
            // Some error that worth surfacing.
            Err(ref e) if e.is_error() => {
                error!(target: "client", "Error on receival of header: {}", e);
                return NetworkClientResponses::NoResponse;
            }
            // Got an error when trying to process the block header, but it's not due to
            // invalid data or underlying error. Surface as fine.
            Err(_) => return NetworkClientResponses::NoResponse,
            _ => {}
        }

        // Succesfully processed a block header and can request the full block.
        self.request_block_by_hash(header.hash(), peer_info);
        NetworkClientResponses::NoResponse
    }

    fn receive_headers(&mut self, headers: Vec<BlockHeader>, peer_id: PeerId) -> bool {
        info!(target: "client", "Received {} block headers from {}", headers.len(), peer_id);
        if headers.len() == 0 {
            return true;
        }
        match self.chain.sync_block_headers(headers) {
            Ok(_) => true,
            Err(err) => {
                if err.is_bad_data() {
                    error!(target: "client", "Error processing sync blocks: {}", err);
                    false
                } else {
                    debug!(target: "client", "Block headers refused by chain: {}", err);
                    true
                }
            }
        }
    }

    fn request_block_by_hash(&mut self, hash: CryptoHash, peer_id: PeerId) {
        match self.chain.block_exists(&hash) {
            Ok(false) => {
                // TODO: ?? should we add a wait for response here?
                let _ = self.network_actor.do_send(NetworkRequests::BlockRequest { hash, peer_id });
            }
            Ok(true) => {
                debug!(target: "client", "send_block_request_to_peer: block {} already known", hash)
            }
            Err(e) => {
                error!(target: "client", "send_block_request_to_peer: failed to check block exists: {:?}", e)
            }
        }
    }

    fn retrieve_headers(
        &mut self,
        hashes: Vec<CryptoHash>,
    ) -> Result<Vec<BlockHeader>, near_chain::Error> {
        let header = match self.chain.find_common_header(&hashes) {
            Some(header) => header,
            None => return Ok(vec![]),
        };

        let mut headers = vec![];
        let max_height = self.chain.header_head()?.height;
        // TODO: this may be inefficient if there are a lot of skipped blocks.
        for h in header.height + 1..=max_height {
            if let Ok(header) = self.chain.get_header_by_height(h) {
                headers.push(header.clone());
                if headers.len() >= sync::MAX_BLOCK_HEADERS as usize {
                    break;
                }
            }
        }
        Ok(headers)
    }

    /// Check whether need to (continue) sync.
    fn needs_syncing(&self) -> Result<(bool, u64), near_chain::Error> {
        let head = self.chain.head()?;
        let mut is_syncing = self.sync_status.is_syncing();

        let full_peer_info =
            if let Some(full_peer_info) = most_weight_peer(&self.network_info.most_weight_peers) {
                full_peer_info
            } else {
                if !self.config.skip_sync_wait {
                    warn!(target: "client", "Sync: no peers available, disabling sync");
                }
                return Ok((false, 0));
            };

        if is_syncing {
            if full_peer_info.chain_info.total_weight <= head.total_weight {
                info!(target: "client", "Sync: synced at {} @ {} [{}]", head.total_weight.to_num(), head.height, head.last_block_hash);
                is_syncing = false;
            }
        } else {
            if full_peer_info.chain_info.total_weight.to_num()
                > head.total_weight.to_num() + self.config.sync_weight_threshold
                && full_peer_info.chain_info.height
                    > head.height + self.config.sync_height_threshold
            {
                info!(
                    target: "client",
                    "Sync: height/weight: {}/{}, peer height/weight: {}/{}, enabling sync",
                    head.height,
                    head.total_weight,
                    full_peer_info.chain_info.height,
                    full_peer_info.chain_info.total_weight
                );
                is_syncing = true;
            }
        }
        Ok((is_syncing, full_peer_info.chain_info.height))
    }

    /// Starts syncing and then switches to either syncing or regular mode.
    fn start_sync(&mut self, ctx: &mut Context<ClientActor>) {
        // Wait for connections reach at least minimum peers unless skipping sync.
        if self.network_info.num_active_peers < self.config.min_num_peers
            && !self.config.skip_sync_wait
        {
            ctx.run_later(self.config.sync_step_period, move |act, ctx| {
                act.start_sync(ctx);
            });
            return;
        }
        // Start main sync loop.
        self.sync(ctx);
    }

    fn find_sync_hash(&mut self) -> Result<CryptoHash, near_chain::Error> {
        let header_head = self.chain.header_head()?;
        let mut sync_hash = header_head.prev_block_hash;
        for _ in 0..self.config.state_fetch_horizon {
            sync_hash = self.chain.get_block_header(&sync_hash)?.prev_hash;
        }
        Ok(sync_hash)
    }

    /// Walks through all the ongoing state syncs for future epochs and processes them
    fn catchup(&mut self, ctx: &mut Context<ClientActor>) -> Result<(), Error> {
        let me = &self.block_producer.as_ref().map(|x| x.account_id.clone());
        for (sync_hash, state_sync_info) in self.chain.store().iterate_state_sync_infos() {
            assert_eq!(sync_hash, state_sync_info.epoch_tail_hash);
            let network_actor1 = self.network_actor.clone();

            let (state_sync, new_shard_sync) = self
                .catchup_state_syncs
                .entry(sync_hash)
                .or_insert_with(|| (StateSync::new(network_actor1), HashMap::new()));

            debug!(
                target: "client",
                "Catchup me: {:?}: sync_hash: {:?}, sync_info: {:?}", me, sync_hash, new_shard_sync
            );

            match state_sync.run(
                sync_hash,
                new_shard_sync,
                &mut self.chain,
                &self.network_info.most_weight_peers,
                state_sync_info.shards.iter().map(|tuple| tuple.0).collect(),
            )? {
                StateSyncResult::Unchanged => {}
                StateSyncResult::Changed => {}
                StateSyncResult::Completed => {
                    let accepted_blocks = Arc::new(RwLock::new(vec![]));
                    let blocks_missing_chunks = Arc::new(RwLock::new(vec![]));

                    self.chain.catchup_blocks(
                        me,
                        sync_hash,
                        |block, status, provenance| {
                            accepted_blocks.write().unwrap().push((
                                block.hash(),
                                status,
                                provenance,
                            ));
                        },
                        |missing_chunks| {
                            blocks_missing_chunks.write().unwrap().push(missing_chunks)
                        },
                    )?;

                    for (hash, status, provenance) in accepted_blocks.write().unwrap().drain(..) {
                        self.on_block_accepted(ctx, hash, status, provenance);
                    }
                    for missing_chunks in blocks_missing_chunks.write().unwrap().drain(..) {
                        self.shards_mgr.request_chunks(missing_chunks);
                    }
                }
            }
        }

        Ok(())
    }

    /// Main syncing job responsible for syncing client with other peers.
    fn sync(&mut self, ctx: &mut Context<ClientActor>) {
        // Macro to schedule to call this function later if error occurred.
        macro_rules! unwrap_or_run_later(($obj: expr) => (match $obj {
            Ok(v) => v,
            Err(err) => {
                error!(target: "sync", "Sync: Unexpected error: {}", err);
                ctx.run_later(self.config.sync_step_period, move |act, ctx| {
                    act.sync(ctx);
                });
                return;
            }
        }));

        match self.catchup(ctx) {
            Ok(_) => {}
            Err(err) => error!("Error occurred during state sync for some future epoch: {:?}", err),
        }

        let mut wait_period = self.config.sync_step_period;

        let currently_syncing = self.sync_status.is_syncing();
        let (needs_syncing, highest_height) = unwrap_or_run_later!(self.needs_syncing());

        if !needs_syncing {
            if currently_syncing {
                self.last_block_processed = Instant::now();
                self.sync_status = SyncStatus::NoSync;

                // Initial transition out of "syncing" state.
                // Start by handling scheduling block production if needed.
                let head = unwrap_or_run_later!(self.chain.head());
                println!("MOO CHECK ANNOUNCE: {:?}", head);
                self.check_send_announce_account(head.epoch_hash);
                self.handle_scheduling_block_production(
                    ctx,
                    head.last_block_hash,
                    head.height,
                    head.height,
                );
            }
            wait_period = self.config.sync_check_period;
        } else {
            // Run each step of syncing separately.
            unwrap_or_run_later!(self.header_sync.run(
                &mut self.sync_status,
                &mut self.chain,
                highest_height,
                &self.network_info.most_weight_peers
            ));
            // Only body / state sync if header height is latest.
            let header_head = unwrap_or_run_later!(self.chain.header_head());
            if header_head.height == highest_height {
                // Sync state if already running sync state or if block sync is too far.
                let sync_state = match self.sync_status {
                    SyncStatus::StateSync(_, _) => true,
                    _ => unwrap_or_run_later!(self.block_sync.run(
                        &mut self.sync_status,
                        &mut self.chain,
                        highest_height,
                        &self.network_info.most_weight_peers
                    )),
                };
                if sync_state {
                    let (sync_hash, mut new_shard_sync) = match &self.sync_status {
                        SyncStatus::StateSync(sync_hash, shard_sync) => {
                            (sync_hash.clone(), shard_sync.clone())
                        }
                        _ => (unwrap_or_run_later!(self.find_sync_hash()), HashMap::default()),
                    };

                    let me = &self.block_producer.as_ref().map(|x| x.account_id.clone());
                    match unwrap_or_run_later!(self.state_sync.run(
                        sync_hash,
                        &mut new_shard_sync,
                        &mut self.chain,
                        &self.network_info.most_weight_peers,
                        // TODO: add tracking shards here.
                        vec![0],
                    )) {
                        StateSyncResult::Unchanged => (),
                        StateSyncResult::Changed => {
                            self.sync_status = SyncStatus::StateSync(sync_hash, new_shard_sync)
                        }
                        StateSyncResult::Completed => {
                            info!(target: "sync", "State sync: all shards are done");

                            let accepted_blocks = Arc::new(RwLock::new(vec![]));
                            let blocks_missing_chunks = Arc::new(RwLock::new(vec![]));

                            unwrap_or_run_later!(self.chain.reset_heads_post_state_sync(
                                me,
                                sync_hash,
                                |block, status, provenance| {
                                    accepted_blocks.write().unwrap().push((
                                        block.hash(),
                                        status,
                                        provenance,
                                    ));
                                },
                                |missing_chunks| {
                                    blocks_missing_chunks.write().unwrap().push(missing_chunks)
                                },
                            ));

                            for (hash, status, provenance) in
                                accepted_blocks.write().unwrap().drain(..)
                            {
                                self.on_block_accepted(ctx, hash, status, provenance);
                            }
                            for missing_chunks in blocks_missing_chunks.write().unwrap().drain(..) {
                                self.shards_mgr.request_chunks(missing_chunks);
                            }

                            self.sync_status =
                                SyncStatus::BodySync { current_height: 0, highest_height: 0 };
                        }
                    }
                }
            }
        }

        ctx.run_later(wait_period, move |act, ctx| {
            act.sync(ctx);
        });
    }

    /// Periodically fetch network info.
    fn fetch_network_info(&mut self, ctx: &mut Context<Self>) {
        // TODO: replace with push from network?
        self.network_actor
            .send(NetworkRequests::FetchInfo { level: 0 })
            .into_actor(self)
            .then(move |res, act, _ctx| match res {
                Ok(NetworkResponses::Info(network_info)) => {
                    act.network_info = network_info;
                    actix::fut::ok(())
                }
                Ok(NetworkResponses::NoResponse) => actix::fut::ok(()),
                Err(e) => {
                    error!(target: "client", "Sync: recieved error or incorrect result: {}", e);
                    actix::fut::err(())
                }
            })
            .wait(ctx);

        ctx.run_later(self.config.fetch_info_period, move |act, ctx| {
            act.fetch_network_info(ctx);
        });
    }

    /// Periodically log summary.
    fn log_summary(&self, ctx: &mut Context<Self>) {
        ctx.run_later(self.config.log_summary_period, move |act, ctx| {
            let head = unwrap_or_return!(act.chain.head(), ());
            let validators = unwrap_or_return!(act.get_epoch_block_proposers(head.epoch_hash), ());
            let num_validators = validators.len();
            let is_validator = if let Some(block_producer) = &act.block_producer {
                validators.contains(&block_producer.account_id)
            } else {
                false
            };

            act.info_helper.info(
                &head,
                &act.sync_status,
                &act.node_id,
                &act.network_info,
                is_validator,
                num_validators,
            );

            act.log_summary(ctx);
        });
    }

    /// Collects block approvals. Returns false if block approval is invalid.
    fn collect_block_approval(
        &mut self,
        account_id: &AccountId,
        hash: &CryptoHash,
        signature: &Signature,
    ) -> bool {
        // TODO: figure out how to validate better before hitting the disk? For example validator and account cache to validate signature first.
        // TODO: This header is missing, should collect for later? should have better way to verify then.
        let header = unwrap_or_return!(self.chain.get_block_header(&hash), true).clone();

        // TODO: Access runtime adapter only once to find the position and public key.

        // If given account is not current block proposer.
        let position = match self.runtime_adapter.get_epoch_block_proposers(header.epoch_hash) {
            Ok(validators) => validators.iter().position(|x| x == account_id),
            Err(err) => {
                error!(target: "client", "Error: {}", err);
                return false;
            }
        };
        if position.is_none() {
            return false;
        }

        // Check signature is correct for given validator.
        if !self.runtime_adapter.verify_validator_signature(
            &header.epoch_hash,
            account_id,
            hash.as_ref(),
            signature,
        ) {
            return false;
        }

        debug!(target: "client", "Received approval for {} from {}", hash, account_id);
        self.approvals.insert(position.unwrap(), signature.clone());
        true
    }

    fn state_request(
        &mut self,
        shard_id: ShardId,
        hash: CryptoHash,
    ) -> Result<(Vec<u8>, Vec<(CryptoHash, Vec<ReceiptTransaction>)>), near_chain::Error> {
        let header = self.chain.get_block_header(&hash)?;
        let payload = self
            .runtime_adapter
            .dump_state(shard_id, header.prev_state_root)
            .map_err(|err| ErrorKind::Other(err.to_string()))?;
        // TODO XXX receipts
        let receipts = vec![]; //self.chain.get_receipts(&prev_hash)?.clone();
        Ok((payload, receipts))
    }
}
