//! The download manager maintains data structures on adverts and download state
//! per peer.
//!
//! A replica connects to all peers that are listed in the current subnet record
//! or the subnet record preceding the current record.
//! For each connected peer, the peer manager manages the list
//! of adverts and ongoing chunk downloads.
//!
//! The following data structures represent the state of the download manager
//! and all ongoing download activity.
//!
//! ```text
//!   +---------------------+
//!   |Download manager     |
//!   |----------- ---------|   +------------------+    +----------+     +----------+
//!   |UnderconstructionList|-->|Artifact          |....|Artifact  |.....|Artifact  |
//!   |                     |   |Download          |    |Download  |     |Download  |
//!   |                     |   |Tracker#1         |    |Tracker#2 |     |Tracker#N |
//!   |                     |   |requested_instant |    |          |     |          |
//!   |                     |   +------------------+    +----------+     +----------+
//!   |                     |
//!   |                     |
//!   |          PeerList   |-->+-------------------------------------------+---> ....
//!   |                     |   |Peer#1  (PeerContext)                      |
//!   |                     |   +-------------------------------------------+
//!   +---------------------+   |Peer In-flight Chunk "requested" map       |
//!                             +-----------------------+-------------------+
//!                             |Key                    |Value              |
//!                             +-----------------------+-------------------+
//!                             |ArtifactID +ChunkID    |requested_instant  |
//!                             +-----------------------+-------------------+
//!                             |...                    |                   |
//!                             +-----------------------+-------------------+
//! ```
//!
//! Locking is required to protect the data structures. The locking Hierarchy
//! is as follows:
//! 1) Peer context lock
//! 2) Under construction list lock
//! 3) Prioritizer Lock (R/W)
//! 4) Advert tracker Lock (R/W)
//!
//! Note that only the `download_next_compute_work()` workflow
//! *requires* the acquisition of multiple/all locks in the correct order:
//!
//! 1) Peer context lock to update the peer context "requested" list.
//! 2) Under construction list lock to add new artifacts being constructed.
//! 3) Prioritizer lock to iterate over adverts that are eligible for download.
//! 4) Advert tracker lock to mark a download attempt on a advert.
//!
//! All other workflows, viz. `on_timeout()`, `on_chunk()` and so forth DO NOT
//! require locks to be acquired simultaneously. The general approach
//! is to lock and copy out the state and then immediately drop the
//! lock before proceeding to acquire the next lock. The pattern is as
//! follows:
//!
//!  // Copy state and drop locks.</br>
//!  `let state = advert_tracker.lock().unwrap().[state].clone();`</br>
//!  // Acquire the next lock.</br>
//!  `prioritizer.lock().unwrap();`
//!
//! The locking hierarchy is irrelevant if only 1 lock is acquired at a time.
//!
//! In theory, the above locking rules prevent "circular waits" and thus
//! guarantee deadlock avoidance.

extern crate lru;

use crate::{
    artifact_download_list::ArtifactDownloadList,
    download_prioritization::{AdvertTracker, AdvertTrackerFinalAction, DownloadAttemptTracker},
    gossip_protocol::{
        GossipAdvertAction, GossipAdvertSendRequest, GossipImpl, Percentage, ReceiveCheckCache,
    },
    gossip_types::{GossipChunk, GossipChunkRequest, GossipMessage},
    peer_context::{
        GossipChunkRequestTracker, GossipChunkRequestTrackerKey, PeerContext, PeerContextMap,
    },
    P2PError, P2PErrorCode, P2PResult,
};
use ic_interfaces::{
    artifact_manager::OnArtifactError::ArtifactPoolError,
    artifact_pool::ArtifactPoolError::ArtifactReplicaVersionError,
};
use ic_interfaces_transport::{TransportChannelId, TransportError, TransportPayload};
use ic_logger::{error, info, trace, warn};
use ic_protobuf::{p2p::v1 as pb, proxy::ProtoProxy, registry::node::v1::NodeRecord};
use ic_registry_client_helpers::subnet::SubnetTransportRegistry;
use ic_types::{
    artifact::{Artifact, ArtifactFilter, ArtifactId, ArtifactTag},
    chunkable::{ArtifactErrorCode, ChunkId},
    crypto::CryptoHash,
    p2p::GossipAdvert,
    NodeId, RegistryVersion,
};
use rand::{seq::SliceRandom, thread_rng};
use std::{
    collections::BTreeMap,
    error::Error,
    net::{IpAddr, SocketAddr},
    ops::DerefMut,
    str::FromStr,
    time::{Instant, SystemTime},
};

/// `DownloadManagerImpl` implements the `DownloadManager` trait.
impl GossipImpl {
    /// The method sends adverts to peers.
    pub fn send_advert_to_peers(&self, advert_request: GossipAdvertSendRequest) {
        let (peers, label) = match advert_request.action {
            GossipAdvertAction::SendToAllPeers => (self.get_current_peer_ids(), "all_peers"),
            GossipAdvertAction::SendToRandomSubset(percentage) => (
                get_random_subset_of_peers(self.get_current_peer_ids(), percentage),
                "random_subset",
            ),
        };
        self.metrics
            .adverts_by_action
            .with_label_values(&[label])
            .inc_by(peers.len() as u64);
        self.send_advert_to_peer_list(advert_request.advert, peers);
    }

    /// The method downloads chunks for adverts with the highest priority from
    /// the given peer.
    pub fn on_advert(&self, gossip_advert: GossipAdvert, peer_id: NodeId) {
        // The precondition ensured by gossip_protocol.on_advert() is that
        // the corresponding artifact is not in the artifact pool.
        // Check if we have seen this artifact before:
        if self
            .receive_check_caches
            .read()
            .values()
            .any(|cache| cache.contains(&gossip_advert.integrity_hash))
        {
            // If yes, the advert is ignored.
            return;
        }

        let mut current_peers = self.current_peers.lock();
        if let Some(_peer_context) = current_peers.get_mut(&peer_id) {
            let _ = self.prioritizer.add_advert(gossip_advert, peer_id);
        } else {
            warn!(every_n_seconds => 30, self.log, "Dropping advert from unknown node {:?}", peer_id);
        }
        self.metrics.adverts_received.inc();
    }

    /// The method starts downloading a chunk of the highest-priority
    /// artifact in the request queue if sufficient bandwidth is
    /// available.
    ///
    /// The method looks at the adverts queue for the given peer i and
    /// retrieves the highest-priority advert that is not already
    /// being downloaded from other peers more times in parallel than
    /// the maximum duplicity and for which there is room to store the
    /// corresponding artifact in the unvalidated artifact pool of
    /// peer i. If such an advert is found, the node adds a tracker
    /// for the advert to the requested queue of peer i, and sends a
    /// chunk requests for the corresponding artifact.
    ///
    /// The node also sets a download timeout for this chunk request
    /// with a duration that is appropriate for the size of the chunk.
    /// Periodically, the node iterates over the requested chunks and
    /// checks timed-out requests. The timed-out requests are removed
    /// from the requested queue of this peer and a history of such
    /// time-outs is retained.  Future download_next(i) calls take the
    /// download time-out history into account, de-prioritizing the
    /// request for peers that timed out in the past.
    ///
    /// This is a security requirement because in case no duplicity is
    /// allowed, a bad peer could otherwise maintain a "monopoly" on
    /// providing the node with a particular artifact and prevent the
    /// node from ever receiving it.
    pub fn download_next(&self, peer_id: NodeId) -> Result<(), Box<dyn Error>> {
        self.metrics.download_next_calls.inc();
        let start_time = Instant::now();
        let gossip_requests = self.download_next_compute_work(peer_id)?;
        self.metrics
            .download_next_time
            .set(start_time.elapsed().as_micros() as i64);
        self.send_chunk_requests(gossip_requests, peer_id);
        Ok(())
    }

    /// The method sends a chunk to the peer with the given node ID.
    pub fn send_chunk_to_peer(&self, gossip_chunk: GossipChunk, peer_id: NodeId) {
        trace!(
            self.log,
            "Node-{:?} sent chunk data  ->{:?} {:?}",
            self.node_id,
            peer_id,
            gossip_chunk
        );
        let message = GossipMessage::Chunk(gossip_chunk);
        let transport_channel = self.transport_channel_mapper.map(&message);
        self.transport_send(message, peer_id, transport_channel)
            .map(|_| self.metrics.chunks_sent.inc())
            .unwrap_or_else(|e| {
                // Transport and gossip implement fixed-sized queues for flow control.
                // Logging is performed at a lower level to avoid being spammed by misbehaving
                // nodes. Errors are ignored as protocol violations.
                trace!(self.log, "Send chunk failed: peer {:?} {:?} ", peer_id, e);
                self.metrics.chunk_send_failed.inc();
            });
    }

