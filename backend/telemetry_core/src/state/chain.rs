// Source code for the Substrate Telemetry Server.
// Copyright (C) 2021 Parity Technologies (UK) Ltd.
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use common::node_message::Payload;
use common::node_types::BlockHash;
use common::node_types::{Block, Timestamp};
use common::{id_type, time, DenseMap, MostSeen, NumStats};
use once_cell::sync::Lazy;
use std::collections::HashSet;
use std::str::FromStr;
use std::time::{Duration, Instant};

use crate::feed_message::{self, ChainStats, FeedMessageSerializer};
use crate::find_location;

use super::chain_stats::ChainStatsCollator;
use super::counter::CounterValue;
use super::node::Node;

id_type! {
    /// A Node ID that is unique to the chain it's in.
    pub struct ChainNodeId(usize)
}

pub type Label = Box<str>;

const STALE_TIMEOUT: u64 = 2 * 60 * 1000; // 2 minutes
const STATS_UPDATE_INTERVAL: Duration = Duration::from_secs(5);

pub struct Chain {
    /// Labels that nodes use for this chain. We keep track of
    /// the most commonly used label as nodes are added/removed.
    labels: MostSeen<Label>,
    /// Set of nodes that are in this chain
    nodes: DenseMap<ChainNodeId, Node>,
    /// Best block
    best: Block,
    /// Finalized block
    finalized: Block,
    /// Block times history, stored so we can calculate averages
    block_times: NumStats<u64>,
    /// Calculated average block time
    average_block_time: Option<u64>,
    /// When the best block first arrived
    timestamp: Option<Timestamp>,
    /// Genesis hash of this chain
    genesis_hash: BlockHash,
    /// Maximum number of nodes allowed to connect from this chain
    max_nodes: usize,
    /// Collator for the stats.
    stats_collator: ChainStatsCollator,
    /// Stats for this chain.
    stats: ChainStats,
    /// Timestamp of when the stats were last regenerated.
    stats_last_regenerated: Instant,
}

pub enum AddNodeResult {
    Overquota,
    Added {
        id: ChainNodeId,
        chain_renamed: bool,
    },
}

pub struct RemoveNodeResult {
    pub chain_renamed: bool,
}

/// Genesis hashes of chains we consider "first party". These chains allow any
/// number of nodes to connect.
static FIRST_PARTY_NETWORKS: Lazy<HashSet<BlockHash>> = Lazy::new(|| {
    let genesis_hash_strs = &[
        "0x1459b0204b92719ffc978c5da3d6a2057973916bd548f8076df2064bc1cb4cfc", // Mainnet
        "0x6d04f01a398a0de6466f7e3d8300e81fb1e5e8428a48ac4975469e90bedb96b6", // Testnet
    ];

    genesis_hash_strs
        .iter()
        .map(|h| BlockHash::from_str(h).expect("hardcoded hash str should be valid"))
        .collect()
});

/// When we construct a chain, we want to check to see whether or not it's a "first party"
/// network first, and assign a `max_nodes` accordingly. This helps us do that.
pub fn is_first_party_network(genesis_hash: &BlockHash) -> bool {
    FIRST_PARTY_NETWORKS.contains(genesis_hash)
}

impl Chain {
    /// Create a new chain with an initial label.
    pub fn new(genesis_hash: BlockHash, max_nodes: usize) -> Self {
        Chain {
            labels: MostSeen::default(),
            nodes: DenseMap::new(),
            best: Block::zero(),
            finalized: Block::zero(),
            block_times: NumStats::new(50),
            average_block_time: None,
            timestamp: None,
            genesis_hash,
            max_nodes,
            stats_collator: Default::default(),
            stats: Default::default(),
            stats_last_regenerated: Instant::now(),
        }
    }

    /// Is the chain the node belongs to overquota?
    pub fn is_overquota(&self) -> bool {
        self.nodes.len() >= self.max_nodes
    }

