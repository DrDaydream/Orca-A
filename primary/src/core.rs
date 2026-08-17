// Copyright(C) Facebook, Inc. and its affiliates.
use crate::aggregators::{CertificatesAggregator, GradeVotesAggregator, VotesAggregator};
use crate::error::{DagError, DagResult};
use crate::messages::{
    Certificate, ConsensusCommand, ConsensusMessage, GradeVote, GradedCertificate, Header, Vote,
};
use crate::primary::{PrimaryMessage, Round};
use crate::proposer::ProposerMessage;
use crate::synchronizer::Synchronizer;
use async_recursion::async_recursion;
use bytes::Bytes;
use config::{Committee, Stake};
use crypto::Hash as _;
use crypto::{Digest, PublicKey, SignatureService};
use log::{debug, error, trace, warn};
use network::{CancelHandler, ReliableSender};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel as work_channel, Sender as WorkSender};
use std::sync::{Arc, Mutex};
use store::Store;
use tokio::sync::mpsc::{Receiver, Sender};

#[cfg(test)]
#[path = "tests/core_tests.rs"]
pub mod core_tests;

pub struct Core {
    /// The public key of this primary.
    name: PublicKey,
    /// The committee information.
    committee: Committee,
    /// The persistent storage.
    store: Store,
    /// Handles synchronization with other nodes and our workers.
    synchronizer: Synchronizer,
    /// Service to sign headers.
    signature_service: SignatureService,
    /// The current consensus round (used for cleanup).
    consensus_round: Arc<AtomicU64>,
    /// The depth of the garbage collector.
    gc_depth: Round,

    /// Receiver for dag messages (headers, votes, certificates).
    rx_primaries: Receiver<PrimaryMessage>,
    /// Receives loopback headers from the `HeaderWaiter`.
    rx_header_waiter: Receiver<Header>,
    /// Receives loopback certificates from the `CertificateWaiter`.
    rx_certificate_waiter: Receiver<Certificate>,
    /// Receives our newly created headers from the `Proposer`.
    rx_proposer: Receiver<Header>,
    rx_consensus: Receiver<ConsensusCommand>,
    /// Output all certificates to the consensus layer.
    tx_consensus: Sender<ConsensusMessage>,
    /// Send valid a quorum of certificates' ids to the `Proposer` (along with their round).
    tx_proposer: Sender<ProposerMessage>,

    /// The last garbage collected round.
    gc_round: Round,
    /// The authors of the last voted headers.
    last_voted: HashMap<Round, HashSet<PublicKey>>,
    /// The set of headers we are currently processing.
    processing: HashMap<Round, HashSet<Digest>>,
    /// Every node aggregates votes for every observed header.
    votes_aggregators: HashMap<Digest, VotesAggregator>,
    /// Valid votes that arrived before their header.
    pending_votes: HashMap<Digest, Vec<Vote>>,
    /// Aggregates certificates to use as parents for new headers.
    certificates_aggregators: HashMap<Round, Box<CertificatesAggregator>>,
    /// Certificates delivered at GRBC grade 1, indexed by their digest.
    grbc_certificates: HashMap<Digest, Certificate>,
    /// Valid author-signed headers seen before any certificate/grade forms.
    observed_headers: HashMap<Digest, Header>,
    /// Grade-1 acknowledgements collected by the certificate origin.
    grade_aggregators: HashMap<Digest, GradeVotesAggregator>,
    /// Valid READY/grade-1 messages that arrived before their certificate.
    pending_grade_votes: HashMap<Digest, Vec<GradeVote>>,
    /// Unique READY senders and bound metadata used by the f+1 relay rule.
    ready_support: HashMap<Digest, (Round, PublicKey, HashSet<PublicKey>, Stake)>,
    /// Certificates for which this primary already emitted a grade vote.
    grade_voted: HashSet<Digest>,
    /// Certificates carrying a verified grade-2 proof.
    grade_two: HashSet<Digest>,
    /// Grade-2 blocks waiting for weak causal dependencies before they may
    /// contribute to the round-advance quorum.
    round_advance_pending: HashMap<Digest, Certificate>,
    /// Grade-2 blocks not yet referenced by a weak edge.
    weak_edge_candidates: HashMap<Digest, Round>,
    /// A network sender to send the batches to the other workers.
    network: ReliableSender,
    /// Keeps the cancel handlers of the messages we sent.
    cancel_handlers: HashMap<Round, Vec<CancelHandler>>,
}

