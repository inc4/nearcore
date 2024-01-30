use crate::stateless_validation::chunk_endorsement_tracker::ChunkEndorsementTracker;
use crate::{metrics, Client};
use itertools::Itertools;
use near_async::messaging::{CanSend, Sender};
use near_cache::SyncLruCache;
use near_chain::chain::{
    apply_new_chunk, apply_old_chunk, NewChunkData, NewChunkResult, OldChunkData, OldChunkResult,
    ShardContext, StorageContext,
};
use near_chain::sharding::shuffle_receipt_proofs;
use near_chain::types::{
    ApplyChunkBlockContext, ApplyChunkResult, PreparedTransactions, RuntimeAdapter,
    RuntimeStorageConfig, StorageDataSource,
};
use near_chain::validate::validate_chunk_with_chunk_extra_and_receipts_root;
use near_chain::{Block, Chain, ChainStoreAccess};
use near_chain_primitives::Error;
use near_epoch_manager::EpochManagerAdapter;
use near_network::types::{NetworkRequests, PeerManagerMessageRequest, ReasonForBan};
use near_pool::TransactionGroupIteratorWrapper;
use near_primitives::hash::{hash, CryptoHash};
use near_primitives::merkle::merklize;
use near_primitives::network::PeerId;
use near_primitives::receipt::Receipt;
use near_primitives::sharding::{ChunkHash, ReceiptProof, ShardChunkHeader};
use near_primitives::stateless_validation::{
    ChunkEndorsement, ChunkStateWitness, ChunkStateWitnessInner,
};
use near_primitives::transaction::SignedTransaction;
use near_primitives::types::chunk_extra::ChunkExtra;
use near_primitives::types::{AccountId, ShardId};
use near_primitives::validator_signer::ValidatorSigner;
use near_store::PartialStorage;
use std::collections::HashMap;
use std::sync::Arc;

// After validating a chunk state witness, we ideally need to send the chunk endorsement
// to just the next block producer at height h. However, it's possible that blocks at height
// h may be skipped and block producer at height h+1 picks up the chunk. We need to ensure
// that these later block producers also receive the chunk endorsement.
// Keeping a threshold of 5 block producers should be sufficient for most scenarios.
const NUM_NEXT_BLOCK_PRODUCERS_TO_SEND_CHUNK_ENDORSEMENT: u64 = 5;

/// A module that handles chunk validation logic. Chunk validation refers to a
/// critical process of stateless validation, where chunk validators (certain
/// validators selected to validate the chunk) verify that the chunk's state
/// witness is correct, and then send chunk endorsements to the block producer
/// so that the chunk can be included in the block.
pub struct ChunkValidator {
    /// The signer for our own node, if we are a validator. If not, this is None.
    my_signer: Option<Arc<dyn ValidatorSigner>>,
    epoch_manager: Arc<dyn EpochManagerAdapter>,
    network_sender: Sender<PeerManagerMessageRequest>,
    runtime_adapter: Arc<dyn RuntimeAdapter>,
}

impl ChunkValidator {
    pub fn new(
        my_signer: Option<Arc<dyn ValidatorSigner>>,
        epoch_manager: Arc<dyn EpochManagerAdapter>,
        network_sender: Sender<PeerManagerMessageRequest>,
        runtime_adapter: Arc<dyn RuntimeAdapter>,
    ) -> Self {
        Self { my_signer, epoch_manager, network_sender, runtime_adapter }
    }

