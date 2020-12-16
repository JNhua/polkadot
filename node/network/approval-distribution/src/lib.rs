// Copyright 2020 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! [`ApprovalDistributionSubsystem`] implementation.
//!
//! https://w3f.github.io/parachain-implementers-guide/node/approval/approval-distribution.html

#![warn(missing_docs)]

#[cfg(test)]
mod tests;


use std::collections::{BTreeMap, HashMap, HashSet, hash_map};
use futures::{channel::oneshot, FutureExt as _};
use polkadot_primitives::v1::{
	Hash, BlockNumber, ValidatorIndex, ValidatorSignature,
};
use polkadot_node_primitives::{
	approval::{AssignmentCert, BlockApprovalMeta, IndirectSignedApprovalVote, IndirectAssignmentCert},
};
use polkadot_node_subsystem::{
	messages::{
		AllMessages, ApprovalDistributionMessage, ApprovalVotingMessage, NetworkBridgeMessage,
		ChainApiMessage, AssignmentCheckResult, ApprovalCheckResult,
	},
	ActiveLeavesUpdate, FromOverseer, OverseerSignal, SpawnedSubsystem, Subsystem, SubsystemContext,
};
use polkadot_node_subsystem_util::metrics::{self, prometheus};
use polkadot_node_network_protocol::{
	PeerId, View, NetworkBridgeEvent, v1 as protocol_v1, ReputationChange as Rep,
};

const LOG_TARGET: &str = "approval_distribution";

// TODO: justify the numbers:
const COST_UNEXPECTED_MESSAGE: Rep = Rep::new(-100, "Peer sent an out-of-view assignment or approval");
const COST_DUPLICATE_MESSAGE: Rep = Rep::new(-100, "Peer sent identical messages");
const COST_ASSIGNMENT_TOO_FAR_IN_THE_FUTURE: Rep = Rep::new(-10, "The vote was valid but too far in the future");
const COST_INVALID_MESSAGE: Rep = Rep::new(-500, "The vote was bad");

const BENEFIT_VALID_MESSAGE: Rep = Rep::new(10, "Peer sent a valid message");
const BENEFIT_VALID_MESSAGE_FIRST: Rep = Rep::new(15, "Valid message with new information");


/// The Approval Distribution subsystem.
pub struct ApprovalDistribution {
	metrics: Metrics,
}

/// The [`State`] struct is responsible for tracking the overall state of the subsystem.
///
/// It tracks metadata about our view of the unfinalized chain,
/// which assignments and approvals we have seen, and our peers' views.
#[derive(Default)]
struct State {
	/// These three fields are used in conjunction to construct a view over the unfinalized chain.
	blocks_by_number: BTreeMap<BlockNumber, Vec<Hash>>,
	blocks: HashMap<Hash, BlockEntry>,

	/// Peer view data is partially stored here, and partially inline within the [`BlockEntry`]s
	peer_views: HashMap<PeerId, View>,
}

// TODO: Make it public and put in primitives?
type CandidateIndex = u32;

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
enum MessageFingerprint {
	Assignment(Hash, CandidateIndex, ValidatorIndex),
	Approval(Hash, CandidateIndex, ValidatorIndex),
}

#[derive(Debug, Clone, Default)]
struct Knowledge {
	known_messages: HashSet<MessageFingerprint>,
}

/// Information about blocks in our current view as well as whether peers know of them.
struct BlockEntry {
	/// Peers who we know are aware of this block and thus, the candidates within it.
	/// This maps to their knowledge of messages.
	known_by: HashMap<PeerId, Knowledge>,
	/// The number of the block.
	number: BlockNumber,
	/// The parent hash of the block.
	parent_hash: Hash,
	/// Our knowledge of messages.
	knowledge: Knowledge,
	/// A votes entry for each candidate.
	candidates: HashMap<CandidateIndex, CandidateEntry>,
}

#[derive(Debug)]
enum ApprovalState {
	Assigned(AssignmentCert),
	Approved(AssignmentCert, ValidatorSignature),
}