impl Core {
    #[allow(clippy::too_many_arguments)]
    pub fn spawn(
        name: PublicKey,
        committee: Committee,
        store: Store,
        synchronizer: Synchronizer,
        signature_service: SignatureService,
        consensus_round: Arc<AtomicU64>,
        gc_depth: Round,
        rx_primaries: Receiver<PrimaryMessage>,
        rx_header_waiter: Receiver<Header>,
        rx_certificate_waiter: Receiver<Certificate>,
        rx_proposer: Receiver<Header>,
        rx_consensus: Receiver<ConsensusCommand>,
        tx_consensus: Sender<ConsensusMessage>,
        tx_proposer: Sender<ProposerMessage>,
    ) {
        tokio::spawn(async move {
            Self {
                name,
                committee,
                store,
                synchronizer,
                signature_service,
                consensus_round,
                gc_depth,
                rx_primaries,
                rx_header_waiter,
                rx_certificate_waiter,
                rx_proposer,
                rx_consensus,
                tx_consensus,
                tx_proposer,
                gc_round: 0,
                last_voted: HashMap::with_capacity(2 * gc_depth as usize),
                processing: HashMap::with_capacity(2 * gc_depth as usize),
                votes_aggregators: HashMap::new(),
                pending_votes: HashMap::new(),
                certificates_aggregators: HashMap::with_capacity(2 * gc_depth as usize),
                grbc_certificates: HashMap::new(),
                observed_headers: HashMap::new(),
                grade_aggregators: HashMap::new(),
                pending_grade_votes: HashMap::new(),
                ready_support: HashMap::new(),
                grade_voted: HashSet::new(),
                grade_two: HashSet::new(),
                round_advance_pending: HashMap::new(),
                weak_edge_candidates: HashMap::new(),
                network: ReliableSender::new(),
                cancel_handlers: HashMap::with_capacity(2 * gc_depth as usize),
            }
            .run()
            .await;
        });
    }

    async fn observe_header(&mut self, header: &Header) {
        if self
            .observed_headers
            .insert(header.id.clone(), header.clone())
            .is_none()
        {
            self.tx_consensus
                .send(ConsensusMessage::Observed(header.clone()))
                .await
                .expect("Failed to send observed header to consensus");
        }

        if let Some(votes) = self.pending_votes.remove(&header.id) {
            for vote in votes {
                self.process_vote(vote).await.expect("Invalid pending vote");
            }
        }
    }

    async fn process_consensus_command(&mut self, command: ConsensusCommand) -> DagResult<()> {
        match command {
            ConsensusCommand::Cleanup(_) | ConsensusCommand::CleanupBatch(_) => unreachable!(),
            ConsensusCommand::LeaderRequest(round, leader) => {
                let addresses = self
                    .committee
                    .others_primaries(&self.name)
                    .iter()
                    .map(|(_, x)| x.primary_to_primary)
                    .collect();
                let request = PrimaryMessage::LeaderRequest(round, leader, self.name);
                let bytes =
                    bincode::serialize(&request).expect("Failed to serialize leader request");
                let handlers = self.network.broadcast(addresses, Bytes::from(bytes)).await;
                self.cancel_handlers
                    .entry(round)
                    .or_default()
                    .extend(handlers);
            }
        }
        Ok(())
    }

    async fn process_leader_request(
        &mut self,
        round: Round,
        leader: PublicKey,
        requestor: PublicKey,
    ) -> DagResult<()> {
        let header = self
            .observed_headers
            .values()
            .find(|header| header.round == round && header.author == leader)
            .cloned();
        if let Some(header) = header {
            if let Ok(address) = self.committee.primary(&requestor) {
                // A signed header is sufficient for observation/recovery; no
                // quorum certificate or GRBC grade is required.
                let bytes = bincode::serialize(&PrimaryMessage::Header(header))
                    .expect("Failed to serialize leader response");
                let handler = self
                    .network
                    .send(address.primary_to_primary, Bytes::from(bytes))
                    .await;
                self.cancel_handlers.entry(round).or_default().push(handler);
            }
        }
        Ok(())
    }