    /// The method reacts to a chunk received from the peer with the given node
    /// ID.
    pub fn on_chunk(&self, gossip_chunk: GossipChunk, peer_id: NodeId) {
        trace!(
            self.log,
            "Node-{:?} received chunk from Node-{:?} ->{:?}",
            self.node_id,
            peer_id,
            gossip_chunk
        );

        // Remove the chunk request tracker.
        let mut current_peers = self.current_peers.lock();
        if let Some(peer_context) = current_peers.get_mut(&peer_id) {
            if let Some(tracker) = peer_context
                .requested
                .remove(&GossipChunkRequestTrackerKey {
                    artifact_id: gossip_chunk.artifact_id.clone(),
                    integrity_hash: gossip_chunk.integrity_hash.clone(),
                    chunk_id: gossip_chunk.chunk_id,
                })
            {
                let artifact_tag: &'static str =
                    ArtifactTag::from(&gossip_chunk.artifact_id).into();
                self.metrics
                    .chunk_delivery_time
                    .with_label_values(&[artifact_tag])
                    .observe(tracker.requested_instant.elapsed().as_millis() as f64);
            } else {
                trace!(
                    self.log,
                    "unsolicited or timed out artifact {:?} chunk {:?} from peer {:?}",
                    gossip_chunk.artifact_id,
                    gossip_chunk.chunk_id,
                    peer_id.get()
                );
                self.metrics.chunks_unsolicited_or_timed_out.inc();
            }
        }

        // Check if the request has been served. If an error is
        // returned, the artifact chunk cannot be served by this peer.
        // In this case, the chunk download is marked as failed but
        // the advert is still being tracked for other chunks (as this might be useful
        // for StateSync). This situation is possible if one of the replicas
        // misses a part of the artifact due to corruption or progress
        // (if the peer has a higher executed height now, it might
        // have changed its state and thus may only be able to serve
        // some but not all chunks of the artifact the node is
        // interested in).
        // Allowing the rest of the artifact to be downloaded and
        // skipping only the affected chunk increase overall
        // resilience.
        if let Err(error) = gossip_chunk.artifact_chunk {
            self.metrics.chunks_not_served_from_peer.inc();
            trace!(
                self.log,
                "Chunk download failed for artifact{:?} chunk {:?} from peer {:?}",
                gossip_chunk.artifact_id,
                gossip_chunk.chunk_id,
                peer_id
            );
            if let P2PErrorCode::NotFound = error.p2p_error_code {
                // If the artifact is not found on the sender's side, drop the
                // advert from the context for this peer to prevent it from
                // being requested again from this peer.
                self.delete_advert_from_peer(
                    peer_id,
                    &gossip_chunk.artifact_id,
                    &gossip_chunk.integrity_hash,
                    self.artifacts_under_construction.write().deref_mut(),
                )
            }
            return;
        }

        // Increment the received chunks counter.
        self.metrics.chunks_received.inc();

        // Feed the chunk to artifact tracker-
        let mut artifacts_under_construction = self.artifacts_under_construction.write();

        // Find the tracker to feed the chunk.
        let artifact_tracker =
            artifacts_under_construction.get_tracker(&gossip_chunk.integrity_hash);
        if artifact_tracker.is_none() {
            trace!(
                self.log,
                "Chunk received although artifact is complete or dropped from under construction list (e.g., due to priority function change) {:?} chunk {:?} from peer {:?}",
                gossip_chunk.artifact_id,
                gossip_chunk.chunk_id,
                peer_id.get()
            );
            let _ = self.prioritizer.delete_advert_from_peer(
                &gossip_chunk.artifact_id,
                &gossip_chunk.integrity_hash,
                peer_id,
                AdvertTrackerFinalAction::Abort,
            );
            self.metrics.chunks_redundant_residue.inc();
            return;
        }
        let artifact_tracker = artifact_tracker.unwrap();

        // Feed the chunk to the tracker.
        let completed_artifact = match artifact_tracker
            .chunkable
            .add_chunk(gossip_chunk.artifact_chunk.unwrap())
        {
            // Artifact assembly is complete.
            Ok(artifact) => Some(artifact),
            Err(ArtifactErrorCode::ChunksMoreNeeded) => None,
            Err(ArtifactErrorCode::ChunkVerificationFailed) => {
                trace!(
                    self.log,
                    "Chunk verification failed for artifact{:?} chunk {:?} from peer {:?}",
                    gossip_chunk.artifact_id,
                    gossip_chunk.chunk_id,
                    peer_id
                );
                self.metrics.chunks_verification_failed.inc();
                None
            }
        };

        // Return if the artifact is complete.
        if completed_artifact.is_none() {
            return;
        }

        // Record metrics.
        self.metrics.artifacts_received.inc();

        self.metrics
            .artifact_download_time
            .observe(artifact_tracker.get_duration_sec());

        let completed_artifact = completed_artifact.unwrap();

        // Check whether the artifact matches the advertised integrity hash.
        let advert = match self.prioritizer.get_advert_from_peer(
            &gossip_chunk.artifact_id,
            &gossip_chunk.integrity_hash,
            &peer_id,
        ) {
            Ok(Some(advert)) => advert,
            Err(_) | Ok(None) => {
                trace!(
                self.log,
                "The advert for {:?} chunk {:?} from peer {:?} was not found, seems the peer never sent it.",
                gossip_chunk.artifact_id,
                gossip_chunk.chunk_id,
                peer_id.get()
            );
                return;
            }
        };
        // Check if the artifact's integrity hash matches the advertised hash
        // This construction to compute the integrity hash over all variants of an enum
        // may be updated in the future.
        let expected_ih = match &completed_artifact {
            Artifact::ConsensusMessage(msg) => ic_types::crypto::crypto_hash(msg).get(),
            Artifact::IngressMessage(msg) => ic_types::crypto::crypto_hash(msg.binary()).get(),
            Artifact::CertificationMessage(msg) => ic_types::crypto::crypto_hash(msg).get(),
            Artifact::DkgMessage(msg) => ic_types::crypto::crypto_hash(msg).get(),
            Artifact::EcdsaMessage(msg) => ic_types::crypto::crypto_hash(msg).get(),
            Artifact::CanisterHttpMessage(msg) => ic_types::crypto::crypto_hash(msg).get(),
            // FileTreeSync is not of ArtifactKind kind, and it's used only for testing.
            // Thus, we make up the integrity_hash.
            Artifact::FileTreeSync(_msg) => CryptoHash(vec![]),
            Artifact::StateSync(msg) => ic_types::crypto::crypto_hash(msg).get(),
        };

        if expected_ih != advert.integrity_hash {
            warn!(
                self.log,
                "The integrity hash for {:?} from peer {:?} does not match. Expected {:?}, got {:?}.",
                gossip_chunk.artifact_id,
                peer_id.get(),
                expected_ih,
                advert.integrity_hash;
            );
            self.metrics.integrity_hash_check_failed.inc();

            // The advert is deleted from this particular peer. Gossip may fetch the
            // artifact again from another peer.
            let _ = self.prioritizer.delete_advert_from_peer(
                &gossip_chunk.artifact_id,
                &gossip_chunk.integrity_hash,
                peer_id,
                AdvertTrackerFinalAction::Abort,
            );
            return;
        }

        // Add the artifact hash to the receive check set.
        let charged_peer = artifact_tracker.peer_id;
        match self.receive_check_caches.write().get_mut(&charged_peer) {
            Some(v) => {
                v.put(advert.integrity_hash.clone(), ());
            }
            None => warn!(
                every_n_seconds => 5,
                self.log,
                "Peer {:?} has no receive check cache", charged_peer
            ),
        }

        // The artifact is complete and the integrity hash is okay.
        // Clean up the adverts for all peers:
        let _ = self.prioritizer.delete_advert(
            &gossip_chunk.artifact_id,
            &gossip_chunk.integrity_hash,
            AdvertTrackerFinalAction::Success,
        );
        artifacts_under_construction.remove_tracker(&gossip_chunk.integrity_hash);

        // Drop the locks before calling client callbacks.
        std::mem::drop(artifacts_under_construction);
        std::mem::drop(current_peers);

        // Client callbacks.
        trace!(
            self.log,
            "Node-{:?} received artifact from Node-{:?} ->{:?}",
            self.node_id,
            peer_id,
            gossip_chunk.artifact_id
        );
        match self
            .artifact_manager
            .on_artifact(completed_artifact, advert, &peer_id)
        {
            Ok(_) => (),
            // If this Replica is running an unexpected version, it will log
            // an unhelpfully large volume of `ArtifactReplicaVersionError`s.
            // Here we set the log rate at a more appropriate level.
            Err(ArtifactPoolError(ArtifactReplicaVersionError(err))) => warn!(
                every_n_seconds => 5,
                self.log,
                "Artifact is not processed successfully by Artifact Manager: {:?}", err
            ),
            Err(err) => warn!(
                self.log,
                "Artifact is not processed successfully by Artifact Manager: {:?}", err
            ),
        }
    }

    /// The method reacts to a disconnect event event for the peer with the
    /// given node ID.
    pub fn peer_connection_down(&self, peer_id: NodeId) {
        self.metrics.connection_down_events.inc();
        let now = SystemTime::now();
        let mut current_peers = self.current_peers.lock();
        if let Some(peer_context) = current_peers.get_mut(&peer_id) {
            peer_context.disconnect_time = Some(now);
            trace!(
                self.log,
                "Gossip On Disconnect event with peer: {:?} at time {:?}",
                peer_id,
                now
            );
        };
    }

    /// The method reacts to a connect event event for the peer with the given
    /// node ID.
    pub fn peer_connection_up(&self, peer_id: NodeId) {
        self.metrics.connection_up_events.inc();
        let _now = SystemTime::now();

        let last_disconnect = self
            .current_peers
            .lock()
            .get_mut(&peer_id)
            .and_then(|res| res.disconnect_time);
        match last_disconnect {
            Some(last_disconnect) => {
                match last_disconnect.elapsed() {
                    Ok(elapsed) => {
                        trace!(
                            self.log,
                            "Disconnect to peer {:?} for {:?} seconds",
                            peer_id,
                            elapsed
                        );

                        // Clear the send queues and send re-transmission request to the peer on
                        // connect.
                        self.transport.clear_send_queues(&peer_id);
                    }
                    Err(e) => {
                        warn!(self.log, "Error in elapsed time calculation: {:?}", e);
                    }
                }
            }
            None => {
                trace!(
                    self.log,
                    "No previous disconnect event recorded in peer manager for node : {:?}",
                    peer_id
                );
            }
        }
        self.send_retransmission_request(peer_id);
    }