    /// Performs the chunk validation logic. When done, it will send the chunk
    /// endorsement message to the block producer. The actual validation logic
    /// happens in a separate thread.
    pub fn start_validating_chunk(
        &self,
        state_witness: ChunkStateWitness,
        chain: &Chain,
        peer_id: PeerId,
        chunk_endorsement_tracker: &ChunkEndorsementTracker,
    ) -> Result<(), Error> {
        if !self.epoch_manager.verify_chunk_state_witness_signature(&state_witness)? {
            return Err(Error::InvalidChunkStateWitness("Invalid signature".to_string()));
        }

        let state_witness_inner = state_witness.inner;
        let chunk_header = state_witness_inner.chunk_header.clone();
        let Some(my_signer) = self.my_signer.as_ref() else {
            return Err(Error::NotAValidator);
        };
        let epoch_id =
            self.epoch_manager.get_epoch_id_from_prev_block(chunk_header.prev_block_hash())?;
        // We will only validate something if we are a chunk validator for this chunk.
        // Note this also covers the case before the protocol upgrade for chunk validators,
        // because the chunk validators will be empty.
        let chunk_validator_assignments = self.epoch_manager.get_chunk_validator_assignments(
            &epoch_id,
            chunk_header.shard_id(),
            chunk_header.height_created(),
        )?;
        if !chunk_validator_assignments.contains(my_signer.validator_id()) {
            return Err(Error::NotAChunkValidator);
        }

        let pre_validation_result = pre_validate_chunk_state_witness(
            &state_witness_inner,
            chain,
            self.epoch_manager.as_ref(),
            self.runtime_adapter.as_ref(),
        )?;

        let network_sender = self.network_sender.clone();
        let signer = my_signer.clone();
        let epoch_manager = self.epoch_manager.clone();
        let runtime_adapter = self.runtime_adapter.clone();
        let my_chunk_endorsements = chunk_endorsement_tracker.chunk_endorsements.clone();
        rayon::spawn(move || {
            match validate_chunk_state_witness(
                state_witness_inner,
                pre_validation_result,
                epoch_manager.as_ref(),
                runtime_adapter.as_ref(),
            ) {
                Ok(()) => {
                    send_chunk_endorsement_to_block_producers(
                        &chunk_header,
                        epoch_manager.as_ref(),
                        signer.as_ref(),
                        &network_sender,
                        my_chunk_endorsements.as_ref(),
                    );
                }
                Err(err) => {
                    if let Error::InvalidChunkStateWitness(_) = &err {
                        network_sender.send(PeerManagerMessageRequest::NetworkRequests(
                            NetworkRequests::BanPeer {
                                peer_id,
                                ban_reason: ReasonForBan::BadChunkStateWitness,
                            },
                        ));
                    }
                    tracing::error!("Failed to validate chunk: {:?}", err);
                }
            }
        });
        Ok(())
    }
}

/// Checks that proposed `transactions` are valid for a chunk with `chunk_header`.
/// Uses `storage_config` to possibly record reads or use recorded storage.
pub(crate) fn validate_prepared_transactions(
    chain: &Chain,
    runtime_adapter: &dyn RuntimeAdapter,
    chunk_header: &ShardChunkHeader,
    storage_config: RuntimeStorageConfig,
    transactions: &[SignedTransaction],
) -> Result<PreparedTransactions, Error> {
    let parent_block_header =
        chain.chain_store().get_block_header(chunk_header.prev_block_hash())?;

    runtime_adapter.prepare_transactions(
        storage_config,
        chunk_header.into(),
        (&parent_block_header).into(),
        &mut TransactionGroupIteratorWrapper::new(transactions),
        &mut chain.transaction_validity_check(parent_block_header),
        None,
    )
}