    async fn process_own_header(&mut self, header: Header) -> DagResult<()> {
        // Broadcast the new header in a reliable manner.
        let addresses = self
            .committee
            .others_primaries(&self.name)
            .iter()
            .map(|(_, x)| x.primary_to_primary)
            .collect();
        let bytes = bincode::serialize(&PrimaryMessage::Header(header.clone()))
            .expect("Failed to serialize our own header");
        let handlers = self.network.broadcast(addresses, Bytes::from(bytes)).await;
        self.cancel_handlers
            .entry(header.round)
            .or_insert_with(Vec::new)
            .extend(handlers);

        // Process the header.
        self.process_header(&header).await
    }

    #[async_recursion]
    async fn process_header(&mut self, header: &Header) -> DagResult<()> {
        debug!("Processing {:?}", header);
        // Callers reach this point only after Header::verify (or for a locally
        // signed header). Observation deliberately precedes parent/payload sync.
        self.observe_header(header).await;
        // Indicate that we are processing this header.
        self.processing
            .entry(header.round)
            .or_insert_with(HashSet::new)
            .insert(header.id.clone());

        // Ensure we have the parents. If at least one parent is missing, the synchronizer returns an empty
        // vector; it will gather the missing parents (as well as all ancestors) from other nodes and then
        // reschedule processing of this header.
        let referenced = self.synchronizer.get_parents(header).await?;
        if referenced.is_empty() {
            debug!("Processing of {} suspended: missing parent(s)", header.id);
            return Ok(());
        }

        // Check the parent certificates. Ensure the parents form a quorum and are all from the previous round.
        let mut stake = 0;
        for x in referenced
            .iter()
            .filter(|x| header.parents.contains(&x.digest()))
        {
            ensure!(
                x.round() + 1 == header.round,
                DagError::MalformedHeader(header.id.clone())
            );
            stake += self.committee.stake(&x.origin());
        }
        ensure!(
            stake >= self.committee.quorum_threshold(),
            DagError::HeaderRequiresQuorum(header.id.clone())
        );

        // Weak edges must point strictly below the previous round.
        for x in referenced
            .iter()
            .filter(|x| header.weak_edges.contains(&x.digest()))
        {
            ensure!(
                x.round() + 1 < header.round,
                DagError::MalformedHeader(header.id.clone())
            );
        }

        // Virtual edges reference grade-1 blocks left in VDag at the end of
        // the immediately preceding round.
        for x in referenced
            .iter()
            .filter(|x| header.virtual_edges.contains(&x.digest()))
        {
            ensure!(
                x.round() + 1 == header.round,
                DagError::MalformedHeader(header.id.clone())
            );
        }

        // Ensure we have the payload. If we don't, the synchronizer will ask our workers to get it, and then
        // reschedule processing of this header once we have it.
        if self.synchronizer.missing_payload(header).await? {
            debug!("Processing of {} suspended: missing payload", header);
            return Ok(());
        }

        // Store the header.
        let bytes = bincode::serialize(header).expect("Failed to serialize header");
        self.store.write(header.id.to_vec(), bytes).await;

        // Check if we can vote for this header.
        if self
            .last_voted
            .entry(header.round)
            .or_insert_with(HashSet::new)
            .insert(header.author)
        {
            // Ordinary VOTE is an all-to-all GRBC message. Every node can
            // therefore collect a quorum and form the certificate locally.
            let vote = Vote::new(header, &self.name, &mut self.signature_service).await;
            debug!("Created {:?}", vote);
            let addresses = self
                .committee
                .others_primaries(&self.name)
                .iter()
                .map(|(_, authority)| authority.primary_to_primary)
                .collect();
            let bytes = bincode::serialize(&PrimaryMessage::Vote(vote.clone()))
                .expect("Failed to serialize GRBC vote");
            let handlers = self.network.broadcast(addresses, Bytes::from(bytes)).await;
            self.cancel_handlers
                .entry(header.round)
                .or_insert_with(Vec::new)
                .extend(handlers);
            self.process_vote(vote).await?;
        }
        // A HeaderWaiter loopback also signals that optional weak references
        // requested earlier may now be stored. Recheck only the Grade-2 blocks
        // that were explicitly waiting for those dependencies.
        self.retry_round_advance_pending().await?;
        Ok(())
    }

