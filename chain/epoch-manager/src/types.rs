use borsh::{BorshDeserialize, BorshSerialize};
use unc_primitives::block_header::BlockHeader;
use unc_primitives::challenge::SlashedValidator;
use unc_primitives::epoch_manager::block_info::BlockInfo;
use unc_primitives::epoch_manager::epoch_info::EpochInfo;
use unc_primitives::hash::CryptoHash;
use unc_primitives::types::validator_power::ValidatorPower;
use unc_primitives::types::{
    AccountId, Balance, BlockHeight, EpochId, ShardId, ValidatorId, ValidatorStats,
};
use unc_primitives::version::ProtocolVersion;
use std::collections::{BTreeMap, HashMap};
use tracing::{debug, debug_span};
use unc_primitives::types::validator_stake::ValidatorPledge;

use crate::EpochManager;

pub type RngSeed = [u8; 32];

/// Compressed information about block.
/// Useful for epoch manager.
#[derive(Default, Clone, Debug)]
pub struct BlockHeaderInfo {
    pub hash: CryptoHash,
    pub prev_hash: CryptoHash,
    pub height: BlockHeight,
    pub random_value: CryptoHash,
    pub last_finalized_height: BlockHeight,
    pub last_finalized_block_hash: CryptoHash,
    pub power_proposals: Vec<ValidatorPower>,
    pub pledge_proposals: Vec<ValidatorPledge>,
    pub slashed_validators: Vec<SlashedValidator>,
    pub chunk_mask: Vec<bool>,
    pub total_supply: Balance,
    pub latest_protocol_version: ProtocolVersion,
    pub timestamp_nanosec: u64,
}

impl BlockHeaderInfo {
    pub fn new(header: &BlockHeader, last_finalized_height: u64) -> Self {
        Self {
            hash: *header.hash(),
            prev_hash: *header.prev_hash(),
            height: header.height(),
            random_value: *header.random_value(),
            last_finalized_height,
            last_finalized_block_hash: *header.last_final_block(),
            power_proposals: header.prev_validator_power_proposals().collect(),
            pledge_proposals: header.prev_validator_pledge_proposals().collect(),
            slashed_validators: vec![],
            chunk_mask: header.chunk_mask().to_vec(),
            total_supply: header.total_supply(),
            latest_protocol_version: header.latest_protocol_version(),
            timestamp_nanosec: header.raw_timestamp(),
        }
    }
}

/// Aggregator of information needed for validator computation at the end of the epoch.
#[derive(Clone, BorshSerialize, BorshDeserialize, Debug, Default)]
pub struct EpochInfoAggregator {
    /// Map from validator index to (num_blocks_produced, num_blocks_expected) so far in the given epoch.
    pub block_tracker: HashMap<ValidatorId, ValidatorStats>,
    /// For each shard, a map of validator id to (num_chunks_produced, num_chunks_expected) so far in the given epoch.
    pub shard_tracker: HashMap<ShardId, HashMap<ValidatorId, ValidatorStats>>,
    /// Latest protocol version that each validator supports.
    pub version_tracker: HashMap<ValidatorId, ProtocolVersion>,
    /// All power proposals in this epoch up to this block.
    pub all_power_proposals: BTreeMap<AccountId, ValidatorPower>,
    /// All pledge proposals in this epoch up to this block.
    pub all_pledge_proposals: BTreeMap<AccountId, ValidatorPledge>,
    /// Id of the epoch that this aggregator is in.
    pub epoch_id: EpochId,
    /// Last block hash recorded.
    pub last_block_hash: CryptoHash,
}

impl EpochInfoAggregator {
    pub fn new(epoch_id: EpochId, last_block_hash: CryptoHash) -> Self {
        Self {
            block_tracker: Default::default(),
            shard_tracker: Default::default(),
            version_tracker: Default::default(),
            all_power_proposals: BTreeMap::default(),
            all_pledge_proposals: BTreeMap::default(),
            epoch_id,
            last_block_hash,
        }
    }