    /// Assign a node to this chain.
    pub fn add_node(&mut self, node: Node) -> AddNodeResult {
        if self.is_overquota() {
            return AddNodeResult::Overquota;
        }

        let details = node.details();
        self.stats_collator
            .add_or_remove_node(details, None, CounterValue::Increment);

        let node_chain_label = &details.chain;
        let label_result = self.labels.insert(node_chain_label);
        let node_id = self.nodes.add(node);

        AddNodeResult::Added {
            id: node_id,
            chain_renamed: label_result.has_changed(),
        }
    }

    /// Remove a node from this chain.
    pub fn remove_node(&mut self, node_id: ChainNodeId) -> RemoveNodeResult {
        let node = match self.nodes.remove(node_id) {
            Some(node) => node,
            None => {
                return RemoveNodeResult {
                    chain_renamed: false,
                }
            }
        };

        let details = node.details();
        self.stats_collator
            .add_or_remove_node(details, node.hwbench(), CounterValue::Decrement);

        let node_chain_label = &node.details().chain;
        let label_result = self.labels.remove(node_chain_label);

        RemoveNodeResult {
            chain_renamed: label_result.has_changed(),
        }
    }

    /// Attempt to update the best block seen in this chain.
    pub fn update_node(
        &mut self,
        nid: ChainNodeId,
        payload: Payload,
        feed: &mut FeedMessageSerializer,
        expose_node_details: bool,
    ) {
        if let Some(block) = payload.best_block() {
            self.handle_block(block, nid, feed);
        }

        if let Some(node) = self.nodes.get_mut(nid) {
            match payload {
                Payload::SystemInterval(ref interval) => {
                    // Send a feed message if any of the relevant node details change:
                    if node.update_hardware(interval) {
                        feed.push(feed_message::Hardware(nid.into(), node.hardware()));
                    }
                    if let Some(stats) = node.update_stats(interval) {
                        feed.push(feed_message::NodeStatsUpdate(nid.into(), stats));
                    }
                    if let Some(io) = node.update_io(interval) {
                        feed.push(feed_message::NodeIOUpdate(nid.into(), io));
                    }
                }
                Payload::AfgAuthoritySet(authority) => {
                    // If our node validator address (and thus details) change, send an
                    // updated "add node" feed message:
                    if node.set_validator_address(authority.authority_id.clone()) {
                        feed.push(feed_message::AddedNode(
                            nid.into(),
                            &node,
                            expose_node_details,
                        ));
                    }
                    return;
                }
                Payload::HwBench(ref hwbench) => {
                    let new_hwbench = common::node_types::NodeHwBench {
                        cpu_hashrate_score: hwbench.cpu_hashrate_score,
                        memory_memcpy_score: hwbench.memory_memcpy_score,
                        disk_sequential_write_score: hwbench.disk_sequential_write_score,
                        disk_random_write_score: hwbench.disk_random_write_score,
                    };
                    let old_hwbench = node.update_hwbench(new_hwbench);
                    // The `hwbench` for this node has changed, send an updated "add node".
                    // Note: There is no need to send this message if the details
                    // will not be serialized over the wire.
                    if expose_node_details {
                        feed.push(feed_message::AddedNode(
                            nid.into(),
                            &node,
                            expose_node_details,
                        ));
                    }

                    self.stats_collator
                        .update_hwbench(old_hwbench.as_ref(), CounterValue::Decrement);
                    self.stats_collator
                        .update_hwbench(node.hwbench(), CounterValue::Increment);
                }
                _ => {}
            }

            if let Some(block) = payload.finalized_block() {
                if let Some(finalized) = node.update_finalized(block) {
                    feed.push(feed_message::FinalizedBlock(
                        nid.into(),
                        finalized.height,
                        finalized.hash,
                    ));

                    if finalized.height > self.finalized.height {
                        self.finalized = *finalized;
                        feed.push(feed_message::BestFinalized(
                            finalized.height,
                            finalized.hash,
                        ));
                    }
                }
            }
        }
    }