    #[async_recursion]
    async fn process_vote(&mut self, vote: Vote) -> DagResult<()> {
        debug!("Processing {:?}", vote);

        let header = match self.observed_headers.get(&vote.id).cloned() {
            Some(header) => header,
            None => {
                let pending = self
                    .pending_votes
                    .entry(vote.id.clone())
                    .or_insert_with(Vec::new);
                if !pending
                    .iter()
                    .any(|candidate| candidate.author == vote.author)
                {
                    pending.push(vote);
                }
                return Ok(());
            }
        };

        // Add it to the votes' aggregator and try to make a new certificate.
        if let Some(certificate) = self
            .votes_aggregators
            .entry(header.id.clone())
            .or_insert_with(VotesAggregator::new)
            .append(vote, &self.committee, &header)?
        {
            debug!("Assembled {:?}", certificate);

            // Broadcast the certificate.
            let addresses = self
                .committee
                .others_primaries(&self.name)
                .iter()
                .map(|(_, x)| x.primary_to_primary)
                .collect();
            let bytes = bincode::serialize(&PrimaryMessage::Certificate(certificate.clone()))
                .expect("Failed to serialize our own certificate");
            let handlers = self.network.broadcast(addresses, Bytes::from(bytes)).await;
            self.cancel_handlers
                .entry(certificate.round())
                .or_insert_with(Vec::new)
                .extend(handlers);

            // Process the new certificate.
            self.process_certificate(certificate)
                .await
                .expect("Failed to process valid certificate");
        }
        Ok(())
    }

    #[async_recursion]
    async fn process_certificate(&mut self, certificate: Certificate) -> DagResult<()> {
        debug!("Processing {:?}", certificate);

        // The certificate has already passed signature/quorum verification at
        // the network boundary (or was assembled locally). Expose it before
        // ancestor synchronization so rules 1 and 3 can observe every GRBC stage.
        self.observe_header(&certificate.header).await;

        // Process the header embedded in the certificate if we haven't already voted for it (if we already
        // voted, it means we already processed it). Since this header got certified, we are sure that all
        // the data it refers to (ie. its payload and its parents) are available. We can thus continue the
        // processing of the certificate even if we don't have them in store right now.
        if !self
            .processing
            .get(&certificate.header.round)
            .map_or_else(|| false, |x| x.contains(&certificate.header.id))
        {
            // This function may still throw an error if the storage fails.
            self.process_header(&certificate.header).await?;
        }

        // Ensure we have all the ancestors of this certificate yet. If we don't, the synchronizer will gather
        // them and trigger re-processing of this certificate.
        if !self.synchronizer.deliver_certificate(&certificate).await? {
            debug!(
                "Processing of {:?} suspended: missing ancestors",
                certificate
            );
            return Ok(());
        }

        let digest = certificate.digest();

        // A looped-back or retransmitted certificate must not be delivered twice.
        if self.grbc_certificates.contains_key(&digest) {
            // The certificate waiter also loops Grade-2 blocks back once weak
            // dependencies arrive. GRBC delivery is already complete, but the
            // block may now become eligible for round advancement.
            if self.grade_two.contains(&digest) {
                self.try_advance_grade_two(certificate).await?;
            }
            self.retry_round_advance_pending().await?;
            return Ok(());
        }

        // Store the certificate. This is the grade-1 delivery point of GRBC.
        let bytes = bincode::serialize(&certificate).expect("Failed to serialize certificate");
        self.store.write(digest.to_vec(), bytes).await;
        self.grbc_certificates
            .insert(digest.clone(), certificate.clone());

        // Grade 1 enters VDag only; it is not inserted into Tusk's Dag yet.
        let id = certificate.header.id.clone();
        if let Err(e) = self
            .tx_consensus
            .send(ConsensusMessage::GradeOne(certificate.clone()))
            .await
        {
            warn!(
                "Failed to deliver certificate {} to the consensus: {}",
                id, e
            );
        }

        // Grade-1 is the READY step of GRBC. Every node broadcasts its READY,
        // then locally delivers grade 2 after collecting a quorum. Deliver is
        // a local event and is never broadcast again.
        if self.grade_voted.insert(digest) {
            let vote = GradeVote::new(&certificate, &self.name, &mut self.signature_service).await;
            self.broadcast_grade_vote(&vote).await;
            self.process_grade_vote(vote).await?;
        }

        // READY messages may race ahead of the certificate over independent
        // network connections. They were signature-checked on arrival.
        if let Some(votes) = self.pending_grade_votes.remove(&certificate.digest()) {
            for vote in votes {
                self.process_grade_vote(vote).await?;
            }
        }
        self.retry_round_advance_pending().await?;
        Ok(())
    }