/// Information about candidates in the context of a particular block they are included in.
/// In other words, multiple `CandidateEntry`s may exist for the same candidate,
/// if it is included by multiple blocks - this is likely the case when there are forks.
#[derive(Debug, Default)]
struct CandidateEntry {
	approvals: HashMap<ValidatorIndex, ApprovalState>,
}

#[derive(Debug, Clone)]
enum MessageSource {
	Peer(PeerId),
	Local,
}

impl MessageSource {
	fn peer_id(&self) -> Option<PeerId> {
		match self {
			Self::Peer(id) => Some(id.clone()),
			Self::Local => None,
		}
	}
}

impl State {
	async fn handle_network_msg(
		&mut self,
		ctx: &mut impl SubsystemContext<Message = ApprovalDistributionMessage>,
		metrics: &Metrics,
		event: NetworkBridgeEvent<protocol_v1::ApprovalDistributionMessage>,
	) {
		match event {
			NetworkBridgeEvent::PeerConnected(peer_id, _role) => {
				// insert a blank view if none already present
				self.peer_views.entry(peer_id).or_default();
			}
			NetworkBridgeEvent::PeerDisconnected(peer_id) => {
				self.peer_views.remove(&peer_id);
				self.blocks.iter_mut().for_each(|(_hash, entry)| {
					entry.known_by.remove(&peer_id);
				})
			}
			NetworkBridgeEvent::PeerViewChange(peer_id, view) => {
				self.handle_peer_view_change(ctx, metrics, peer_id, view).await;
			}
			NetworkBridgeEvent::OurViewChange(view) => {
				self.handle_our_view_change(metrics, view).await;
			}
			NetworkBridgeEvent::PeerMessage(peer_id, msg) => {
				self.process_incoming_peer_message(ctx, metrics, peer_id, msg).await;
			}
		}
	}

	async fn handle_new_blocks(
		&mut self,
		ctx: &mut impl SubsystemContext<Message = ApprovalDistributionMessage>,
		metrics: &Metrics,
		metas: Vec<BlockApprovalMeta>,
	) {
		// TODO: get rid of superfluous clones
		let hashes: HashSet<Hash> = metas.iter().map(|m| m.hash.clone()).collect();
		for meta in metas.iter() {
			match self.blocks.entry(meta.hash.clone()) {
				hash_map::Entry::Vacant(entry) => {
					// TODO: can we include parent_hash in `BlockApprovalMeta`?
					let parent_hash = match request_parent_hash(ctx, meta.hash.clone()).await {
						Some(parent_hash) => parent_hash,
						None => continue,
					};

					entry.insert(BlockEntry {
						known_by: HashMap::new(),
						number: meta.number,
						parent_hash,
						knowledge: Knowledge::default(),
						candidates: HashMap::new(),
					});
				}
				_ => continue,
			}
			// TODO: how do we make sure there are no duplicates?
			self.blocks_by_number.entry(meta.number).or_default().push(meta.hash.clone());
		}
		for (peer_id, view) in self.peer_views.iter() {
			let view_set = view.heads.iter().cloned().collect::<HashSet<_>>();
			let intersection = view_set.intersection(&hashes);
			let view_intersection = View {
				heads: intersection.cloned().collect(),
				finalized_number: view.finalized_number,
			};
			Self::unify_with_peer(
				&mut self.blocks,
				ctx,
				metrics,
				peer_id.clone(),
				view_intersection,
			).await;
		}
	}