    /// The method reacts to a retransmission request.
    pub fn on_retransmission_request(
        &self,
        gossip_re_request: &ArtifactFilter,
        peer_id: NodeId,
    ) -> P2PResult<()> {
        const BUSY_ERR: P2PResult<()> = Err(P2PError {
            p2p_error_code: P2PErrorCode::Busy,
        });
        // Throttle processing of incoming re-transmission request
        self.current_peers
            .lock()
            .get_mut(&peer_id)
            .ok_or_else(|| {
                warn!(self.log, "Can't find peer context for peer: {:?}", peer_id);
                P2PError {
                    p2p_error_code: P2PErrorCode::NotFound,
                }
            })
            .map_or_else(Err, |peer_context| {
                let elapsed_ms = peer_context
                    .last_retransmission_request_processed_time
                    .elapsed()
                    .as_millis();
                if elapsed_ms < self.gossip_config.retransmission_request_ms as u128 {
                    BUSY_ERR
                } else {
                    peer_context.last_retransmission_request_processed_time = Instant::now();
                    Ok(())
                }
            })?;

        // A retransmission request was received from a peer.
        // The send queues are cleared and a response is sent containing all the adverts
        // that satisfy the filter.
        self.transport.clear_send_queues(&peer_id);

        let adverts = self
            .artifact_manager
            .get_all_validated_by_filter(gossip_re_request)
            .into_iter();

        adverts.for_each(|advert| self.send_advert_to_peer_list(advert, vec![peer_id]));
        Ok(())
    }

    /// The method sends a retransmission request to the peer with the given
    /// node ID.
    pub fn send_retransmission_request(&self, peer_id: NodeId) {
        let filter = self.artifact_manager.get_filter();
        let message = GossipMessage::RetransmissionRequest(filter);
        let transport_channel = self.transport_channel_mapper.map(&message);
        let start_time = Instant::now();
        self.transport_send(message, peer_id, transport_channel)
            .map(|_| self.metrics.retransmission_requests_sent.inc())
            .unwrap_or_else(|e| {
                trace!(
                    self.log,
                    "Send retransmission request failed: peer {:?} {:?} ",
                    peer_id,
                    e
                );
                self.metrics.retransmission_request_send_failed.inc();
            });
        self.metrics
            .retransmission_request_time
            .observe(start_time.elapsed().as_millis() as f64)
    }

    /// The method is invoked periodically by the *Gossip* component to perform
    /// P2P book keeping tasks.
    pub fn on_timer(&self) {
        let (update_priority_fns, retransmission_request, refresh_registry) =
            self.get_timer_tasks();
        if update_priority_fns {
            let dropped_adverts = self
                .prioritizer
                .update_priority_functions(self.artifact_manager.as_ref());
            let mut artifacts_under_construction = self.artifacts_under_construction.write();
            dropped_adverts
                .iter()
                .for_each(|id| artifacts_under_construction.remove_tracker(id));
        }

        if retransmission_request {
            // Send a retransmission request to all peers.
            let current_peers = self.get_current_peer_ids();
            for peer in current_peers {
                self.send_retransmission_request(peer);
            }
        }

        if refresh_registry {
            self.refresh_registry();
        }

        // Collect the peers with timed-out requests.
        let mut timed_out_peers = Vec::new();
        for (node_id, peer_context) in self.current_peers.lock().iter_mut() {
            if self.process_timed_out_requests(node_id, peer_context) {
                timed_out_peers.push(*node_id);
            }
        }

        // Process timed-out artifacts.
        self.process_timed_out_artifacts();

        // Compute the set of peers that need to be evaluated by the download manager.
        let peer_ids = if update_priority_fns {
            self.get_current_peer_ids().into_iter()
        } else {
            timed_out_peers.into_iter()
        };

        // Invoke download_next(i) for each peer i.
        for peer_id in peer_ids {
            let _ = self.download_next(peer_id);
        }
    }

    fn get_current_peer_ids(&self) -> Vec<NodeId> {
        self.current_peers
            .lock()
            .iter()
            .map(|(k, _v)| k.to_owned())
            .collect()
    }

    /// The method adds the given peer to the list of current peers.
    fn add_node(
        &self,
        node_id: NodeId,
        peer_addr: SocketAddr,
        registry_version: RegistryVersion,
    ) -> P2PResult<()> {
        // Only add other peers to the peer list.
        if node_id == self.node_id {
            return Err(P2PError {
                p2p_error_code: P2PErrorCode::Failed,
            });
        }

        // Add the peer to the list of current peers and the event handler, and drop the
        // lock before calling into transport.
        {
            let mut current_peers = self.current_peers.lock();

            if current_peers.contains_key(&node_id) {
                return Err(P2PError {
                    p2p_error_code: P2PErrorCode::Exists,
                });
            }
            current_peers
                .entry(node_id)
                .or_insert_with(PeerContext::new);
            info!(self.log, "Nodes {:0} added", node_id);
        }

        self.transport
            .start_connection(&node_id, peer_addr, registry_version)
            .map_err(|e| {
                let mut current_peers = self.current_peers.lock();
                current_peers.remove(&node_id);
                warn!(self.log, "start connections failed {:?} {:?}", node_id, e);
                P2PError {
                    p2p_error_code: P2PErrorCode::InitFailed,
                }
            })
    }

    /// This helper method returns a list of tasks to be performed by this timer
    /// invocation.
    fn get_timer_tasks(&self) -> (bool, bool, bool) {
        let mut update_priority_fns = false;
        let mut refresh_registry = false;
        let mut retransmission_request = false;
        // Check if the priority function should be updated.
        {
            let mut pfn_invocation_instant = self.pfn_invocation_instant.lock();
            if pfn_invocation_instant.elapsed().as_millis()
                >= self.gossip_config.pfn_evaluation_period_ms as u128
            {
                update_priority_fns = true;
                *pfn_invocation_instant = Instant::now();
            }
        }

        // Check if a retransmission request needs to be sent.
        {
            let mut retransmission_request_instant = self.retransmission_request_instant.lock();
            if retransmission_request_instant.elapsed().as_millis()
                >= self.gossip_config.retransmission_request_ms as u128
            {
                retransmission_request = true;
                *retransmission_request_instant = Instant::now();
            }
        }

        // Check if the registry has to be refreshed.
        {
            let mut registry_refresh_instant = self.registry_refresh_instant.lock();
            if registry_refresh_instant.elapsed().as_millis()
                >= self.gossip_config.pfn_evaluation_period_ms as u128
            {
                refresh_registry = true;
                *registry_refresh_instant = Instant::now();
            }
        }
        (
            update_priority_fns,
            retransmission_request,
            refresh_registry,
        )
    }

    // Update the peer manager state based on the latest registry value.
    pub fn refresh_registry(&self) {
        let latest_registry_version = self.registry_client.get_latest_version();
        self.metrics
            .registry_version_used
            .set(latest_registry_version.get() as i64);

        let subnet_nodes = self.merge_subnet_membership(latest_registry_version);
        let self_not_in_subnet = !subnet_nodes.contains_key(&self.node_id);

        // If a peer is not in the nodes within this subnet, remove.
        // If self is not in the subnet, remove all peers.
        for peer_id in self.get_current_peer_ids().iter() {
            if !subnet_nodes.contains_key(peer_id) || self_not_in_subnet {
                self.remove_node(*peer_id);
                self.metrics.nodes_removed.inc();
            }
        }
        // If self is not subnet, exit early to avoid adding nodes to the list of peers.
        if self_not_in_subnet {
            return;
        }
        // Add in nodes to peer manager.
        for (node_id, node_record) in subnet_nodes.iter() {
            match get_peer_addr(node_record) {
                Err(err) => {
                    // If getting the peer socket fails, remove the node from current peer list.
                    // This removal makes it possible to attempt a re-connection on the next registry refresh.
                    self.current_peers.lock().remove(node_id);
                    // Invalid socket addresses should not be pushed in the registry/config on first place.
                    error!(
                        self.log,
                        "Invalid socket addr: node_id = {:?}, error = {:?}.", *node_id, err
                    );
                }
                Ok(peer_addr) => {
                    if self
                        .add_node(*node_id, peer_addr, latest_registry_version)
                        .is_ok()
                    {
                        self.receive_check_caches.write().insert(
                            *node_id,
                            ReceiveCheckCache::new(
                                self.gossip_config.receive_check_cache_size as usize,
                            ),
                        );
                    }
                }
            }
        }
    }

    // Merge node records from subnet_membership_version (provided by consensus)
    // to latest_registry_version. This returns the current subnet membership set.
    fn merge_subnet_membership(
        &self,
        latest_registry_version: RegistryVersion,
    ) -> BTreeMap<NodeId, NodeRecord> {
        let subnet_membership_version = self
            .consensus_pool_cache
            .get_oldest_registry_version_in_use();
        let mut subnet_nodes = BTreeMap::new();
        // Iterate from min(consensus_registry_version,latest_local_registry_version) to max(consensus_registry_version,latest_local_registry_version).
        // The `consensus_registry_version` is extracted from the latest CUP seen.
        // The `latest_local_registry_version` is the latest registry version known to this node.
        // In almost any case `latest_local_registry_version >= consensus_registry_version` but there may exist cases where this condition does not hold.
        // In that case we should at least include our latest local view of the subnet.
        for version in subnet_membership_version
            .get()
            .min(latest_registry_version.get())
            ..=subnet_membership_version
                .get()
                .max(latest_registry_version.get())
        {
            let version = RegistryVersion::from(version);
            let node_records = self
                .registry_client
                .get_subnet_transport_infos(self.subnet_id, version)
                .unwrap_or(None)
                .unwrap_or_default();
            for node in node_records {
                subnet_nodes.insert(node.0, node.1);
            }
        }
        subnet_nodes
    }

    /// This method removes the given node from peer manager and clears adverts.
    fn remove_node(&self, node_id: NodeId) {
        {
            let mut current_peers = self.current_peers.lock();
            self.transport.stop_connection(&node_id);
            // Remove the peer irrespective of the result of the stop_connection() call.
            current_peers.remove(&node_id);
            info!(self.log, "Nodes {:0} removed", node_id);
        }

        self.receive_check_caches.write().remove(&node_id);
        self.prioritizer
            .clear_peer_adverts(node_id, AdvertTrackerFinalAction::Abort)
            .unwrap_or_else(|e| {
                info!(
                    self.log,
                    "Failed to clear peer adverts when removing peer {:?} with error {:?}",
                    node_id,
                    e
                )
            });
    }