    async fn broadcast_grade_vote(&mut self, vote: &GradeVote) {
        let addresses = self
            .committee
            .others_primaries(&self.name)
            .iter()
            .map(|(_, authority)| authority.primary_to_primary)
            .collect();
        let bytes = bincode::serialize(&PrimaryMessage::GradeVote(vote.clone()))
            .expect("Failed to serialize GRBC READY");
        let handlers = self.network.broadcast(addresses, Bytes::from(bytes)).await;
        self.cancel_handlers
            .entry(vote.round)
            .or_insert_with(Vec::new)
            .extend(handlers);
    }

    #[async_recursion]
    async fn process_grade_vote(&mut self, vote: GradeVote) -> DagResult<()> {
        let author_stake = self.committee.stake(&vote.author);
        let support = self
            .ready_support
            .entry(vote.id.clone())
            .or_insert_with(|| (vote.round, vote.origin, HashSet::new(), 0));
        ensure!(
            support.0 == vote.round && support.1 == vote.origin,
            DagError::UnexpectedGradeVote(vote.id.clone())
        );
        if support.2.insert(vote.author) {
            support.3 += author_stake;
        }
        let support_stake = support.3;

        // Bracha READY amplification: f+1 matching READY messages guarantee
        // that at least one honest node sent READY. Relay once even when the
        // certificate itself has not arrived yet.
        if support_stake >= self.committee.validity_threshold()
            && self.grade_voted.insert(vote.id.clone())
        {
            let relay = GradeVote::new_for(
                vote.id.clone(),
                vote.round,
                vote.origin,
                &self.name,
                &mut self.signature_service,
            )
            .await;
            self.broadcast_grade_vote(&relay).await;
            self.process_grade_vote(relay).await?;
        }

        let certificate = match self.grbc_certificates.get(&vote.id).cloned() {
            Some(certificate) => certificate,
            None => {
                let pending = self
                    .pending_grade_votes
                    .entry(vote.id.clone())
                    .or_insert_with(Vec::new);
                if !pending
                    .iter()
                    .any(|candidate| candidate.author == vote.author)
                {
                    pending.push(vote);
                }
                return Ok(());
            }
        };
        let digest = vote.id.clone();
        let proof = self
            .grade_aggregators
            .entry(digest.clone())
            .or_insert_with(GradeVotesAggregator::new)
            .append(vote, &self.committee, &certificate)?;

        if let Some(proof) = proof {
            trace!("Locally delivered GRBC grade 2 for {:?}", certificate);
            self.process_graded_certificate(proof).await?;
        }
        Ok(())
    }

    async fn process_graded_certificate(&mut self, proof: GradedCertificate) -> DagResult<()> {
        self.observe_header(&proof.certificate.header).await;
        let digest = proof.certificate.digest();
        if self.grade_two.insert(digest.clone()) {
            trace!("GRBC grade 2 delivered for {:?}", proof.certificate);
            self.tx_consensus
                .send(ConsensusMessage::GradeTwo(proof.certificate.clone()))
                .await
                .expect("Failed to send grade-2 certificate to consensus");
            self.weak_edge_candidates
                .insert(digest, proof.certificate.round());
            self.try_advance_grade_two(proof.certificate).await?;
        }
        Ok(())
    }

    /// Grade-2 delivery is local and immediate, but a block contributes to the
    /// strong-parent quorum only after all strong and weak dependencies exist.
    async fn try_advance_grade_two(&mut self, certificate: Certificate) -> DagResult<()> {
        if !self
            .synchronizer
            .ready_for_round_advance(&certificate)
            .await?
        {
            self.round_advance_pending
                .insert(certificate.digest(), certificate);
            return Ok(());
        }
        self.round_advance_pending.remove(&certificate.digest());
        if let Some(parents) = self
            .certificates_aggregators
            .entry(certificate.round())
            .or_insert_with(|| Box::new(CertificatesAggregator::new()))
            .append(certificate.clone(), &self.committee)?
        {
            let round = certificate.round();
            let virtual_edges = self
                .grbc_certificates
                .iter()
                .filter(|(digest, certificate)| {
                    certificate.round() == round && !self.grade_two.contains(*digest)
                })
                .map(|(digest, _)| digest.clone())
                .collect();
            let weak_edges: Vec<_> = self
                .weak_edge_candidates
                .iter()
                .filter(|(_, block_round)| **block_round < round)
                .map(|(digest, _)| digest.clone())
                .collect();
            for digest in &parents {
                self.weak_edge_candidates.remove(digest);
            }
            for digest in &weak_edges {
                self.weak_edge_candidates.remove(digest);
            }
            self.tx_consensus
                .send(ConsensusMessage::RoundAdvanced(round + 1))
                .await
                .expect("Failed to notify consensus of round advancement");
            self.tx_proposer
                .send((parents, weak_edges, virtual_edges, round))
                .await
                .expect("Failed to send GRBC edges to proposer");
        }
        Ok(())
    }