    /// Aggregates data from a block which directly precede the first block this
    /// aggregator has statistic on.
    ///
    /// For example, this method can be used in the following situation (where
    /// A through G are blocks ordered in increasing height and arrows denote
    /// ‘is parent’ relationship and B is start of a new epoch):
    ///
    /// ```text
    ///     ┌─ block_info
    /// A ← B ← C ← D ← E ← F ← G ← H ← I
    ///         │←── self ─→│
    /// ```
    ///
    /// Note that there is no method which allows adding information about G,
    /// H or I blocks into the aggregator.  The expected usage is to create
    /// a new aggregator starting from I, add H and G into it (using this
    /// method) and then [merge][`Self::merge`] it into `self`.
    pub fn update_tail(
        &mut self,
        block_info: &BlockInfo,
        epoch_info: &EpochInfo,
        prev_block_height: BlockHeight,
    ) {
        let _span =
            debug_span!(target: "epoch_tracker", "update_tail", prev_block_height).entered();
        // Step 1: update block tracer
        let block_info_height = block_info.height();
        for height in prev_block_height + 1..=block_info_height {
            let block_producer_id = EpochManager::block_producer_from_info(epoch_info, height);
            let entry = self.block_tracker.entry(block_producer_id);
            if height == block_info_height {
                entry
                    .and_modify(|validator_stats| {
                        validator_stats.produced += 1;
                        validator_stats.expected += 1;
                    })
                    .or_insert(ValidatorStats { produced: 1, expected: 1 });
            } else {
                debug!(
                    target: "epoch_tracker",
                    block_producer = ?epoch_info.validator_account_id(block_producer_id),
                    block_height = height, "Missed block");
                entry
                    .and_modify(|validator_stats| {
                        validator_stats.expected += 1;
                    })
                    .or_insert(ValidatorStats { produced: 0, expected: 1 });
            }
        }

        // Step 2: update shard tracker
        for (i, mask) in block_info.chunk_mask().iter().enumerate() {
            let chunk_validator_id = EpochManager::chunk_producer_from_info(
                epoch_info,
                prev_block_height + 1,
                i as ShardId,
            );
            let tracker = self.shard_tracker.entry(i as ShardId).or_insert_with(HashMap::new);
            tracker
                .entry(chunk_validator_id)
                .and_modify(|stats| {
                    if *mask {
                        stats.produced += 1;
                    } else {
                        debug!(
                            target: "epoch_tracker",
                            chunk_validator = ?epoch_info.validator_account_id(chunk_validator_id),
                            shard_id = i,
                            block_height = prev_block_height + 1,
                            "Missed chunk");
                    }
                    stats.expected += 1;
                })
                .or_insert(ValidatorStats { produced: u64::from(*mask), expected: 1 });
        }

        // Step 3: update version tracker
        let block_producer_id =
            EpochManager::block_producer_from_info(epoch_info, block_info_height);
        self.version_tracker
            .entry(block_producer_id)
            .or_insert_with(|| *block_info.latest_protocol_version());

        // Step 4: update proposals
        for proposal in block_info.power_proposals_iter() {
            self.all_power_proposals.entry(proposal.account_id().clone()).or_insert(proposal);
        }

        for proposal in block_info.pledge_proposals_iter() {
            self.all_pledge_proposals.entry(proposal.account_id().clone()).or_insert(proposal);
        }
    }

    /// Merges information from `other` aggregator into `self`.
    ///
    /// The `other` aggregator must hold statistics from blocks which **follow**
    /// the ones this aggregator has.  Both aggregators have to be for the same
    /// epoch (the function panics if they aren’t).
    ///
    /// For example, this method can be used in the following situation (where
    /// A through J are blocks ordered in increasing height, arrows denote ‘is
    /// parent’ relationship and B is start of a new epoch):
    ///
    /// ```text
    ///     │←─── self ────→│   │←─ other ─→│
    /// A ← B ← C ← D ← E ← F ← G ← H ← I ← J
    ///     └── new epoch ──→
    /// ```
    ///
    /// Once the method finishes `self` will hold statistics for blocks from
    /// B till J.
    pub fn merge(&mut self, other: EpochInfoAggregator) {
        self.merge_common(&other);

        // merge version tracker
        self.version_tracker.extend(other.version_tracker);
        // merge proposals
        self.all_power_proposals.extend(other.all_power_proposals);

        self.all_pledge_proposals.extend(other.all_pledge_proposals);

        self.last_block_hash = other.last_block_hash;
    }

    /// Merges information from `other` aggregator into `self`.
    ///
    /// The `other` aggregator must hold statistics from blocks which
    /// **precede** the ones this aggregator has.  Both aggregators have to be
    /// for the same epoch (the function panics if they aren’t).
    ///
    /// For example, this method can be used in the following situation (where
    /// A through J are blocks ordered in increasing height, arrows denote ‘is
    /// parent’ relationship and B is start of a new epoch):
    ///
    /// ```text
    ///     │←─── other ───→│   │←─ self ──→│
    /// A ← B ← C ← D ← E ← F ← G ← H ← I ← J
    ///     └── new epoch ──→
    /// ```
    ///
    /// Once the method finishes `self` will hold statistics for blocks from
    /// B till J.
    ///
    /// The method is a bit like doing `other.merge(self)` except that `other`
    /// is not changed.
    pub fn merge_prefix(&mut self, other: &EpochInfoAggregator) {
        self.merge_common(&other);

        // merge version tracker
        self.version_tracker.reserve(other.version_tracker.len());
        // TODO(mina86): Use try_insert once map_try_insert is stabilised.
        for (k, v) in other.version_tracker.iter() {
            self.version_tracker.entry(*k).or_insert_with(|| *v);
        }

        // merge proposals
        // TODO(mina86): Use try_insert once map_try_insert is stabilised.
        for (k, v) in other.all_power_proposals.iter() {
            self.all_power_proposals.entry(k.clone()).or_insert_with(|| v.clone());
        }

        for (k, v) in other.all_pledge_proposals.iter() {
            self.all_pledge_proposals.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }

    /// Merges block and shard trackers from `other` into `self`.
    ///
    /// See [`Self::merge`] and [`Self::merge_prefix`] method for description of
    /// merging.
    fn merge_common(&mut self, other: &EpochInfoAggregator) {
        assert_eq!(self.epoch_id, other.epoch_id);

        // merge block tracker
        for (block_producer_id, stats) in other.block_tracker.iter() {
            self.block_tracker
                .entry(*block_producer_id)
                .and_modify(|e| {
                    e.expected += stats.expected;
                    e.produced += stats.produced
                })
                .or_insert_with(|| stats.clone());
        }
        // merge shard tracker
        for (shard_id, stats) in other.shard_tracker.iter() {
            self.shard_tracker
                .entry(*shard_id)
                .and_modify(|e| {
                    for (chunk_producer_id, stat) in stats.iter() {
                        e.entry(*chunk_producer_id)
                            .and_modify(|entry| {
                                entry.expected += stat.expected;
                                entry.produced += stat.produced;
                            })
                            .or_insert_with(|| stat.clone());
                    }
                })
                .or_insert_with(|| stats.clone());
        }
    }
}
