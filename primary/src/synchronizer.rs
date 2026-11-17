// Copyright(C) Facebook, Inc. and its affiliates.
use crate::error::{DagError, DagResult};
use crate::header_waiter::WaiterMessage;
use crate::messages::{proposal_header_key, Certificate, Header};
use crate::primary::Round;
use config::Committee;
use crypto::Hash as _;
use crypto::{Digest, PublicKey};
use std::collections::{BTreeSet, HashMap, HashSet};
use store::Store;
use tokio::sync::mpsc::Sender;

/// Checks proposal dependencies and requests missing records through HeaderWaiter.
pub struct Synchronizer {
    /// The persistent storage.
    store: Store,
    /// Send commands to the `HeaderWaiter`.
    tx_header_waiter: Sender<WaiterMessage>,
    /// The genesis and its digests.
    genesis: Vec<(Digest, Certificate)>,
}

impl Synchronizer {
    pub fn new(
        committee: &Committee,
        store: Store,
        tx_header_waiter: Sender<WaiterMessage>,
    ) -> Self {
        Self {
            store,
            tx_header_waiter,
            genesis: Certificate::genesis(committee)
                .into_iter()
                .map(|x| (x.digest(), x))
                .collect(),
        }
    }

    /// Returns the parents of a header if we have them all. If at least one parent is missing,
    /// we return an empty vector, synchronize with other nodes, and re-schedule processing
    /// of the header for when we will have all the parents.
    pub async fn get_parents(
        &mut self,
        header: &Header,
        ready: &HashMap<Digest, Certificate>,
        completed: &HashSet<Digest>,
    ) -> DagResult<Vec<Certificate>> {
        let mut missing_required = Vec::new();
        let mut missing_virtual = Vec::new();
        let mut parents = Vec::new();
        for digest in header.parents.iter().chain(&header.weak_edges) {
            // READY metadata alone cannot satisfy a strong/weak dependency.
            // Reuse Core's immutable record only after local completion.
            if let Some(parent) = ready
                .get(digest)
                .filter(|c| completed.contains(&c.header.id))
            {
                parents.push(parent.clone());
                continue;
            }
            if let Some(genesis) = self
                .genesis
                .iter()
                .find(|(x, _)| x == digest)
                .map(|(_, x)| x)
            {
                parents.push(genesis.clone());
                continue;
            }

            match self.store.read(digest.to_vec()).await? {
                Some(certificate) => parents.push(bincode::deserialize(&certificate)?),
                None => missing_required.push(digest.clone()),
            };
        }
        for digest in &header.virtual_edges {
            if let Some(parent) = ready.get(digest) {
                parents.push(parent.clone());
                continue;
            }
            if let Some((_, genesis)) = self
                .genesis
                .iter()
                .find(|(candidate, _)| candidate == digest)
            {
                parents.push(genesis.clone());
                continue;
            }
            // Validate the target's signed metadata, not its local grade. One
            // honest node reaching READY does not imply every peer can reach
            // READY, so requiring a local grade here would block honest proposals.
            match self.store.read(proposal_header_key(digest)).await? {
                Some(bytes) => parents.push(Certificate {
                    header: bincode::deserialize(&bytes)?,
                    votes: Vec::new(),
                }),
                None => missing_virtual.push(digest.clone()),
            }
        }
        if !missing_required.is_empty() || !missing_virtual.is_empty() {
            self.tx_header_waiter
                .send(WaiterMessage::SyncParents(
                    missing_required,
                    missing_virtual,
                    header.clone(),
                ))
                .await
                .expect("Failed to send sync parents request");
            return Ok(Vec::new());
        }
        Ok(parents)
    }

    pub async fn ready_for_round_advance(
        &mut self,
        certificate: &Certificate,
        ready: &HashMap<Digest, Certificate>,
        completed: &HashSet<Digest>,
    ) -> DagResult<bool> {
        for digest in certificate
            .header
            .parents
            .iter()
            .chain(&certificate.header.weak_edges)
        {
            if ready
                .get(digest)
                .map_or(false, |c| completed.contains(&c.header.id))
            {
                continue;
            }
            if self
                .genesis
                .iter()
                .any(|(candidate, _)| candidate == digest)
            {
                continue;
            }
            if self.store.read(digest.to_vec()).await?.is_none() {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub async fn committed(&mut self, blocks: Vec<(Digest, Digest)>) {
        if !blocks.is_empty() {
            self.tx_header_waiter
                .send(WaiterMessage::Committed(blocks))
                .await
                .expect("Failed to clear committed dependency requests");
        }
    }

    /// Fill the weak-edge budget with old, not-yet-output vertices absent from
    /// this parent snapshot's causal history. Never follow virtual evidence.
    pub async fn select_weak_parents(
        &mut self,
        parent_round: Round,
        parents: &[Digest],
        candidates: &BTreeSet<(Round, PublicKey, Digest)>,
        limit: usize,
        ready: &HashMap<Digest, Certificate>,
        completed: &HashSet<Digest>,
    ) -> DagResult<Vec<Digest>> {
        let oldest = match candidates.iter().next() {
            Some((round, _, _)) if *round < parent_round && limit > 0 => *round,
            _ => return Ok(Vec::new()),
        };
        let mut covered = HashSet::new();
        let mut pending = parents.to_vec();
        while let Some(digest) = pending.pop() {
            if !covered.insert(digest.clone()) {
                continue;
            }
            // Borrow the usual hot-path metadata. The disk fallback is only
            // needed after metadata GC or consensus completion without READY.
            let stored;
            let certificate = if let Some(certificate) = ready
                .get(&digest)
                .filter(|c| completed.contains(&c.header.id))
            {
                certificate
            } else if let Some((_, genesis)) = self.genesis.iter().find(|(id, _)| id == &digest) {
                genesis
            } else {
                let bytes = self
                    .store
                    .read(digest.to_vec())
                    .await?
                    .ok_or_else(|| DagError::MalformedHeader(digest.clone()))?;
                stored = bincode::deserialize::<Certificate>(&bytes)?;
                ensure!(stored.digest() == digest, DagError::MalformedHeader(digest));
                &stored
            };
            // Every non-output completion registers a candidate; only exact
            // output removes one, including across GC. A COMPLETE vertex absent
            // from candidates is therefore an output frontier: its strong/weak
            // ancestors cannot contain an unoutput candidate. READY alone must
            // never supply this inference. No second output ledger is needed.
            if certificate.round() > oldest
                && candidates.contains(&(certificate.round(), certificate.origin(), digest))
            {
                pending.extend(
                    certificate
                        .header
                        .parents
                        .iter()
                        .chain(&certificate.header.weak_edges)
                        .cloned(),
                );
            }
        }
        Ok(candidates
            .iter()
            .take_while(|(round, _, _)| *round < parent_round)
            .filter(|(_, _, digest)| !covered.contains(digest))
            .take(limit)
            .map(|(_, _, digest)| digest.clone())
            .collect())
    }
}