	async fn process_incoming_peer_message(
		&mut self,
		ctx: &mut impl SubsystemContext<Message = ApprovalDistributionMessage>,
		metrics: &Metrics,
		peer_id: PeerId,
		msg: protocol_v1::ApprovalDistributionMessage,
	) {
		match msg {
			protocol_v1::ApprovalDistributionMessage::Assignments(assignments) => {
				tracing::trace!(
					target: LOG_TARGET,
					peer_id = %peer_id,
					num = assignments.len(),
					"Processing assignments from a peer",
				);
				// TODO: can we batch the circulation part?
				for (assignment, claimed_index) in assignments.into_iter() {
					self.import_and_circulate_assignment(
						ctx,
						metrics,
						MessageSource::Peer(peer_id.clone()),
						assignment,
						claimed_index,
					).await;
				}
			}
			protocol_v1::ApprovalDistributionMessage::Approvals(approvals) => {
				tracing::trace!(
					target: LOG_TARGET,
					peer_id = %peer_id,
					num = approvals.len(),
					"Processing approvals from a peer",
				);
				for approval_vote in approvals.into_iter() {
					self.import_and_circulate_approval(
						ctx,
						metrics,
						MessageSource::Peer(peer_id.clone()),
						approval_vote,
					).await;
				}
			}
		}
	}

	async fn handle_peer_view_change(
		&mut self,
		ctx: &mut impl SubsystemContext<Message = ApprovalDistributionMessage>,
		metrics: &Metrics,
		peer_id: PeerId,
		view: View,
	) {
		Self::unify_with_peer(&mut self.blocks, ctx, metrics, peer_id.clone(), view.clone()).await;
		let finalized_number = view.finalized_number;
		self.peer_views.insert(peer_id.clone(), view);

		// cleanup
		let blocks = &mut self.blocks;
		self.blocks_by_number
			.range(0..=finalized_number)
			.map(|(_n, h)| h)
			.flatten()
			.for_each(|h| {
				if let Some(entry) = blocks.get_mut(h) {
					entry.known_by.remove(&peer_id);
				}
			});
	}

	async fn handle_our_view_change(
		&mut self,
		_metrics: &Metrics,
		view: View,
	) {
		// split_off returns everything after the given key, including the key
		let split_point = view.finalized_number.saturating_add(1);
		let mut old_blocks = self.blocks_by_number.split_off(&split_point);
		std::mem::swap(&mut self.blocks_by_number, &mut old_blocks);

		old_blocks.values()
			.flatten()
			.for_each(|h| {
				self.blocks.remove(h);
			});
	}