    fn handle_block(&mut self, block: &Block, nid: ChainNodeId, feed: &mut FeedMessageSerializer) {
        let mut propagation_time = None;
        let now = time::now();
        let nodes_len = self.nodes.len();

        self.update_stale_nodes(now, feed);
        self.regenerate_stats_if_necessary(feed);

        let node = match self.nodes.get_mut(nid) {
            Some(node) => node,
            None => return,
        };

        if node.update_block(*block) {
            if block.height > self.best.height {
                self.best = *block;
                log::debug!(
                    "[{}] [nodes={}] new best block={}/{:?}",
                    self.labels.best(),
                    nodes_len,
                    self.best.height,
                    self.best.hash,
                );
                if let Some(timestamp) = self.timestamp {
                    self.block_times.push(now.saturating_sub(timestamp));
                    self.average_block_time = Some(self.block_times.average());
                }
                self.timestamp = Some(now);
                feed.push(feed_message::BestBlock(
                    self.best.height,
                    now,
                    self.average_block_time,
                ));
                propagation_time = Some(0);
            } else if block.height == self.best.height {
                if let Some(timestamp) = self.timestamp {
                    propagation_time = Some(now.saturating_sub(timestamp));
                }
            }

            if let Some(details) = node.update_details(now, propagation_time) {
                feed.push(feed_message::ImportedBlock(nid.into(), details));
            }
        }
    }

    /// Check if the chain is stale (has not received a new best block in a while).
    /// If so, find a new best block, ignoring any stale nodes and marking them as such.
    fn update_stale_nodes(&mut self, now: u64, feed: &mut FeedMessageSerializer) {
        let threshold = now - STALE_TIMEOUT;
        let timestamp = match self.timestamp {
            Some(ts) => ts,
            None => return,
        };

        if timestamp > threshold {
            // Timestamp is in range, nothing to do
            return;
        }

        let mut best = Block::zero();
        let mut finalized = Block::zero();
        let mut timestamp = None;

        for (nid, node) in self.nodes.iter_mut() {
            if !node.update_stale(threshold) {
                if node.best().height > best.height {
                    best = *node.best();
                    timestamp = Some(node.best_timestamp());
                }

                if node.finalized().height > finalized.height {
                    finalized = *node.finalized();
                }
            } else {
                feed.push(feed_message::StaleNode(nid.into()));
            }
        }

        if self.best.height != 0 || self.finalized.height != 0 {
            self.best = best;
            self.finalized = finalized;
            self.block_times.reset();
            self.timestamp = timestamp;

            feed.push(feed_message::BestBlock(
                self.best.height,
                timestamp.unwrap_or(now),
                None,
            ));
            feed.push(feed_message::BestFinalized(
                finalized.height,
                finalized.hash,
            ));
        }
    }

    fn regenerate_stats_if_necessary(&mut self, feed: &mut FeedMessageSerializer) {
        let now = Instant::now();
        let elapsed = now - self.stats_last_regenerated;
        if elapsed < STATS_UPDATE_INTERVAL {
            return;
        }

        self.stats_last_regenerated = now;
        let new_stats = self.stats_collator.generate();
        if new_stats != self.stats {
            self.stats = new_stats;
            feed.push(feed_message::ChainStatsUpdate(&self.stats));
        }
    }

    pub fn update_node_location(
        &mut self,
        node_id: ChainNodeId,
        location: find_location::Location,
    ) -> bool {
        if let Some(node) = self.nodes.get_mut(node_id) {
            node.update_location(location);
            true
        } else {
            false
        }
    }

    pub fn get_node(&self, id: ChainNodeId) -> Option<&Node> {
        self.nodes.get(id)
    }
    pub fn nodes_slice(&self) -> &[Option<Node>] {
        self.nodes.as_slice()
    }
    pub fn label(&self) -> &str {
        &self.labels.best()
    }
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }
    pub fn best_block(&self) -> &Block {
        &self.best
    }
    pub fn timestamp(&self) -> Option<Timestamp> {
        self.timestamp
    }
    pub fn average_block_time(&self) -> Option<u64> {
        self.average_block_time
    }
    pub fn finalized_block(&self) -> &Block {
        &self.finalized
    }
    pub fn genesis_hash(&self) -> BlockHash {
        self.genesis_hash
    }
    pub fn stats(&self) -> &ChainStats {
        &self.stats
    }
}