/// Pre-validates the chunk's receipts and transactions against the chain.
/// We do this before handing off the computationally intensive part to a
/// validation thread.
pub(crate) fn pre_validate_chunk_state_witness(
    state_witness: &ChunkStateWitnessInner,
    chain: &Chain,
    epoch_manager: &dyn EpochManagerAdapter,
    runtime_adapter: &dyn RuntimeAdapter,
) -> Result<PreValidationOutput, Error> {
    let store = chain.chain_store();
    let shard_id = state_witness.chunk_header.shard_id();

    // First, go back through the blockchain history to locate the last new chunk
    // and last last new chunk for the shard.

    // Blocks from the last new chunk (exclusive) to the parent block (inclusive).
    let mut blocks_after_last_chunk = Vec::new();
    // Blocks from the last last new chunk (exclusive) to the last new chunk (inclusive).
    let mut blocks_after_last_last_chunk = Vec::new();

    {
        let mut block_hash = *state_witness.chunk_header.prev_block_hash();
        let mut prev_chunks_seen = 0;
        loop {
            let block = store.get_block(&block_hash)?;
            let chunks = block.chunks();
            let Some(chunk) = chunks.get(shard_id as usize) else {
                return Err(Error::InvalidChunkStateWitness(format!(
                    "Shard {} does not exist in block {:?}",
                    shard_id, block_hash
                )));
            };
            let is_new_chunk = chunk.is_new_chunk(block.header().height());
            block_hash = *block.header().prev_hash();
            if is_new_chunk {
                prev_chunks_seen += 1;
            }
            if prev_chunks_seen == 0 {
                blocks_after_last_chunk.push(block);
            } else if prev_chunks_seen == 1 {
                blocks_after_last_last_chunk.push(block);
            }
            if prev_chunks_seen == 2 {
                break;
            }
        }
    }

    let receipts_to_apply = validate_source_receipt_proofs(
        &state_witness.source_receipt_proofs,
        &blocks_after_last_last_chunk,
        shard_id,
    )?;
    let applied_receipts_hash = hash(&borsh::to_vec(receipts_to_apply.as_slice()).unwrap());
    if applied_receipts_hash != state_witness.applied_receipts_hash {
        return Err(Error::InvalidChunkStateWitness(format!(
            "Receipts hash {:?} does not match expected receipts hash {:?}",
            applied_receipts_hash, state_witness.applied_receipts_hash
        )));
    }
    let (tx_root_from_state_witness, _) = merklize(&state_witness.transactions);
    let last_chunk_block = blocks_after_last_last_chunk.first().ok_or_else(|| {
        Error::Other("blocks_after_last_last_chunk is empty, this should be impossible!".into())
    })?;
    let last_new_chunk_tx_root =
        last_chunk_block.chunks().get(shard_id as usize).unwrap().tx_root();
    if last_new_chunk_tx_root != tx_root_from_state_witness {
        return Err(Error::InvalidChunkStateWitness(format!(
            "Transaction root {:?} does not match expected transaction root {:?}",
            tx_root_from_state_witness, last_new_chunk_tx_root
        )));
    }

    // Verify that all proposed transactions are valid.
    let new_transactions = &state_witness.new_transactions;
    if !new_transactions.is_empty() {
        let transactions_validation_storage_config = RuntimeStorageConfig {
            state_root: state_witness.chunk_header.prev_state_root(),
            use_flat_storage: true,
            source: StorageDataSource::Recorded(PartialStorage {
                nodes: state_witness.new_transactions_validation_state.clone(),
            }),
            state_patch: Default::default(),
            record_storage: false,
        };

        match validate_prepared_transactions(
            chain,
            runtime_adapter,
            &state_witness.chunk_header,
            transactions_validation_storage_config,
            &new_transactions,
        ) {
            Ok(result) => {
                if result.transactions.len() != new_transactions.len() {
                    return Err(Error::InvalidChunkStateWitness(format!(
                        "New transactions validation failed. {} transactions out of {} proposed transactions were valid.",
                        result.transactions.len(),
                        new_transactions.len(),
                    )));
                }
            }
            Err(error) => {
                return Err(Error::InvalidChunkStateWitness(format!(
                    "New transactions validation failed: {}",
                    error,
                )));
            }
        };
    }

    Ok(PreValidationOutput {
        main_transition_params: NewChunkData {
            chunk_header: last_chunk_block.chunks().get(shard_id as usize).unwrap().clone(),
            transactions: state_witness.transactions.clone(),
            receipts: receipts_to_apply,
            resharding_state_roots: None,
            block: Chain::get_apply_chunk_block_context(
                epoch_manager,
                last_chunk_block.header(),
                &store.get_previous_header(last_chunk_block.header())?,
                true,
            )?,
            is_first_block_with_chunk_of_version: false,
            storage_context: StorageContext {
                storage_data_source: StorageDataSource::Recorded(PartialStorage {
                    nodes: state_witness.main_state_transition.base_state.clone(),
                }),
                state_patch: Default::default(),
                record_storage: false,
            },
        },
        implicit_transition_params: blocks_after_last_chunk
            .into_iter()
            .rev()
            .map(|block| -> Result<_, Error> {
                Ok(Chain::get_apply_chunk_block_context(
                    epoch_manager,
                    block.header(),
                    &store.get_previous_header(block.header())?,
                    false,
                )?)
            })
            .collect::<Result<_, _>>()?,
    })
}