	async fn import_and_circulate_assignment(
		&mut self,
		ctx: &mut impl SubsystemContext<Message = ApprovalDistributionMessage>,
		_metrics: &Metrics,
		source: MessageSource,
		assignment: IndirectAssignmentCert,
		claimed_candidate_index: CandidateIndex,
	) {
		let block_hash = assignment.block_hash.clone();
		let validator_index = assignment.validator;

		let entry = match self.blocks.get_mut(&block_hash) {
			Some(entry) => entry,
			None => {
				if let Some(peer_id) = source.peer_id() {
					modify_reputation(ctx, peer_id, COST_UNEXPECTED_MESSAGE).await;
				}
				return;
			}
		};

		// compute a fingerprint of the assignment
		let fingerprint = MessageFingerprint::Assignment(
			block_hash,
			claimed_candidate_index,
			validator_index,
		);

		if let Some(peer_id) = source.peer_id() {
			// check if our knowledge of the peer already contains this assignment
			match entry.known_by.entry(peer_id.clone()) {
				hash_map::Entry::Occupied(knowledge) => {
					if knowledge.get().known_messages.contains(&fingerprint) {
						modify_reputation(ctx, peer_id, COST_DUPLICATE_MESSAGE).await;
						return;
					}
				}
				hash_map::Entry::Vacant(_) => {
					modify_reputation(ctx, peer_id.clone(), COST_UNEXPECTED_MESSAGE).await;
				}
			}

			// if the assignment is known to be valid, reward the peer
			if entry.knowledge.known_messages.contains(&fingerprint) {
				modify_reputation(ctx, peer_id.clone(), BENEFIT_VALID_MESSAGE).await;
				entry.known_by.entry(peer_id).or_default().known_messages.insert(fingerprint.clone());
				return;
			}

			// FIXME: possibly deadlocks due to https://github.com/paritytech/polkadot/issues/2149
			// unless ApprovalVoting does not .await for the reply
			let (tx, rx) = oneshot::channel();

			ctx.send_message(AllMessages::ApprovalVoting(ApprovalVotingMessage::CheckAndImportAssignment(
				assignment.clone(),
				tx,
			))).await;

			let result = match rx.await {
				Ok(result) => result,
				Err(_) => {
					tracing::debug!(
						target: LOG_TARGET,
						"The approval voting subsystem is down",
					);
					return;
				}
			};

			match result {
				AssignmentCheckResult::Accepted | AssignmentCheckResult::AcceptedDuplicate => {
					if result == AssignmentCheckResult::Accepted {
						modify_reputation(ctx, peer_id.clone(), BENEFIT_VALID_MESSAGE_FIRST).await;
					}
					entry.knowledge.known_messages.insert(fingerprint.clone());
					entry.known_by
						.entry(peer_id)
						.or_default()
						.known_messages
						.insert(fingerprint.clone());
				}
				AssignmentCheckResult::TooFarInFuture => {
					modify_reputation(ctx, peer_id, COST_ASSIGNMENT_TOO_FAR_IN_THE_FUTURE).await;
					return;
				}
				AssignmentCheckResult::Bad => {
					modify_reputation(ctx, peer_id, COST_INVALID_MESSAGE).await;
					return;
				}
			}
		} else {
			entry.knowledge.known_messages.insert(fingerprint.clone());
		}

		match entry.candidates.get_mut(&claimed_candidate_index) {
			Some(candidate_entry) => {
				// set the approval state for validator_index to Assigned
				// unless the approval state is set already
				candidate_entry.approvals
					.entry(validator_index)
					.or_insert_with(|| ApprovalState::Assigned(assignment.cert.clone()));
			}
			None => {
				tracing::warn!(
					target: LOG_TARGET,
					hash = ?block_hash,
					?claimed_candidate_index,
					"Expected a candidate entry on import_and_circulate_assignment",
				);
			}
		}

		// Dispatch a ApprovalDistributionV1Message::Assignment(assignment, candidate_index)
		// to all peers in the BlockEntry's known_by set,
		// excluding the peer in the source, if source has kind MessageSource::Peer.
		let maybe_peer_id = source.peer_id();
		let peers = self.peer_views
			.keys()
			.cloned()
			.filter(|key| maybe_peer_id.as_ref().map_or(true, |id| id != key))
			.collect::<Vec<_>>();

		let assignments = vec![(assignment, claimed_candidate_index)];

		ctx.send_message(NetworkBridgeMessage::SendValidationMessage(
			peers.clone(),
			protocol_v1::ValidationProtocol::ApprovalDistribution(
				protocol_v1::ApprovalDistributionMessage::Assignments(assignments)
			),
		).into()).await;

		// Add the fingerprint of the assignment to the knowledge of each peer.
		for peer in peers.into_iter() {
			entry.known_by
				.entry(peer)
				.or_default()
				.known_messages
				.insert(fingerprint.clone());
		}
	}