    /// The method sends the given message over transport to the given peer.
    fn transport_send(
        &self,
        message: GossipMessage,
        peer_id: NodeId,
        channel_id: TransportChannelId,
    ) -> Result<(), TransportError> {
        let _timer = self
            .metrics
            .op_duration
            .with_label_values(&["transport_send"])
            .start_timer();
        let message = TransportPayload(pb::GossipMessage::proxy_encode(message).unwrap());
        self.transport
            .send(&peer_id, channel_id, message)
            .map_err(|e| {
                trace!(
                    self.log,
                    "Failed to send gossip message to peer {:?}: {:?}",
                    peer_id,
                    e
                );
                e
            })
    }

    /// The method sends the given advert to the given list of peers.
    fn send_advert_to_peer_list(&self, gossip_advert: GossipAdvert, peer_ids: Vec<NodeId>) {
        let message = GossipMessage::Advert(gossip_advert.clone());
        let transport_channel = self.transport_channel_mapper.map(&message);
        for peer_id in peer_ids {
            self.transport_send(message.clone(), peer_id, transport_channel)
                .map(|_| self.metrics.adverts_sent.inc())
                .unwrap_or_else(|_e| {
                    // Ignore advert send failures
                    self.metrics.adverts_send_failed.inc();
                });
            trace!(
                self.log,
                "Node-{:?} sent gossip advert ->{:?} {:?}",
                self.node_id,
                peer_id,
                gossip_advert
            );
        }
    }

    /// The method sends the given chunk requests to the given peer.
    fn send_chunk_requests(&self, requests: Vec<GossipChunkRequest>, peer_id: NodeId) {
        for request in requests {
            let message = GossipMessage::ChunkRequest(request);
            let transport_channel = self.transport_channel_mapper.map(&message);
            // Debugging
            trace!(
                self.log,
                "Node-{:?} sending chunk request ->{:?} {:?}",
                self.node_id,
                peer_id,
                message
            );
            self.transport_send(message, peer_id, transport_channel)
                .map(|_| self.metrics.chunks_requested.inc())
                .unwrap_or_else(|_e| {
                    // Ingore chunk send failures. Points to a misbehaving peer
                    self.metrics.chunk_request_send_failed.inc();
                });
        }
    }

    /// The method checks if a download from a peer can be initiated.
    ///
    /// A peer may not be ready for downloads for various reasons:
    ///
    /// a) The peer's download request capacity has been reached.</br>
    /// b) The peer is not a current peer (e.g., it is an unknown peer or a peer
    /// that was removed)</br>
    /// c) The peer was disconnected (TODO -  P2P512)
    fn is_peer_ready_for_download<'a>(
        &self,
        peer_id: NodeId,
        peer_dictionary: &'a PeerContextMap,
    ) -> Result<&'a PeerContext, P2PError> {
        match peer_dictionary.get(&peer_id) {
            // Check that the peer is present and
            // there is available capacity to stream chunks from this peer.
            Some(peer_context)
                if peer_context.requested.len()
                    < self.gossip_config.max_artifact_streams_per_peer as usize =>
            {
                Ok(peer_context)
            }
            _ => Err(P2PError {
                p2p_error_code: P2PErrorCode::Busy,
            }),
        }
    }

    /// The method returns the request tracker for ongoing chunk requests from a
    /// peer.
    fn get_peer_chunk_tracker<'a>(
        &self,
        peer_id: &NodeId,
        peers: &'a PeerContextMap,
        artifact_id: &ArtifactId,
        integrity_hash: &CryptoHash,
        chunk_id: ChunkId,
    ) -> Option<&'a GossipChunkRequestTracker> {
        let peer_context = peers.get(peer_id)?;
        peer_context.requested.get(&GossipChunkRequestTrackerKey {
            artifact_id: artifact_id.clone(),
            integrity_hash: integrity_hash.clone(),
            chunk_id,
        })
    }

    /// The method returns a chunk request if a chunk can be downloaded from the
    /// given peer.
    ///
    /// This is a helper function for `download_next()`. It consolidates checks
    /// and conditions that dictate a chunk's download eligibility from a
    /// given peer.
    fn get_chunk_request(
        &self,
        peers: &PeerContextMap,
        peer_id: NodeId,
        advert_tracker: &AdvertTracker,
        chunk_id: ChunkId,
    ) -> Option<GossipChunkRequest> {
        // Skip if the chunk download has been already attempted even if the node is
        // currently downloading it OR has a failed attempt in this round.
        if advert_tracker.peer_attempted(chunk_id, &peer_id) {
            None?
        }

        // Skip if some other peer is downloading the chunk and maximum
        // duplicity has been reached.
        let duplicity = advert_tracker
            .peers
            .iter()
            .filter_map(|advertiser| {
                self.get_peer_chunk_tracker(
                    advertiser,
                    peers,
                    &advert_tracker.advert.artifact_id,
                    &advert_tracker.advert.integrity_hash,
                    chunk_id,
                )
            })
            .count();

        if duplicity >= self.gossip_config.max_duplicity as usize {
            None?
        }

        // Since the peer has not attempted a chunk download in this round and will not
        // violate duplicity constraints, a gossip chunk request is returned.
        Some(GossipChunkRequest {
            artifact_id: advert_tracker.advert.artifact_id.clone(),
            integrity_hash: advert_tracker.advert.integrity_hash.clone(),
            chunk_id,
        })
    }

    /// The method returns the next set of downloads that can be initiated
    /// within the constraints of the ICP protocol.
    fn download_next_compute_work(
        &self,
        peer_id: NodeId,
    ) -> Result<Vec<GossipChunkRequest>, impl Error> {
        // Get the peer context.
        let mut current_peers = self.current_peers.lock();
        let peer_context = self.is_peer_ready_for_download(peer_id, &current_peers)?;
        let requested_instant = Instant::now(); // function granularity for instant is good enough
        let max_streams_per_peer = self.gossip_config.max_artifact_streams_per_peer as usize;

        assert!(peer_context.requested.len() <= max_streams_per_peer);
        let num_downloadable_chunks = max_streams_per_peer - peer_context.requested.len();
        if num_downloadable_chunks == 0 {
            return Err(Box::new(P2PError {
                p2p_error_code: P2PErrorCode::Busy,
            }));
        }

        let mut requests = Vec::new();
        let mut artifacts_under_construction = self.artifacts_under_construction.write();
        // Get a prioritized iterator.
        let peer_advert_queues = self.prioritizer.get_peer_priority_queues(peer_id);
        let peer_advert_map = peer_advert_queues.peer_advert_map_ref.read().unwrap();

        let mut visited = 0;
        for (_, advert_tracker) in peer_advert_map.iter() {
            visited += 1;
            if requests.len() >= num_downloadable_chunks {
                break;
            }

            let mut advert_tracker = advert_tracker.write().unwrap();
            let advert_tracker = advert_tracker.deref_mut();

            // Try to begin a download for the artifact and collect its chunk requests.
            if let Some(artifact_tracker) = artifacts_under_construction.schedule_download(
                peer_id,
                &advert_tracker.advert,
                &self.gossip_config,
                current_peers.len() as u32,
                self.artifact_manager.as_ref(),
            ) {
                // Collect gossip requests that can be initiated for this artifact.
                // The function get_chunk_request() returns requests for chunks that satisfy
                // chunk download constraints. These requests are collected and download
                // attempts are recorded.
                let new_chunk_requests = artifact_tracker
                    .chunkable
                    .chunks_to_download()
                    .filter_map(|id: ChunkId| {
                        self.get_chunk_request(&current_peers, peer_id, advert_tracker, id)
                            .map(|req| {
                                advert_tracker.record_attempt(id, &peer_id);
                                req
                            })
                    })
                    .take(num_downloadable_chunks - requests.len());

                // Extend the requests to be send out to this peer
                requests.extend(new_chunk_requests);
            }
        }

        self.metrics.download_next_visited.set(visited as i64);
        self.metrics
            .download_next_selected
            .set(requests.len() as i64);

        let peer_context = current_peers.get_mut(&peer_id).unwrap();
        peer_context.requested.extend(requests.iter().map(|req| {
            (
                GossipChunkRequestTrackerKey {
                    artifact_id: req.artifact_id.clone(),
                    integrity_hash: req.integrity_hash.clone(),
                    chunk_id: req.chunk_id,
                },
                GossipChunkRequestTracker { requested_instant },
            )
        }));

        assert!(peer_context.requested.len() <= max_streams_per_peer);
        Ok(requests)
    }

    /// The method deletes the given advert from a particular peer.
    ///
    /// If the deletion results in zero peers downloading the advert, then the
    /// entry in the under-construction list is cleaned up as well.
    fn delete_advert_from_peer(
        &self,
        peer_id: NodeId,
        artifact_id: &ArtifactId,
        integrity_hash: &CryptoHash,
        artifacts_under_construction: &mut dyn ArtifactDownloadList,
    ) {
        let ret = self.prioritizer.delete_advert_from_peer(
            artifact_id,
            integrity_hash,
            peer_id,
            AdvertTrackerFinalAction::Abort,
        );
        // Remove the artifact from the under-construction list if this peer was the
        // last peer with an advert tracker for this artifact, indicated by the
        // previous call's return value.
        if ret.is_ok() {
            artifacts_under_construction.remove_tracker(integrity_hash);
        }
    }

    /// The method processes timed-out artifacts.
    ///
    /// This method is called by the method on_timer(). It checks if there are
    /// any timed-out artifacts in the under-construction list and removes
    /// them from.
    fn process_timed_out_artifacts(&self) {
        // Prune the expired downloads from the under-construction list.
        let expired_downloads = self
            .artifacts_under_construction
            .write()
            .deref_mut()
            .prune_expired_downloads();

        self.metrics
            .artifact_timeouts
            .inc_by(expired_downloads.len() as u64);

        // Add the timed-out adverts to the end of their respective
        // priority queue in the prioritizer.
        expired_downloads
            .into_iter()
            .for_each(|(artifact_id, integrity_hash)| {
                let _ = self
                    .prioritizer
                    .reinsert_advert_at_tail(&artifact_id, &integrity_hash);
            });
    }

    /// The method processes timed-out requests
    ///
    /// This method is called by the method on_timer(). It checks if there are
    /// any chunk requests that timed out from the given peer and returns
    /// "true" if this is the case.
    fn process_timed_out_requests(&self, node_id: &NodeId, peer_context: &mut PeerContext) -> bool {
        // Mark time-out chunks.
        let mut timed_out_chunks: Vec<_> = Vec::new();
        let mut peer_timed_out: bool = false;
        peer_context.requested.retain(|key, tracker| {
            let timed_out = tracker.requested_instant.elapsed().as_millis()
                >= self.gossip_config.max_chunk_wait_ms as u128;
            if timed_out {
                self.metrics.chunks_timed_out.inc();
                timed_out_chunks.push((
                    *node_id,
                    key.chunk_id,
                    key.artifact_id.clone(),
                    key.integrity_hash.clone(),
                ));
                peer_timed_out = true;
                trace!(
                    self.log,
                    "Chunk timeout Key {:?} Tracker {:?} elapsed{:?} requested {:?} Now {:?}",
                    key,
                    tracker,
                    tracker.requested_instant.elapsed().as_millis(),
                    tracker.requested_instant,
                    std::time::Instant::now()
                )
            }
            // Retain chunks that have not timed out.
            !timed_out
        });

        for (node_id, chunk_id, artifact_id, integrity_hash) in timed_out_chunks.into_iter() {
            self.process_timed_out_chunk(&node_id, artifact_id, integrity_hash, chunk_id)
        }

        peer_timed_out
    }

    /// The method processes a timed-out chunk.
    fn process_timed_out_chunk(
        &self,
        node_id: &NodeId,
        artifact_id: ArtifactId,
        integrity_hash: CryptoHash,
        chunk_id: ChunkId,
    ) {
        // Drop it and switch the preferred primary so that the next node that
        // advertised the chunk picks it up.
        let _ = self
            .prioritizer
            .get_advert_tracker(&artifact_id, &integrity_hash)
            .map(|advert_tracker| {
                // unset the in-progress flag.
                let mut advert_tracker = advert_tracker.write().unwrap();
                advert_tracker.unset_in_progress(chunk_id);
                // If we have exhausted a round of download attempts (i.e., each peer that
                // advertised it has timed out once), then reset the attempts
                // history so that peers can be probed once for the next round.
                if advert_tracker.is_attempts_round_complete(chunk_id) {
                    advert_tracker.attempts_round_reset(chunk_id)
                }
            });
        #[rustfmt::skip]
        trace!(self.log, "Timed-out: Peer{:?} Artifact{:?} Chunk{:?}",
               node_id, chunk_id, artifact_id);
    }
}