    async fn retry_round_advance_pending(&mut self) -> DagResult<()> {
        let pending: Vec<_> = self.round_advance_pending.values().cloned().collect();
        for certificate in pending {
            self.try_advance_grade_two(certificate).await?;
        }
        Ok(())
    }

    fn sanitize_header(&mut self, header: &Header) -> DagResult<()> {
        ensure!(
            self.gc_round <= header.round,
            DagError::TooOld(header.id.clone(), header.round)
        );

        // Verify the header's signature.
        header.verify(&self.committee)?;

        // TODO [issue #3]: Prevent bad nodes from sending junk headers with high round numbers.

        Ok(())
    }

    fn sanitize_vote(&mut self, vote: &Vote) -> DagResult<()> {
        ensure!(
            self.gc_round <= vote.round,
            DagError::TooOld(vote.digest(), vote.round)
        );

        // Verify the vote.
        vote.verify(&self.committee).map_err(DagError::from)
    }

    fn sanitize_certificate(&mut self, certificate: &Certificate) -> DagResult<()> {
        ensure!(
            self.gc_round <= certificate.round(),
            DagError::TooOld(certificate.digest(), certificate.round())
        );

        // Verify the certificate (and the embedded header).
        certificate.verify(&self.committee).map_err(DagError::from)
    }

    fn verify_grade_vote(
        committee: &Committee,
        gc_round: Round,
        vote: &GradeVote,
    ) -> DagResult<()> {
        ensure!(
            gc_round <= vote.round,
            DagError::TooOld(vote.id.clone(), vote.round)
        );
        ensure!(
            committee.stake(&vote.origin) > 0,
            DagError::UnknownAuthority(vote.origin)
        );
        vote.verify(committee)
    }

    fn sanitize_graded_certificate(&mut self, proof: &GradedCertificate) -> DagResult<()> {
        ensure!(
            self.gc_round <= proof.certificate.round(),
            DagError::TooOld(proof.certificate.digest(), proof.certificate.round())
        );
        proof.verify(&self.committee)
    }