	async fn import_and_circulate_approval(
		&mut self,
		ctx: &mut impl SubsystemContext<Message = ApprovalDistributionMessage>,
		_metrics: &Metrics,
		source: MessageSource,
		vote: IndirectSignedApprovalVote,
	) {
		let block_hash = vote.block_hash.clone();
		let validator_index = vote.validator;
		let candidate_index = vote.candidate_index;

		let entry = match self.blocks.get_mut(&block_hash) {
			Some(entry) if entry.candidates.contains_key(&candidate_index) => entry,
			_ => {
				if let Some(peer_id) = source.peer_id() {
					modify_reputation(ctx, peer_id, COST_UNEXPECTED_MESSAGE).await;
				}
				return;
			}
		};

		// compute a fingerprint of the approval
		let fingerprint = MessageFingerprint::Approval(
			block_hash.clone(),
			candidate_index,
			validator_index,
		);

		if let Some(peer_id) = source.peer_id() {
			let assignment_fingerprint = MessageFingerprint::Assignment(
				block_hash.clone(),
				candidate_index,
				validator_index,
			);

			if !entry.knowledge.known_messages.contains(&assignment_fingerprint) {
				modify_reputation(ctx, peer_id, COST_UNEXPECTED_MESSAGE).await;
				return;
			}

			// check if our knowledge of the peer already contains this assignment
			match entry.known_by.entry(peer_id.clone()) {
				hash_map::Entry::Occupied(knowledge) => {
					if knowledge.get().known_messages.contains(&fingerprint) {
						modify_reputation(ctx, peer_id, COST_DUPLICATE_MESSAGE).await;
						return;
					}
				}
				hash_map::Entry::Vacant(_) => {
					modify_reputation(ctx, peer_id.clone(), COST_UNEXPECTED_MESSAGE).await;
				}
			}

			// if the assignment is known to be valid, reward the peer
			if entry.knowledge.known_messages.contains(&fingerprint) {
				modify_reputation(ctx, peer_id.clone(), BENEFIT_VALID_MESSAGE).await;
				entry.known_by.entry(peer_id).or_default().known_messages.insert(fingerprint.clone());
				return;
			}

			// FIXME: possibly deadlocks due to https://github.com/paritytech/polkadot/issues/2149
			let (tx, rx) = oneshot::channel();

			ctx.send_message(AllMessages::ApprovalVoting(ApprovalVotingMessage::CheckAndImportApproval(
				vote.clone(),
				tx,
			))).await;

			let result = match rx.await {
				Ok(result) => result,
				Err(_) => {
					tracing::debug!(
						target: LOG_TARGET,
						"The approval voting subsystem is down",
					);
					return;
				}
			};

			match result {
				ApprovalCheckResult::Accepted => {
					modify_reputation(ctx, peer_id.clone(), BENEFIT_VALID_MESSAGE_FIRST).await;

					entry.knowledge.known_messages.insert(fingerprint.clone());
					entry.known_by
						.entry(peer_id)
						.or_default()
						.known_messages
						.insert(fingerprint.clone());
				}
				ApprovalCheckResult::Bad => {
					modify_reputation(ctx, peer_id, COST_INVALID_MESSAGE).await;
					return;
				}
			}
		} else {
			entry.knowledge.known_messages.insert(fingerprint.clone());
		}

		match entry.candidates.get_mut(&candidate_index) {
			Some(candidate_entry) => {
				// set the approval state for validator_index to Approved
				// it should be in assigned state already
				match candidate_entry.approvals.remove(&validator_index) {
					Some(ApprovalState::Assigned(cert)) => {
						candidate_entry.approvals.insert(
							validator_index,
							ApprovalState::Approved(cert, vote.signature.clone()),
						);
					}
					_ => {
						tracing::warn!(
							target: LOG_TARGET,
							hash = ?block_hash,
							?candidate_index,
							"Expected a candidate entry with `ApprovalState::Assigned`",
						);
					}
				}
			}
			None => {
				tracing::warn!(
					target: LOG_TARGET,
					hash = ?block_hash,
					?candidate_index,
					"Expected a candidate entry on import_and_circulate_approval",
				);
			}
		}

		// Dispatch a ApprovalDistributionV1Message::Approval(vote)
		// to all peers in the BlockEntry's known_by set,
		// excluding the peer in the source, if source has kind MessageSource::Peer.
		let maybe_peer_id = source.peer_id();
		let peers = self.peer_views
			.keys()
			.cloned()
			.filter(|key| maybe_peer_id.as_ref().map_or(true, |id| id != key))
			.collect::<Vec<_>>();

		let approvals = vec![vote];

		ctx.send_message(NetworkBridgeMessage::SendValidationMessage(
			peers.clone(),
			protocol_v1::ValidationProtocol::ApprovalDistribution(
				protocol_v1::ApprovalDistributionMessage::Approvals(approvals)
			),
		).into()).await;

		// Add the fingerprint of the assignment to the knowledge of each peer.
		for peer in peers.into_iter() {
			entry.known_by
				.entry(peer)
				.or_default()
				.known_messages
				.insert(fingerprint.clone());
		}
	}