/// Validate that receipt proofs contain the receipts that should be applied during the
/// transition proven by ChunkStateWitness. The receipts are extracted from the proofs
/// and arranged in the order in which they should be applied during the transition.
/// TODO(resharding): Handle resharding properly. If the receipts were sent from before
/// a resharding boundary, we should first validate the proof using the pre-resharding
/// target_shard_id and then extract the receipts that are targeted at this half of a split shard.
fn validate_source_receipt_proofs(
    source_receipt_proofs: &HashMap<ChunkHash, ReceiptProof>,
    receipt_source_blocks: &[Block],
    target_chunk_shard_id: ShardId,
) -> Result<Vec<Receipt>, Error> {
    let mut receipts_to_apply = Vec::new();
    let mut expected_proofs_len = 0;

    // Iterate over blocks between last_chunk_block (inclusive) and last_last_chunk_block (exclusive),
    // from the newest blocks to the oldest.
    for block in receipt_source_blocks {
        // Collect all receipts coming from this block.
        let mut block_receipt_proofs = Vec::new();

        for chunk in block.chunks().iter() {
            if !chunk.is_new_chunk(block.header().height()) {
                continue;
            }

            // Collect receipts coming from this chunk and validate that they are correct.
            let Some(receipt_proof) = source_receipt_proofs.get(&chunk.chunk_hash()) else {
                return Err(Error::InvalidChunkStateWitness(format!(
                    "Missing source receipt proof for chunk {:?}",
                    chunk.chunk_hash()
                )));
            };
            validate_receipt_proof(receipt_proof, chunk, target_chunk_shard_id)?;

            expected_proofs_len += 1;
            block_receipt_proofs.push(receipt_proof);
        }

        // Arrange the receipts in the order in which they should be applied.
        shuffle_receipt_proofs(&mut block_receipt_proofs, block.hash());
        for proof in block_receipt_proofs {
            receipts_to_apply.extend(proof.0.iter().cloned());
        }
    }

    // Check that there are no extraneous proofs in source_receipt_proofs.
    if source_receipt_proofs.len() != expected_proofs_len {
        return Err(Error::InvalidChunkStateWitness(format!(
            "source_receipt_proofs contains too many proofs. Expected {} proofs, found {}",
            expected_proofs_len,
            source_receipt_proofs.len()
        )));
    }
    Ok(receipts_to_apply)
}

fn validate_receipt_proof(
    receipt_proof: &ReceiptProof,
    from_chunk: &ShardChunkHeader,
    target_chunk_shard_id: ShardId,
) -> Result<(), Error> {
    // Validate that from_shard_id is correct. The receipts must match the outgoing receipt root
    // for this shard, so it's impossible to fake it.
    if receipt_proof.1.from_shard_id != from_chunk.shard_id() {
        return Err(Error::InvalidChunkStateWitness(format!(
            "Receipt proof for chunk {:?} is from shard {}, expected shard {}",
            from_chunk.chunk_hash(),
            receipt_proof.1.from_shard_id,
            from_chunk.shard_id(),
        )));
    }
    // Validate that to_shard_id is correct. to_shard_id is also encoded in the merkle tree,
    // so it's impossible to fake it.
    if receipt_proof.1.to_shard_id != target_chunk_shard_id {
        return Err(Error::InvalidChunkStateWitness(format!(
            "Receipt proof for chunk {:?} is for shard {}, expected shard {}",
            from_chunk.chunk_hash(),
            receipt_proof.1.to_shard_id,
            target_chunk_shard_id
        )));
    }
    // Verify that (receipts, to_shard_id) belongs to the merkle tree of outgoing receipts in from_chunk.
    if !receipt_proof.verify_against_receipt_root(from_chunk.prev_outgoing_receipts_root()) {
        return Err(Error::InvalidChunkStateWitness(format!(
            "Receipt proof for chunk {:?} has invalid merkle path, doesn't match outgoing receipts root",
            from_chunk.chunk_hash()
        )));
    }
    Ok(())
}

pub(crate) struct PreValidationOutput {
    main_transition_params: NewChunkData,
    implicit_transition_params: Vec<ApplyChunkBlockContext>,
}