/// The method returns a random subset from the list of peers.
fn get_random_subset_of_peers(peers: Vec<NodeId>, percentage: Percentage) -> Vec<NodeId> {
    let multiplier = (percentage.get() as f64) / 100.0_f64;
    let subset_size = (peers.len() as f64 * multiplier).ceil() as usize;
    let mut rng = thread_rng();
    peers
        .choose_multiple(&mut rng, subset_size)
        .cloned()
        .collect()
}

fn get_peer_addr(node_record: &NodeRecord) -> Result<SocketAddr, String> {
    let socket_addr: (IpAddr, u16) = node_record
        .p2p_flow_endpoints
        .get(0)
        .and_then(|flow_enpoint| flow_enpoint.endpoint.as_ref())
        .and_then(|endpoint| {
            Some((
                IpAddr::from_str(&endpoint.ip_addr).ok()?,
                endpoint.port.try_into().ok()?,
            ))
        })
        .ok_or("Failed to parse NodeRecord to (IpAddr,u16) tuple")?;

    Ok(SocketAddr::from(socket_addr))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::download_prioritization::DownloadPrioritizerError;
    use crate::gossip_protocol::Percentage;
    use ic_interfaces::artifact_manager::{ArtifactManager, OnArtifactError};
    use ic_interfaces::consensus_pool::ConsensusPoolCache;
    use ic_interfaces_registry::RegistryClient;
    use ic_logger::{LoggerImpl, ReplicaLogger};
    use ic_metrics::MetricsRegistry;
    use ic_protobuf::registry::node::v1::{
        connection_endpoint::Protocol, ConnectionEndpoint, FlowEndpoint,
    };
    use ic_registry_client_fake::FakeRegistryClient;
    use ic_test_utilities::consensus::fake::FakeSigner;
    use ic_test_utilities::port_allocation::allocate_ports;
    use ic_test_utilities::{
        consensus::MockConsensusCache,
        p2p::*,
        thread_transport::*,
        types::ids::{node_id_to_u64, node_test_id, subnet_test_id},
    };
    use ic_test_utilities_registry::{add_subnet_record, SubnetRecordBuilder};
    use ic_types::artifact::{DkgMessage, DkgMessageAttribute};
    use ic_types::consensus::dkg::DealingContent;
    use ic_types::crypto::{
        threshold_sig::ni_dkg::{NiDkgDealing, NiDkgId, NiDkgTag, NiDkgTargetSubnet},
        {CryptoHash, CryptoHashOf},
    };
    use ic_types::signature::BasicSignature;
    use ic_types::SubnetId;
    use ic_types::{
        artifact,
        artifact::{Artifact, ArtifactAttribute, ArtifactPriorityFn, Priority},
        chunkable::{ArtifactChunk, ArtifactChunkData, Chunkable, ChunkableArtifact},
        Height, NodeId, PrincipalId,
    };
    use std::collections::HashSet;
    use std::convert::TryFrom;
    use std::ops::Range;
    use std::sync::{Arc, Mutex};

    /// This priority function always returns Priority::FetchNow.
    fn priority_fn_fetch_now_all(_: &ArtifactId, _: &ArtifactAttribute) -> Priority {
        Priority::FetchNow
    }

    /// The test artifact manager.
    #[derive(Default)]
    pub(crate) struct TestArtifactManager {
        /// The quota.
        pub quota: usize,
        /// The number of chunks.
        pub num_chunks: u32,
    }

    /// The test artifact.
    struct TestArtifact {
        /// The number of chunks.
        num_chunks: u32,
        /// The list of artifact chunks.
        chunks: Vec<ArtifactChunk>,
    }

    /// `TestArtifact` implements the `Chunkable` trait.
    impl Chunkable for TestArtifact {
        /// The method returns an Iterator over the chunks to download.
        fn chunks_to_download(&self) -> Box<dyn Iterator<Item = ChunkId>> {
            Box::new(
                (0..self.num_chunks as u32)
                    .map(ChunkId::from)
                    .collect::<Vec<_>>()
                    .into_iter(),
            )
        }

        /// The method adds the given chunk.
        fn add_chunk(
            &mut self,
            artifact_chunk: ArtifactChunk,
        ) -> Result<Artifact, ArtifactErrorCode> {
            self.chunks.push(artifact_chunk.clone());
            if self.chunks.len() == self.num_chunks as usize {
                match artifact_chunk.artifact_chunk_data {
                    ArtifactChunkData::UnitChunkData(artifact) => Ok(artifact),
                    _ => Err(ArtifactErrorCode::ChunkVerificationFailed),
                }
            } else {
                Err(ArtifactErrorCode::ChunksMoreNeeded)
            }
        }
    }

    /// The `TestArtifactManager` implements the `TestArtifact` trait.
    impl ArtifactManager for TestArtifactManager {
        /// The method ignores the artifact and always returns Ok(()).
        fn on_artifact(
            &self,
            mut _msg: artifact::Artifact,
            _advert: GossipAdvert,
            _peer_id: &NodeId,
        ) -> Result<(), OnArtifactError<artifact::Artifact>> {
            Ok(())
        }

        /// The method to test if an artifact is available is not implemented as
        /// it is not used.
        fn has_artifact(&self, _message_id: &artifact::ArtifactId) -> bool {
            unimplemented!()
        }

        /// The method to return a validated artifact is not implemented as
        /// it is not used.
        fn get_validated_by_identifier(
            &self,
            _message_id: &artifact::ArtifactId,
        ) -> Option<Box<dyn ChunkableArtifact + '_>> {
            unimplemented!()
        }

        /// The method to get the artifact filter is not implemented as
        /// it is not used.
        fn get_filter(&self) -> artifact::ArtifactFilter {
            unimplemented!()
        }
        /// The method to get the list of validated adverts is not implemented
        /// as it is not used.
        fn get_all_validated_by_filter(
            &self,
            _filter: &artifact::ArtifactFilter,
        ) -> Vec<GossipAdvert> {
            unimplemented!()
        }

        /// The method returns the internal quota.
        fn get_remaining_quota(
            &self,
            _tag: artifact::ArtifactTag,
            _peer_id: NodeId,
        ) -> Option<usize> {
            Some(self.quota)
        }

        /// The method returns the priority function that always uses
        /// Priority::FetchAll.
        fn get_priority_function(&self, _: artifact::ArtifactTag) -> Option<ArtifactPriorityFn> {
            Some(Box::new(priority_fn_fetch_now_all))
        }

        /// The method returns a new TestArtifact instance.
        fn get_chunk_tracker(
            &self,
            _id: &artifact::ArtifactId,
        ) -> Option<Box<dyn Chunkable + Send + Sync>> {
            let chunks = vec![];
            Some(Box::new(TestArtifact {
                num_chunks: self.num_chunks,
                chunks,
            }))
        }
    }

    /// The function returns a new
    /// ic_test_utilities::thread_transport::ThreadPort instance.
    fn get_transport(
        instance_id: u32,
        hub: Arc<Mutex<Hub>>,
        logger: &LoggerImpl,
        rt_handle: tokio::runtime::Handle,
    ) -> Arc<ThreadPort> {
        let log: ReplicaLogger = logger.root.clone().into();
        ThreadPort::new(node_test_id(instance_id as u64), hub, log, rt_handle)
    }

    fn new_test_gossip_impl_with_registry(
        num_replicas: u32,
        logger: &LoggerImpl,
        registry_client: Arc<dyn RegistryClient>,
        consensus_pool_cache: Arc<dyn ConsensusPoolCache>,
        rt_handle: tokio::runtime::Handle,
    ) -> GossipImpl {
        let log: ReplicaLogger = logger.root.clone().into();
        let artifact_manager = TestArtifactManager {
            quota: 2 * 1024 * 1024 * 1024,
            num_chunks: 1,
        };

        // Set up transport.
        let hub_access: HubAccess = Arc::new(Mutex::new(Default::default()));
        for instance_id in 0..num_replicas {
            let thread_port =
                get_transport(instance_id, hub_access.clone(), logger, rt_handle.clone());
            hub_access
                .lock()
                .unwrap()
                .insert(node_test_id(instance_id as u64), thread_port);
        }

        let transport_hub = hub_access.lock().unwrap();
        let tp = transport_hub.get(&node_test_id(0));

        // Set up the prioritizer.
        let metrics_registry = MetricsRegistry::new();

        let transport_channels = vec![TransportChannelId::from(0)];

        // Create fake peers.
        let artifact_manager = Arc::new(artifact_manager);
        GossipImpl::new(
            node_test_id(0),
            subnet_test_id(0),
            consensus_pool_cache,
            registry_client,
            artifact_manager,
            tp,
            transport_channels,
            log,
            &metrics_registry,
        )
    }

    fn new_test_gossip(
        num_replicas: u32,
        logger: &LoggerImpl,
        rt_handle: tokio::runtime::Handle,
    ) -> GossipImpl {
        let allocated_ports = allocate_ports("127.0.0.1", num_replicas as u16)
            .expect("Port allocation for test failed");
        let node_port_allocation: Vec<u16> = allocated_ports.iter().map(|np| np.port).collect();
        assert_eq!(num_replicas as usize, node_port_allocation.len());
        let node_port_allocation = Arc::new(node_port_allocation);
        let data_provider =
            test_group_set_registry(subnet_test_id(P2P_SUBNET_ID_DEFAULT), node_port_allocation);
        let registry_client = Arc::new(FakeRegistryClient::new(data_provider));
        registry_client.update_to_latest_version();

        let mut mock_consensus_cache = MockConsensusCache::new();
        mock_consensus_cache
            .expect_get_oldest_registry_version_in_use()
            .returning(move || RegistryVersion::from(1));
        let consensus_pool_cache = Arc::new(mock_consensus_cache);

        new_test_gossip_impl_with_registry(
            num_replicas,
            logger,
            registry_client,
            consensus_pool_cache,
            rt_handle,
        )
    }

    /// The function adds the given number of adverts to the download manager.
    fn test_add_adverts(gossip: &GossipImpl, range: Range<u32>, node_id: NodeId) {
        for advert_id in range {
            let gossip_advert = GossipAdvert {
                artifact_id: ArtifactId::FileTreeSync(advert_id.to_string()),
                attribute: ArtifactAttribute::FileTreeSync(advert_id.to_string()),
                size: 0,
                integrity_hash: CryptoHash(Vec::from(advert_id.to_be_bytes())),
            };
            gossip.on_advert(gossip_advert, node_id)
        }
    }

    /// The functions tests that the peer context drops all requests after a
    /// time-out.
    fn test_timeout_peer(gossip: &GossipImpl, node_id: &NodeId) {
        let sleep_duration =
            std::time::Duration::from_millis((gossip.gossip_config.max_chunk_wait_ms * 2) as u64);
        std::thread::sleep(sleep_duration);
        let mut current_peers = gossip.current_peers.lock();
        let peer_context = current_peers.get_mut(node_id).unwrap();
        gossip.process_timed_out_requests(node_id, peer_context);
        assert_eq!(peer_context.requested.len(), 0);
    }

    /// This function tests the functionality to add adverts to the
    /// download manager.
    #[tokio::test]
    async fn download_manager_remove_replica() {
        let logger = p2p_test_setup_logger();
        let num_replicas = 3;
        let num_peers = num_replicas - 1;

        let allocated_ports = allocate_ports("127.0.0.1", num_replicas as u16)
            .expect("Port allocation for test failed");
        let node_port_allocation: Vec<u16> = allocated_ports.iter().map(|np| np.port).collect();
        assert_eq!(num_replicas as usize, node_port_allocation.len());
        let node_port_allocation = Arc::new(node_port_allocation);

        // Create data provider which will have subnet record of all replicas at version
        // 1.
        let data_provider = test_group_set_registry(
            subnet_test_id(P2P_SUBNET_ID_DEFAULT),
            node_port_allocation.clone(),
        );
        let registry_data_provider = data_provider.clone();
        let registry_client = Arc::new(FakeRegistryClient::new(registry_data_provider));
        registry_client.update_to_latest_version();

        // Create consensus cache which returns CUP with version 1, the registry version
        // which contains the subnet record with all replicas.
        let mut mock_consensus_cache = MockConsensusCache::new();
        let consensus_registry_client = registry_client.clone();
        mock_consensus_cache
            .expect_get_oldest_registry_version_in_use()
            .returning(move || consensus_registry_client.get_latest_version());
        let consensus_pool_cache = Arc::new(mock_consensus_cache);

        let gossip = new_test_gossip_impl_with_registry(
            num_replicas,
            &logger,
            Arc::clone(&registry_client) as Arc<_>,
            Arc::clone(&consensus_pool_cache) as Arc<_>,
            tokio::runtime::Handle::current(),
        );
        // Add new subnet record with one less replica and at version 2.
        let node_nums: Vec<u64> = (0..((node_port_allocation.len() - 1) as u64)).collect();
        assert_eq!((num_replicas - 1) as usize, node_nums.len());
        add_subnet_record(
            &data_provider,
            2,
            subnet_test_id(P2P_SUBNET_ID_DEFAULT),
            SubnetRecordBuilder::from(&node_nums.into_iter().map(node_test_id).collect::<Vec<_>>())
                .build(),
        );
        // Get latest subnet record and assert it has one less replica than the initial
        // version.
        registry_client.update_to_latest_version();
        let registry_version = registry_client.get_latest_version();
        let node_records = registry_client
            .get_subnet_transport_infos(subnet_test_id(P2P_SUBNET_ID_DEFAULT), registry_version)
            .unwrap_or(None)
            .unwrap_or_default();
        assert_eq!((num_replicas - 1) as usize, node_records.len());

        // Get removed node
        let peers = gossip.get_current_peer_ids();
        let nodes: HashSet<NodeId> = node_records.iter().map(|node_id| node_id.0).collect();
        let mut removed_peer = node_test_id(10);
        let iter_peers = gossip.get_current_peer_ids();
        for peer in iter_peers.into_iter() {
            if !nodes.contains(&peer) {
                removed_peer = peer;
            }
        }
        assert_ne!(removed_peer, node_test_id(10));
        // Ensure number of peers are the expected amount
        // from registry version 1 (version registry is currently using).
        assert_eq!(num_peers as usize, peers.len());

        // Add adverts from the peer that is removed in the latest registry version
        test_add_adverts(&gossip, 0..5, removed_peer);

        // Refresh registry to get latest version.
        gossip.refresh_registry();
        // Assert number of peers has been decreased by one.
        assert_eq!(
            (num_peers - 1) as usize,
            gossip.get_current_peer_ids().len()
        );

        // Validate adverts from the removed_peer are no longer present.
        for advert_id in 0..5 {
            let advert = gossip.prioritizer.get_advert_from_peer(
                &ArtifactId::FileTreeSync(advert_id.to_string()),
                &CryptoHash(vec![u8::try_from(advert_id).unwrap()]),
                &removed_peer,
            );
            assert_eq!(advert, Err(DownloadPrioritizerError::NotFound));
        }

        // Validate adverts added from removed_peer are rejected
        test_add_adverts(&gossip, 0..5, removed_peer);
        for advert_id in 0..5 {
            let advert = gossip.prioritizer.get_advert_from_peer(
                &ArtifactId::FileTreeSync(advert_id.to_string()),
                &CryptoHash(vec![u8::try_from(advert_id).unwrap()]),
                &removed_peer,
            );
            assert_eq!(advert, Err(DownloadPrioritizerError::NotFound));
        }
    }

    #[tokio::test]
    async fn download_manager_add_adverts() {
        let logger = p2p_test_setup_logger();
        let gossip = new_test_gossip(2, &logger, tokio::runtime::Handle::current());
        test_add_adverts(&gossip, 0..1000, node_test_id(1));
    }

    /// This function asserts that the chunks to be downloaded is correctly
    /// upper bounded, where the upper bound is specified in the gossip
    /// configuration.
    #[tokio::test]
    async fn download_manager_compute_work_basic() {
        let logger = p2p_test_setup_logger();
        let num_replicas = 2;
        let gossip = new_test_gossip(num_replicas, &logger, tokio::runtime::Handle::current());
        test_add_adverts(&gossip, 0..1000, node_test_id(num_replicas as u64 - 1));
        let chunks_to_be_downloaded = gossip
            .download_next_compute_work(node_test_id(num_replicas as u64 - 1))
            .unwrap();
        assert_eq!(
            chunks_to_be_downloaded.len(),
            gossip.gossip_config.max_artifact_streams_per_peer as usize
        );
        for (i, chunk_req) in chunks_to_be_downloaded.iter().enumerate() {
            assert_eq!(
                chunk_req.artifact_id,
                ArtifactId::FileTreeSync(i.to_string())
            );
            assert_eq!(chunk_req.chunk_id, ChunkId::from(0));
        }
    }

    /// This function tests the correct functioning when a single chunk times
    /// out.
    ///
    /// All peers advertise the same set of adverts. A download is initiated at
    /// the first peer. After it times out, the test verifies that the chunk
    /// gets requested from the next peer.
    /// Once the chunk has been requested from every peer, a new round of
    /// download requests begins.
    #[tokio::test]
    async fn download_manager_single_chunked_timeout() {
        // The total number of replicas is 4 in this test.
        let num_replicas = 4;
        let logger = p2p_test_setup_logger();
        let mut gossip = new_test_gossip(num_replicas, &logger, tokio::runtime::Handle::current());
        gossip.gossip_config.max_chunk_wait_ms = 1000;

        let test_assert_compute_work_len =
            |gossip: &GossipImpl, node_id, compute_work_count: usize| {
                let chunks_to_be_downloaded = gossip.download_next_compute_work(node_id).unwrap();
                assert_eq!(chunks_to_be_downloaded.len(), compute_work_count);
                for (i, chunk_req) in chunks_to_be_downloaded.iter().enumerate() {
                    assert_eq!(
                        chunk_req.artifact_id,
                        ArtifactId::FileTreeSync(i.to_string())
                    );
                    assert_eq!(chunk_req.chunk_id, ChunkId::from(0));
                }
            };
        let request_queue_size = gossip.gossip_config.max_artifact_streams_per_peer as usize;

        // Skip the first peer at index 0 as it is the requesting node.
        for peer_id in 1..num_replicas {
            test_add_adverts(
                &gossip,
                0..request_queue_size as u32,
                node_test_id(peer_id as u64),
            );
        }

        for peer_id in 1..num_replicas {
            test_assert_compute_work_len(&gossip, node_test_id(peer_id as u64), request_queue_size);
            for other_peer in 1..num_replicas {
                if other_peer != peer_id {
                    test_assert_compute_work_len(&gossip, node_test_id(other_peer as u64), 0);
                }
            }
            test_timeout_peer(&gossip, &node_test_id(peer_id as u64));
            if peer_id != num_replicas - 1 {
                test_assert_compute_work_len(&gossip, node_test_id(peer_id as u64), 0);
            }
        }

        // All peers have been probed once. Thus, this attempt round is
        // exhausted and download attempts can start afresh.
        for advert_id in 0..request_queue_size as u32 {
            let artifact_id = ArtifactId::FileTreeSync(advert_id.to_string());
            let advert_tracker = gossip
                .prioritizer
                .get_advert_tracker(
                    &artifact_id,
                    &CryptoHash(Vec::from(advert_id.to_be_bytes())),
                )
                .unwrap();
            let mut advert_tracker = advert_tracker.write().unwrap();
            assert!(!advert_tracker.is_attempts_round_complete(ChunkId::from(0)),);
            for peer_id in 0..num_replicas {
                assert!(
                    !advert_tracker.peer_attempted(ChunkId::from(0), &node_test_id(peer_id as u64)),
                );
            }
        }
    }

    /// This functions tests the correct functioning when chunks and artifacts
    /// time out.
    #[tokio::test]
    async fn download_manager_timeout_artifact() {
        // There are 3 nodes in total, Node 1 and 2 are actively used in the test.
        let num_replicas = 3;
        let logger = p2p_test_setup_logger();
        let mut gossip = new_test_gossip(num_replicas, &logger, tokio::runtime::Handle::current());
        gossip.gossip_config.max_artifact_streams_per_peer = 1;
        gossip.gossip_config.max_chunk_wait_ms = 1000;
        let advert_range = 1..num_replicas as u32;
        // Node 1 and 2 both advertise advert 1 and 2.
        for i in 1..num_replicas {
            test_add_adverts(&gossip, advert_range.clone(), node_test_id(i as u64))
        }

        // Advert 1 and 2 are now being downloaded by node 1 and 2.
        for i in 1..num_replicas {
            let chunks_to_be_downloaded = gossip
                .download_next_compute_work(node_test_id(i as u64))
                .unwrap();
            assert_eq!(
                chunks_to_be_downloaded.len(),
                gossip.gossip_config.max_artifact_streams_per_peer as usize
            );
        }

        // Time out the artifact as well as the chunks.
        let sleep_duration =
            std::time::Duration::from_millis((gossip.gossip_config.max_chunk_wait_ms * 2) as u64);
        std::thread::sleep(sleep_duration);

        // Node 1 and 2 now both have moved forward and advertise advert 3 and
        // 4 while advert 1 and 2 have timed out.
        for i in 1..num_replicas {
            test_add_adverts(&gossip, 3..5, node_test_id(i as u64))
        }

        // Test that chunks have timed out.
        for i in 1..num_replicas {
            test_timeout_peer(&gossip, &node_test_id(i as u64))
        }
        // Test that artifacts also have timed out.
        gossip.process_timed_out_artifacts();
        {
            let mut artifacts_under_construction = gossip.artifacts_under_construction.write();

            for i in advert_range {
                assert!(artifacts_under_construction
                    .get_tracker(&CryptoHash(Vec::from(i.to_be_bytes())))
                    .is_none());
            }
        }

        // After advert 1 and 2 have timed out, the download manager must start
        // downloading the next artifacts 3 and 4 now.
        for i in 1..num_replicas {
            let chunks_to_be_downloaded = gossip
                .download_next_compute_work(node_test_id(i as u64))
                .unwrap();
            assert_eq!(
                chunks_to_be_downloaded.len(),
                gossip.gossip_config.max_artifact_streams_per_peer as usize
            );
        }

        {
            let mut artifacts_under_construction = gossip.artifacts_under_construction.write();
            // Advert 1 and 2 timed out, so check adverts starting from 3 exists.
            for counter in 1u32..3 {
                assert!(artifacts_under_construction
                    .get_tracker(&CryptoHash(Vec::from(counter.to_be_bytes())))
                    .is_none());
            }
            for counter in 3u32..5 {
                assert!(artifacts_under_construction
                    .get_tracker(&CryptoHash(Vec::from(counter.to_be_bytes())))
                    .is_some());
            }
        }
    }

    /// The function tests the downloading of an artifact in multiple chunks in
    /// parallel from multiple peers.
    #[tokio::test]
    async fn download_manager_multi_chunked_artifacts_are_linearly_striped() {
        // There are 3 peers in total. 2 peers advertise an artifact with 40 chunks.
        // Each peer has 20 download slots available for transport.
        let num_peers = 3;
        let logger = p2p_test_setup_logger();
        let mut gossip = new_test_gossip(num_peers, &logger, tokio::runtime::Handle::current());
        let request_queue_size = gossip.gossip_config.max_artifact_streams_per_peer;
        gossip.artifact_manager = Arc::new(TestArtifactManager {
            quota: 2 * 1024 * 1024 * 1024,
            num_chunks: request_queue_size * num_peers,
        });

        // Each peer should download the node_id'th range of chunks, i.e.,
        //  Node 1 downloads the chunks 0-19.
        //  Node 2 downloads the chunks 20-39.
        let test_assert_compute_work_is_striped =
            |gossip: &GossipImpl, node_id: NodeId, compute_work_count: u64| {
                let chunks_to_be_downloaded = gossip.download_next_compute_work(node_id).unwrap();
                assert_eq!(chunks_to_be_downloaded.len() as u64, compute_work_count);
                for (i, chunk_req) in chunks_to_be_downloaded.iter().enumerate() {
                    assert_eq!(
                        chunk_req.artifact_id,
                        ArtifactId::FileTreeSync(0.to_string())
                    );
                    let chunk_num =
                        ((node_id_to_u64(node_id) - 1) * request_queue_size as u64) + i as u64;
                    assert_eq!(chunk_req.chunk_id, ChunkId::from(chunk_num as u32));
                }
            };

        // Advertise the artifact from all peers.
        for i in 1..num_peers {
            test_add_adverts(&gossip, 0..1, node_test_id(i as u64))
        }

        for i in 1..num_peers {
            test_assert_compute_work_is_striped(
                &gossip,
                node_test_id(i as u64),
                request_queue_size as u64,
            );
        }
    }

    /// The function returns a simple DKG message which changes according to the
    /// number passed in.
    fn receive_check_test_create_message(number: u32) -> DkgMessage {
        let dkg_id = NiDkgId {
            start_block_height: Height::from(number as u64),
            dealer_subnet: SubnetId::from(PrincipalId::new_subnet_test_id(number as u64)),
            dkg_tag: NiDkgTag::LowThreshold,
            target_subnet: NiDkgTargetSubnet::Local,
        };
        DkgMessage {
            content: DealingContent::new(
                NiDkgDealing::dummy_dealing_for_tests(number as u8),
                dkg_id,
            ),
            signature: BasicSignature::fake(NodeId::from(PrincipalId::new_node_test_id(
                number as u64,
            ))),
        }
    }
    /// The function returns a new chunk with the given chunk ID and artifact
    /// ID. The chunk created differs based on the number provided (same as the
    /// advert).
    fn receive_check_test_create_chunk(
        chunk_id: ChunkId,
        artifact_id: ArtifactId,
        number: u32,
        integrity_hash: CryptoHash,
    ) -> GossipChunk {
        let payload = Artifact::DkgMessage(receive_check_test_create_message(number));
        let artifact_chunk = ArtifactChunk {
            chunk_id,
            witness: Vec::with_capacity(0),
            artifact_chunk_data: ArtifactChunkData::UnitChunkData(payload),
        };

        GossipChunk {
            artifact_id,
            integrity_hash,
            chunk_id,
            artifact_chunk: Ok(artifact_chunk),
        }
    }

    /// The function returns the given number of adverts.
    fn receive_check_test_create_adverts(range: Range<u32>) -> Vec<GossipAdvert> {
        let mut result = vec![];
        for advert_number in range {
            let msg = receive_check_test_create_message(advert_number);
            let artifact_id = CryptoHashOf::from(ic_types::crypto::crypto_hash(&msg).get());
            let attribute = DkgMessageAttribute {
                interval_start_height: Default::default(),
            };
            let gossip_advert = GossipAdvert {
                artifact_id: ArtifactId::DkgMessage(artifact_id),
                attribute: ArtifactAttribute::DkgMessage(attribute),
                size: 0,
                integrity_hash: ic_types::crypto::crypto_hash(&msg).get(),
            };
            result.push(gossip_advert);
        }
        result
    }

    /// The function tests the processing of chunks.
    ///
    /// In particular, it tests that the cache contains the artifact hash(es)
    /// afterwards and that the download manager does not consider
    /// downloading the same artifact(s) again when receiving the corresponding
    /// adverts again.
    ///
    /// This test also validates that adverts with the same artifact ID but
    /// different integrity hashes can be processed from advert -> artifact.
    #[tokio::test]
    async fn receive_check_test() {
        // Initialize the logger and download manager for the test.
        let logger = p2p_test_setup_logger();
        let gossip = new_test_gossip(2, &logger, tokio::runtime::Handle::current());
        let node_id = node_test_id(1);
        let max_adverts = gossip.gossip_config.max_artifact_streams_per_peer;
        let mut adverts = receive_check_test_create_adverts(0..max_adverts);
        let msg = receive_check_test_create_message(0);
        let artifact_id = ArtifactId::DkgMessage(CryptoHashOf::from(
            ic_types::crypto::crypto_hash(&msg).get(),
        ));
        for mut advert in &mut adverts {
            advert.artifact_id = artifact_id.clone();
        }

        for gossip_advert in &adverts {
            gossip.on_advert(gossip_advert.clone(), node_id);
        }
        let chunks_to_be_downloaded = gossip.download_next_compute_work(node_id).unwrap();

        // Add the chunk(s).
        for (index, chunk_req) in chunks_to_be_downloaded.iter().enumerate() {
            // Verify all the chunk requests processed have the same artifact id.
            assert_eq!(chunk_req.artifact_id, artifact_id);
            assert_eq!(
                chunk_req.integrity_hash,
                ic_types::crypto::crypto_hash(&receive_check_test_create_message(index as u32))
                    .get(),
            );
            assert_eq!(chunk_req.chunk_id, ChunkId::from(0));

            let gossip_chunk = receive_check_test_create_chunk(
                chunks_to_be_downloaded[index].chunk_id,
                chunks_to_be_downloaded[index].artifact_id.clone(),
                index as u32,
                chunk_req.integrity_hash.clone(),
            );

            gossip.on_chunk(gossip_chunk, node_id);
        }

        // Test that the cache contains the artifact(s).
        {
            let receive_check_caches = gossip.receive_check_caches.read();
            let cache = &receive_check_caches.get(&node_id).unwrap();
            for gossip_advert in &adverts {
                assert!(cache.contains(&gossip_advert.integrity_hash));
            }
        }

        // Test that the artifact is ignored when providing the same adverts again.
        for gossip_advert in &adverts {
            gossip.on_advert(gossip_advert.clone(), node_id);
        }
        let new_chunks_to_be_downloaded = gossip.download_next_compute_work(node_id).unwrap();

        assert!(new_chunks_to_be_downloaded.is_empty());
    }

    /// This test will verify that artifacts with incorrect integrity hashes
    /// will not be processed.
    #[tokio::test]
    async fn integrity_hash_test() {
        // Initialize the logger and download manager for the test.
        let logger = p2p_test_setup_logger();
        let gossip = new_test_gossip(2, &logger, tokio::runtime::Handle::current());
        let node_id = node_test_id(1);
        let max_adverts = 20;
        let adverts = receive_check_test_create_adverts(0..max_adverts);
        for gossip_advert in &adverts {
            gossip.on_advert(gossip_advert.clone(), node_id);
        }
        let chunks_to_be_downloaded = gossip.download_next_compute_work(node_id).unwrap();

        // Add the chunks with incorrect integrity hash
        for (i, chunk_req) in adverts.iter().enumerate() {
            // Create chunks with different integrity hashes
            let gossip_chunk = receive_check_test_create_chunk(
                chunks_to_be_downloaded[i].chunk_id,
                chunks_to_be_downloaded[i].artifact_id.clone(),
                max_adverts,
                chunk_req.integrity_hash.clone(),
            );

            gossip.on_chunk(gossip_chunk, node_id);
        }

        // Validate that the cache does not contain the artifacts since we put chunks
        // with incorrect integrity hashes
        {
            let receive_check_caches = gossip.receive_check_caches.read();
            let cache = &receive_check_caches.get(&node_id).unwrap();
            for gossip_advert in &adverts {
                assert!(!cache.contains(&gossip_advert.integrity_hash));
            }
        }

        // Validate that the number of integrity check failures is equivalent to the
        // length of the adverts in the incorrect integrity hash bucket
        assert_eq!(
            adverts.len(),
            gossip.metrics.integrity_hash_check_failed.get() as usize
        );
    }
    #[test]
    fn test_get_random_subset_of_peers() {
        let mut current_peers = vec![];
        for id in 1..29 {
            let node_id = node_test_id(id);
            current_peers.push(node_id);
        }
        {
            // Verify 10% of 28 = 3 (rounded up) nodes are returned.
            let ret = get_random_subset_of_peers(current_peers.clone(), Percentage::from(10));
            assert_eq!(ret.len(), 3);
            let mut unique_peers = HashSet::new();
            for entry in &ret {
                assert!(unique_peers.insert(entry));
                assert!(current_peers.contains(entry));
            }
        }

        {
            // Verify all 28 nodes are returned.
            let ret = get_random_subset_of_peers(current_peers.clone(), Percentage::from(100));
            assert_eq!(ret.len(), 28);
            let mut unique_peers = HashSet::new();
            for entry in &ret {
                assert!(unique_peers.insert(entry));
                assert!(current_peers.contains(entry));
            }
        }
        assert!(get_random_subset_of_peers(vec![], Percentage::from(10)).is_empty());
    }

    #[test]
    fn test_get_peer_addr() {
        {
            let node_record: NodeRecord = Default::default();
            let peer_addr = get_peer_addr(&node_record);
            assert!(peer_addr.is_err());
        }
        {
            let mut node_record: NodeRecord = Default::default();
            node_record.p2p_flow_endpoints.push(FlowEndpoint {
                flow_tag: 2000,
                endpoint: Some(ConnectionEndpoint {
                    ip_addr: "2001:db8:0:1:1:1:1:1".to_string(),
                    port: 200,
                    protocol: Protocol::P2p1Tls13 as i32,
                }),
            });

            let peer_addr = get_peer_addr(&node_record).unwrap();
            assert_eq!(
                peer_addr.to_string(),
                "[2001:db8:0:1:1:1:1:1]:200".to_string()
            );
        }
        {
            let mut node_record: NodeRecord = Default::default();
            node_record.p2p_flow_endpoints.push(FlowEndpoint {
                flow_tag: 1000,
                endpoint: Some(ConnectionEndpoint {
                    ip_addr: "2001:db8:0:1:1:1:1:1".to_string(),
                    port: 100,
                    protocol: Protocol::P2p1Tls13 as i32,
                }),
            });
            node_record.p2p_flow_endpoints.push(FlowEndpoint {
                flow_tag: 2000,
                endpoint: Some(ConnectionEndpoint {
                    ip_addr: "2001:db8:0:1:1:1:1:2".to_string(),
                    port: 200,
                    protocol: Protocol::P2p1Tls13 as i32,
                }),
            });

            let peer_addr = get_peer_addr(&node_record).unwrap();
            assert_eq!(
                peer_addr.to_string(),
                "[2001:db8:0:1:1:1:1:1]:100".to_string()
            );
        }
    }
    #[tokio::test]
    async fn test_merge_subnet_membership() {
        let logger = p2p_test_setup_logger();
        let num_replicas = 3;

        let allocated_ports = allocate_ports("127.0.0.1", num_replicas as u16)
            .expect("Port allocation for test failed");
        let node_port_allocation: Vec<u16> = allocated_ports.iter().map(|np| np.port).collect();
        let data_provider = test_group_set_registry(
            subnet_test_id(P2P_SUBNET_ID_DEFAULT),
            Arc::new(node_port_allocation),
        );
        let registry_data_provider = data_provider;
        let registry_client = Arc::new(FakeRegistryClient::new(registry_data_provider));
        registry_client.update_to_latest_version();

        // Create consensus cache that returns a oldest registry version higher than the the local view.
        let mut mock_consensus_cache = MockConsensusCache::new();
        let consensus_registry_client = registry_client.clone();
        mock_consensus_cache
            .expect_get_oldest_registry_version_in_use()
            .returning(move || {
                RegistryVersion::from(consensus_registry_client.get_latest_version().get() + 5)
            });
        let consensus_pool_cache = Arc::new(mock_consensus_cache);

        let gossip = new_test_gossip_impl_with_registry(
            num_replicas,
            &logger,
            Arc::clone(&registry_client) as Arc<_>,
            Arc::clone(&consensus_pool_cache) as Arc<_>,
            tokio::runtime::Handle::current(),
        );
        // Make sure the subnet membership in non-empty
        assert!(!gossip
            .merge_subnet_membership(registry_client.get_latest_version())
            .is_empty())
    }
}