	async fn unify_with_peer(
		entries: &mut HashMap<Hash, BlockEntry>,
		ctx: &mut impl SubsystemContext<Message = ApprovalDistributionMessage>,
		metrics: &Metrics,
		peer_id: PeerId,
		view: View,
	) {
		let mut to_send = HashSet::new();

		let view_finalized_number = view.finalized_number;
		for head in view.heads.into_iter() {
			let mut block = head;
			let interesting_blocks = std::iter::from_fn(|| {
				// step 2.
				let entry = match entries.get_mut(&block) {
					Some(entry) if entry.number >= view_finalized_number => entry,
					_ => return None,
				};
				let interesting_block = match entry.known_by.entry(peer_id.clone()) {
					// step 3.
					hash_map::Entry::Occupied(_) => return None,
					// step 4.
					hash_map::Entry::Vacant(vacant) => {
						vacant.insert(entry.knowledge.clone());
						block
					}
				};
				// step 5.
				block = entry.parent_hash.clone();
				Some(interesting_block)
			});
			to_send.extend(interesting_blocks);
		}
		// step 6.
		// send all assignments and approvals for all candidates in those blocks to the peer
		Self::send_gossip_messages_to_peer(
			entries,
			ctx,
			metrics,
			peer_id,
			to_send
		).await;
	}

	#[tracing::instrument(level = "trace", skip(entries, ctx, _metrics, blocks), fields(subsystem = LOG_TARGET))]
	async fn send_gossip_messages_to_peer(
		entries: &HashMap<Hash, BlockEntry>,
		ctx: &mut impl SubsystemContext<Message = ApprovalDistributionMessage>,
		_metrics: &Metrics,
		peer_id: PeerId,
		blocks: HashSet<Hash>,
	) {
		let mut assignments = Vec::new();
		let mut approvals = Vec::new();

		for block in blocks.into_iter() {
			let entry = match entries.get(&block) {
				Some(entry) => entry,
				None => continue, // should be unreachable
			};
			for (candidate_index, candidate_entry) in entry.candidates.iter() {
				for (validator_index, approval_state) in candidate_entry.approvals.iter() {
					match approval_state {
						ApprovalState::Assigned(cert) => {
							assignments.push((IndirectAssignmentCert {
								block_hash: block.clone(),
								validator: validator_index.clone(),
								cert: cert.clone(),
							}, candidate_index.clone()));
						}
						ApprovalState::Approved(_, signature) => {
							approvals.push(IndirectSignedApprovalVote {
								block_hash: block.clone(),
								validator: validator_index.clone(),
								candidate_index: candidate_index.clone(),
								signature: signature.clone(),
							});
						}
					}
				}
			}
		}

		if !assignments.is_empty() {
			ctx.send_message(NetworkBridgeMessage::SendValidationMessage(
				vec![peer_id.clone()],
				protocol_v1::ValidationProtocol::ApprovalDistribution(
					protocol_v1::ApprovalDistributionMessage::Assignments(assignments)
				),
			).into()).await;
		}

		if !approvals.is_empty() {
			ctx.send_message(NetworkBridgeMessage::SendValidationMessage(
				vec![peer_id],
				protocol_v1::ValidationProtocol::ApprovalDistribution(
					protocol_v1::ApprovalDistributionMessage::Approvals(approvals)
				),
			).into()).await;
		}
	}
}


/// Modify the reputation of a peer based on its behavior.
#[tracing::instrument(level = "trace", skip(ctx), fields(subsystem = LOG_TARGET))]
async fn modify_reputation(
	ctx: &mut impl SubsystemContext<Message = ApprovalDistributionMessage>,
	peer_id: PeerId,
	rep: Rep,
) {
	tracing::trace!(
		target: LOG_TARGET,
		reputation = ?rep,
		?peer_id,
		"Reputation change for peer",
	);

	ctx.send_message(AllMessages::NetworkBridge(
		NetworkBridgeMessage::ReportPeer(peer_id, rep),
	)).await;
}