pub(crate) fn validate_chunk_state_witness(
    state_witness: ChunkStateWitnessInner,
    pre_validation_output: PreValidationOutput,
    epoch_manager: &dyn EpochManagerAdapter,
    runtime_adapter: &dyn RuntimeAdapter,
) -> Result<(), Error> {
    let main_transition = pre_validation_output.main_transition_params;
    let _timer = metrics::CHUNK_STATE_WITNESS_VALIDATION_TIME
        .with_label_values(&[&main_transition.chunk_header.shard_id().to_string()])
        .start_timer();
    let span = tracing::debug_span!(target: "chain", "validate_chunk_state_witness").entered();
    let chunk_header = main_transition.chunk_header.clone();
    let epoch_id = epoch_manager.get_epoch_id(&main_transition.block.block_hash)?;
    let shard_uid =
        epoch_manager.shard_id_to_uid(main_transition.chunk_header.shard_id(), &epoch_id)?;
    // Should we validate other fields?
    let NewChunkResult { apply_result: mut main_apply_result, .. } = apply_new_chunk(
        &span,
        main_transition,
        ShardContext {
            shard_uid,
            cares_about_shard_this_epoch: true,
            will_shard_layout_change: false,
            should_apply_chunk: true,
            need_to_reshard: false,
        },
        runtime_adapter,
        epoch_manager,
    )?;
    let outgoing_receipts = std::mem::take(&mut main_apply_result.outgoing_receipts);
    let mut chunk_extra = apply_result_to_chunk_extra(main_apply_result, &chunk_header);
    if chunk_extra.state_root() != &state_witness.main_state_transition.post_state_root {
        // This is an early check, it's not for correctness, only for better
        // error reporting in case of an invalid state witness due to a bug.
        // Only the final state root check against the chunk header is required.
        return Err(Error::InvalidChunkStateWitness(format!(
            "Post state root {:?} for main transition does not match expected post state root {:?}",
            chunk_extra.state_root(),
            state_witness.main_state_transition.post_state_root,
        )));
    }

    for (block, transition) in pre_validation_output
        .implicit_transition_params
        .into_iter()
        .zip(state_witness.implicit_transitions.into_iter())
    {
        let block_hash = block.block_hash;
        let old_chunk_data = OldChunkData {
            prev_chunk_extra: chunk_extra.clone(),
            resharding_state_roots: None,
            block,
            storage_context: StorageContext {
                storage_data_source: StorageDataSource::Recorded(PartialStorage {
                    nodes: transition.base_state,
                }),
                state_patch: Default::default(),
                record_storage: false,
            },
        };
        let OldChunkResult { apply_result, .. } = apply_old_chunk(
            &span,
            old_chunk_data,
            ShardContext {
                // Consider other shard uid in case of resharding.
                shard_uid,
                cares_about_shard_this_epoch: true,
                will_shard_layout_change: false,
                should_apply_chunk: false,
                need_to_reshard: false,
            },
            runtime_adapter,
            epoch_manager,
        )?;
        *chunk_extra.state_root_mut() = apply_result.new_root;
        if chunk_extra.state_root() != &transition.post_state_root {
            // This is an early check, it's not for correctness, only for better
            // error reporting in case of an invalid state witness due to a bug.
            // Only the final state root check against the chunk header is required.
            return Err(Error::InvalidChunkStateWitness(format!(
                "Post state root {:?} for implicit transition at block {:?}, does not match expected state root {:?}",
                chunk_extra.state_root(), block_hash, transition.post_state_root
            )));
        }
    }

    // Finally, verify that the newly proposed chunk matches everything we have computed.
    let outgoing_receipts_hashes = {
        let shard_layout = epoch_manager
            .get_shard_layout_from_prev_block(state_witness.chunk_header.prev_block_hash())?;
        Chain::build_receipts_hashes(&outgoing_receipts, &shard_layout)
    };
    let (outgoing_receipts_root, _) = merklize(&outgoing_receipts_hashes);
    validate_chunk_with_chunk_extra_and_receipts_root(
        &chunk_extra,
        &state_witness.chunk_header,
        &outgoing_receipts_root,
    )?;

    Ok(())
}

fn apply_result_to_chunk_extra(
    apply_result: ApplyChunkResult,
    chunk: &ShardChunkHeader,
) -> ChunkExtra {
    let (outcome_root, _) = ApplyChunkResult::compute_outcomes_proof(&apply_result.outcomes);
    ChunkExtra::new(
        &apply_result.new_root,
        outcome_root,
        apply_result.validator_proposals,
        apply_result.total_gas_burnt,
        chunk.gas_limit(),
        apply_result.total_balance_burnt,
    )
}