    // Main loop listening to incoming messages.
    pub async fn run(&mut self) {
        // READY signature verification is CPU work and may complete out of
        // arrival order. Only verified results re-enter this single state
        // machine, keeping support aggregation deterministic and race-free.
        let (tx_verified_ready, mut rx_verified_ready) =
            tokio::sync::mpsc::unbounded_channel::<(GradeVote, DagResult<()>)>();
        type ReadyJob = (Round, GradeVote);
        let worker_count = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(2)
            .min(4)
            .max(1);
        // Keep the CPU concurrency fixed while allowing Core to accept READY
        // bursts without blocking the GRBC state machine.
        let (tx_ready_jobs, rx_ready_jobs) = work_channel::<ReadyJob>();
        let rx_ready_jobs = Arc::new(Mutex::new(rx_ready_jobs));
        let verification_committee = Arc::new(self.committee.clone());
        for _ in 0..worker_count {
            let jobs = rx_ready_jobs.clone();
            let results = tx_verified_ready.clone();
            let committee = verification_committee.clone();
            std::thread::spawn(move || loop {
                let job = jobs.lock().expect("READY verifier queue poisoned").recv();
                let (gc_round, vote) = match job {
                    Ok(job) => job,
                    Err(_) => break,
                };
                let result = Self::verify_grade_vote(&committee, gc_round, &vote);
                if results.send((vote, result)).is_err() {
                    break;
                }
            });
        }
        let submit_ready = |vote: GradeVote, gc_round: Round, jobs: &WorkSender<ReadyJob>| {
            jobs.send((gc_round, vote))
                .expect("READY verifier pool stopped");
        };
        loop {
            let result = tokio::select! {
                // We receive here messages from other primaries.
                Some(message) = self.rx_primaries.recv() => {
                    match message {
                        PrimaryMessage::Header(header) => {
                            match self.sanitize_header(&header) {
                                Ok(()) => self.process_header(&header).await,
                                error => error
                            }

                        },
                        PrimaryMessage::Vote(vote) => {
                            match self.sanitize_vote(&vote) {
                                Ok(()) => self.process_vote(vote).await,
                                error => error
                            }
                        },
                        PrimaryMessage::Certificate(certificate) => {
                            match self.sanitize_certificate(&certificate) {
                                Ok(()) =>  self.process_certificate(certificate).await,
                                error => error
                            }
                        },
                        PrimaryMessage::GradeVote(vote) => {
                            submit_ready(vote, self.gc_round, &tx_ready_jobs);
                            Ok(())
                        },
                        PrimaryMessage::GradedCertificate(proof) => {
                            match self.sanitize_graded_certificate(&proof) {
                                Ok(()) => {
                                    self.process_graded_certificate(proof).await
                                },
                                error => error
                            }
                        },
                        PrimaryMessage::LeaderRequest(round, leader, requestor) => {
                            self.process_leader_request(round, leader, requestor).await
                        },
                        _ => panic!("Unexpected core message")
                    }
                },

                // We receive here loopback headers from the `HeaderWaiter`. Those are headers for which we interrupted
                // execution (we were missing some of their dependencies) and we are now ready to resume processing.
                Some(header) = self.rx_header_waiter.recv() => self.process_header(&header).await,

                // We receive here loopback certificates from the `CertificateWaiter`. Those are certificates for which
                // we interrupted execution (we were missing some of their ancestors) and we are now ready to resume
                // processing.
                Some(certificate) = self.rx_certificate_waiter.recv() => self.process_certificate(certificate).await,

                // We also receive here our new headers created by the `Proposer`.
                Some(header) = self.rx_proposer.recv() => self.process_own_header(header).await,

                Some(command) = self.rx_consensus.recv() => {
                    self.process_consensus_command(command).await
                },
                Some((vote, verification)) = rx_verified_ready.recv() => {
                    match verification {
                        Ok(()) => self.process_grade_vote(vote).await,
                        Err(error) => Err(error),
                    }
                },
            };
            match result {
                Ok(()) => (),
                Err(DagError::StoreError(e)) => {
                    error!("{}", e);
                    panic!("Storage failure: killing node.");
                }
                Err(e @ DagError::TooOld(..)) => debug!("{}", e),
                Err(e) => warn!("{}", e),
            }

            // Cleanup internal state.
            let round = self.consensus_round.load(Ordering::Relaxed);
            if round > self.gc_depth {
                let gc_round = round - self.gc_depth;
                self.last_voted.retain(|k, _| k >= &gc_round);
                self.processing.retain(|k, _| k >= &gc_round);
                let live_headers: HashSet<_> = self
                    .observed_headers
                    .iter()
                    .filter(|(_, header)| header.round >= gc_round)
                    .map(|(digest, _)| digest.clone())
                    .collect();
                self.votes_aggregators
                    .retain(|digest, _| live_headers.contains(digest));
                self.pending_votes
                    .retain(|_, votes| votes.iter().any(|vote| vote.round >= gc_round));
                self.certificates_aggregators.retain(|k, _| k >= &gc_round);
                self.grbc_certificates.retain(|_, x| x.round() >= gc_round);
                self.observed_headers.retain(|_, x| x.round >= gc_round);
                let live: HashSet<_> = self.grbc_certificates.keys().cloned().collect();
                self.grade_aggregators.retain(|k, _| live.contains(k));
                self.pending_grade_votes
                    .retain(|_, votes| votes.iter().any(|vote| vote.round >= gc_round));
                self.ready_support
                    .retain(|_, (round, _, _, _)| *round >= gc_round);
                self.grade_voted.retain(|k| live.contains(k));
                self.grade_two.retain(|k| live.contains(k));
                self.weak_edge_candidates
                    .retain(|_, round| *round >= gc_round);
                self.cancel_handlers.retain(|k, _| k >= &gc_round);
                self.gc_round = gc_round;
            }
        }
    }
}