async fn request_parent_hash(
	ctx: &mut impl SubsystemContext<Message = ApprovalDistributionMessage>,
	block_hash: Hash,
) -> Option<Hash> {
	let (tx, rx) = oneshot::channel();

	ctx.send_message(AllMessages::from(ChainApiMessage::BlockHeader(
		block_hash,
		tx,
	)).into()).await;

	// Make sure this is really OK
	rx.await.ok()?.ok()?.map(|h| h.parent_hash)
}

impl ApprovalDistribution {
	/// Create a new instance of the [`ApprovalDistribution`] subsystem.
	pub fn new(metrics: Metrics) -> Self {
		Self { metrics }
	}

	#[tracing::instrument(skip(self, ctx), fields(subsystem = LOG_TARGET))]
	async fn run<Context>(self, mut ctx: Context)
	where
		Context: SubsystemContext<Message = ApprovalDistributionMessage>,
	{
		let mut state = State::default();
		loop {
			let message = match ctx.recv().await {
				Ok(message) => message,
				Err(e) => {
					tracing::debug!(target: LOG_TARGET, err = ?e, "Failed to receive a message from Overseer, exiting");
					return;
				},
			};
			match message {
				FromOverseer::Communication {
					msg: ApprovalDistributionMessage::NetworkBridgeUpdateV1(event),
				} => {
					tracing::debug!(target: LOG_TARGET, "Processing network message");
					state.handle_network_msg(&mut ctx, &self.metrics, event).await;
				}
				FromOverseer::Communication {
					msg: ApprovalDistributionMessage::NewBlocks(metas),
				} => {
					tracing::debug!(target: LOG_TARGET, "Processing NewBlocks");
					state.handle_new_blocks(&mut ctx, &self.metrics, metas).await;
				}
				FromOverseer::Communication {
					msg: ApprovalDistributionMessage::DistributeAssignment(cert, candidate_index),
				} => {
					tracing::debug!(target: LOG_TARGET, "Processing DistributeAssignment");
					state.import_and_circulate_assignment(
						&mut ctx,
						&self.metrics,
						MessageSource::Local,
						cert,
						candidate_index,
					).await;
				}
				FromOverseer::Communication {
					msg: ApprovalDistributionMessage::DistributeApproval(vote),
				} => {
					tracing::debug!(target: LOG_TARGET, "Processing DistributeApproval");
					state.import_and_circulate_approval(
						&mut ctx,
						&self.metrics,
						MessageSource::Local,
						vote,
					).await;
				}
				FromOverseer::Signal(OverseerSignal::ActiveLeaves(ActiveLeavesUpdate { .. })) => {
					tracing::trace!(target: LOG_TARGET, "active leaves signal (ignored)");
					// handled by NewBlocks
				}
				FromOverseer::Signal(OverseerSignal::BlockFinalized(_hash, number)) => {
					tracing::trace!(target: LOG_TARGET, number = %number, "finalized signal (ignored)");
					// handled by our handle_our_view_change

				},
				FromOverseer::Signal(OverseerSignal::Conclude) => {
					return;
				}
			}
		}
	}
}

impl<C> Subsystem<C> for ApprovalDistribution
where
	C: SubsystemContext<Message = ApprovalDistributionMessage> + Sync + Send,
{
	fn start(self, ctx: C) -> SpawnedSubsystem {
		let future = self.run(ctx)
			.map(|_| Ok(()))
			.boxed();

		SpawnedSubsystem {
			name: "approval-distribution-subsystem",
			future,
		}
	}
}


/// Approval Distribution metrics.
#[derive(Default, Clone)]
pub struct Metrics(Option<MetricsInner>);

#[derive(Clone)]
struct MetricsInner {
}

impl metrics::Metrics for Metrics {
	fn try_register(_registry: &prometheus::Registry) -> Result<Self, prometheus::PrometheusError> {
		Ok(Metrics::default())
	}
}