pub fn send_chunk_endorsement_to_block_producers(
    chunk_header: &ShardChunkHeader,
    epoch_manager: &dyn EpochManagerAdapter,
    signer: &dyn ValidatorSigner,
    network_sender: &Sender<PeerManagerMessageRequest>,
    my_chunk_endorsements: &SyncLruCache<ChunkHash, HashMap<AccountId, ChunkEndorsement>>,
) {
    let epoch_id =
        epoch_manager.get_epoch_id_from_prev_block(chunk_header.prev_block_hash()).unwrap();

    // Send the chunk endorsement to the next NUM_NEXT_BLOCK_PRODUCERS_TO_SEND_CHUNK_ENDORSEMENT block producers.
    // It's possible we may reach the end of the epoch, in which case, ignore the error from get_block_producer.
    let block_height = chunk_header.height_created();
    let block_producers = (0..NUM_NEXT_BLOCK_PRODUCERS_TO_SEND_CHUNK_ENDORSEMENT)
        .map_while(|i| epoch_manager.get_block_producer(&epoch_id, block_height + i).ok())
        .collect_vec();
    assert!(!block_producers.is_empty());

    let chunk_hash = chunk_header.chunk_hash();
    tracing::debug!(
        target: "stateless_validation",
        chunk_hash=?chunk_hash,
        ?block_producers,
        "Chunk validated successfully, sending endorsement",
    );

    let endorsement = ChunkEndorsement::new(chunk_header.chunk_hash(), signer);
    for block_producer in block_producers {
        if signer.validator_id() == &block_producer {
            // Add endorsement to the cache of our chunk endorsements
            // immediately, because network won't handle message to ourselves.
            let mut guard = my_chunk_endorsements.lock();
            guard.get_or_insert(chunk_hash.clone(), || HashMap::new());
            let chunk_endorsements = guard.get_mut(&chunk_hash).unwrap();
            chunk_endorsements.insert(block_producer.clone(), endorsement.clone());
        } else {
            network_sender.send(PeerManagerMessageRequest::NetworkRequests(
                NetworkRequests::ChunkEndorsement(block_producer, endorsement.clone()),
            ));
        }
    }
}

impl Client {
    /// Responds to a network request to verify a `ChunkStateWitness`, which is
    /// sent by chunk producers after they produce a chunk.
    pub fn process_chunk_state_witness(
        &mut self,
        witness: ChunkStateWitness,
        peer_id: PeerId,
    ) -> Result<(), Error> {
        // TODO(#10502): Handle production of state witness for first chunk after genesis.
        // Properly handle case for chunk right after genesis.
        // Context: We are currently unable to handle production of the state witness for the
        // first chunk after genesis as it's not possible to run the genesis chunk in runtime.
        let prev_block_hash = witness.inner.chunk_header.prev_block_hash();
        let prev_block = self.chain.get_block(prev_block_hash)?;
        let prev_chunk_header = Chain::get_prev_chunk_header(
            self.epoch_manager.as_ref(),
            &prev_block,
            witness.inner.chunk_header.shard_id(),
        )?;
        if prev_chunk_header.prev_block_hash() == &CryptoHash::default() {
            let Some(signer) = self.validator_signer.as_ref() else {
                return Err(Error::NotAChunkValidator);
            };
            send_chunk_endorsement_to_block_producers(
                &witness.inner.chunk_header,
                self.epoch_manager.as_ref(),
                signer.as_ref(),
                &self.chunk_validator.network_sender,
                self.chunk_endorsement_tracker.chunk_endorsements.as_ref(),
            );
            return Ok(());
        }

        // TODO(#10265): If the previous block does not exist, we should
        // queue this (similar to orphans) to retry later.
        let result = self.chunk_validator.start_validating_chunk(
            witness,
            &self.chain,
            peer_id.clone(),
            &self.chunk_endorsement_tracker,
        );
        if let Err(Error::InvalidChunkStateWitness(_)) = &result {
            self.network_adapter.send(PeerManagerMessageRequest::NetworkRequests(
                NetworkRequests::BanPeer {
                    peer_id,
                    ban_reason: ReasonForBan::BadChunkStateWitness,
                },
            ));
        }
        result
    }
}