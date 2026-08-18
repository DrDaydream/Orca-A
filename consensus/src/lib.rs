// Copyright(C) Facebook, Inc. and its affiliates.
use config::{Committee, Stake};
use crypto::Hash as _;
use crypto::{Digest, PublicKey};
use log::{debug, info, log_enabled, trace, warn};
use primary::{Certificate, ConsensusCommand, ConsensusMessage, Round};
use std::cmp::max;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::time::{self, Duration};

/// Keep diagnostic logging deterministic while cutting its hot-path volume in
/// half. Benchmark `info!` records and all warnings/errors remain unsampled.
macro_rules! sampled_debug {
    ($round:expr, $($arg:tt)*) => {
        if $round % 2 == 0 {
            debug!($($arg)*);
        }
    };
}

#[cfg(test)]
#[path = "tests/consensus_tests.rs"]
pub mod consensus_tests;

/// The representation of the DAG in memory.
type Dag = HashMap<Round, HashMap<PublicKey, (Digest, Certificate)>>;

/// The validated DAG contains blocks delivered by GRBC at grade 1.
///
/// A block is inserted exactly once when the primary first sends its certificate
/// to consensus. A later grade-2 upgrade does not create another VDag node.
type VDag = HashMap<Round, HashMap<PublicKey, (Digest, Certificate)>>;

/// The state that needs to be persisted for crash-recovery.
struct State {
    /// The last committed round.
    last_committed_round: Round,
    // Keeps the last committed round for each authority. This map is used to clean up the dag and
    // ensure we don't commit twice the same certificate.
    last_committed: HashMap<PublicKey, Round>,
    /// Keeps the latest committed certificate (and its parents) for every authority. Anything older
    /// must be regularly cleaned up through the function `update`.
    dag: Dag,
    /// Direct lookup used by causal traversal. This keeps `order_dag` linear
    /// in the number of visited vertices and edges.
    dag_by_digest: HashMap<Digest, Certificate>,
    /// Blocks locally delivered by GRBC with grade 1.
    vdag: VDag,
    /// Blocks for which a valid grade-2 proof has been delivered.
    grade_two: HashSet<Digest>,
    /// Digest index of blocks already present in the formal Dag.
    dag_digests: HashSet<Digest>,
    /// Every signature-verified certificate seen at any GRBC stage.
    observed: HashMap<Digest, Certificate>,
    /// O(1) lookup for an observed block selected by round and author.
    observed_by_round: HashMap<Round, HashMap<PublicKey, Digest>>,
    /// Incrementally maintained strong-path transitive closure and reverse
    /// dependency index. New ancestors propagate only to affected children.
    strong_ancestors: HashMap<Digest, HashSet<Digest>>,
    strong_children: HashMap<Digest, HashSet<Digest>>,
    /// Per-target support sets updated when a new edge/ancestor becomes known.
    observed_strong_support: HashMap<(Round, Digest), HashSet<PublicKey>>,
    dag_strong_support: HashMap<(Round, Digest), HashSet<PublicKey>>,
    observed_direct_support: HashMap<(Round, Digest), HashSet<PublicKey>>,
    dag_direct_support: HashMap<(Round, Digest), HashSet<PublicKey>>,
    /// Number of strong/weak dependencies not yet admitted to Dag.
    missing_dependencies: HashMap<Digest, usize>,
    /// Reverse waiters wake only VDag blocks affected by a new Dag insertion.
    dependency_waiters: HashMap<Digest, HashSet<Digest>>,
    /// Grade-2 blocks whose dependency count reached zero.
    promotion_queue: VecDeque<Digest>,
    /// The authority designated as leader for every round.
    leaders: HashMap<Round, PublicKey>,
    /// Leader rounds already committed, preventing duplicate commits.
    committed_leaders: HashSet<Round>,
    /// Leader rounds explicitly skipped by commit rule 3.
    skipped_leaders: HashSet<Round>,
    /// Commit-ready leaders waiting for the previous round's leader.
    pending_leaders: BTreeMap<Round, Certificate>,
    /// Wall-clock time at which a leader first completed a commit rule. This
    /// intentionally excludes predecessor and output-channel waiting.
    rule_ready_at_ms: HashMap<Round, u128>,
    /// First commit rule that made each leader ready (1, 2, or 3).
    leader_commit_rules: HashMap<Round, u8>,
    /// DAG sequences prepared as soon as a leader completes a rule, while
    /// predecessor ordering continues independently.
    pending_order: HashMap<Round, Vec<Certificate>>,
    /// Pending rounds whose direct predecessor is already committed/skipped.
    /// This avoids repeatedly scanning the complete pending map.
    ready_pending: BTreeSet<Round>,
    /// Commit-ready rule-3 observers split into the three independent r+3x
    /// backtracking chains. Index is `round % 3`.
    rule_three_stacks: [BTreeSet<Round>; 3],
    /// Rule-3 leaders whose data is being recovered from GRBC/other nodes.
    rule_three_recovery: HashSet<Round>,
    missing_leader_requests: HashSet<Round>,
    /// Causal-history digests authorized by a successful rule-3 recovery but
    /// not observed locally yet, mapped to the leader rounds they unblock.
    forced_history_waiters: HashMap<Digest, HashSet<Round>>,
    /// Historical leaders that must be re-evaluated because a specific event
    /// (leader/history arrival or a future ABA output) changed their inputs.
    dirty_leaders: HashSet<Round>,
    /// Highest local round whose intermediate rule-1/rule-2 checks ran.
    highest_advanced_round: Round,
    /// Non-blocking handoff to the single ordered cleanup/application writer.
    commit_tx: Option<mpsc::UnboundedSender<Vec<Certificate>>>,
}

impl State {
    fn new(genesis: Vec<Certificate>) -> Self {
        let genesis = genesis
            .into_iter()
            .map(|x| (x.origin(), (x.digest(), x)))
            .collect::<HashMap<_, _>>();

        let genesis_dag: Dag = [(0, genesis)].iter().cloned().collect();

        let dag_by_digest: HashMap<_, _> = genesis_dag
            .values()
            .flat_map(|authorities| authorities.values())
            .map(|(digest, certificate)| (digest.clone(), certificate.clone()))
            .collect();
        let dag_digests = dag_by_digest.keys().cloned().collect();
        let observed = genesis_dag
            .values()
            .flat_map(|authorities| authorities.values())
            .map(|(digest, certificate)| (digest.clone(), certificate.clone()))
            .collect();
        let observed_by_round = genesis_dag
            .iter()
            .map(|(round, authorities)| {
                (
                    *round,
                    authorities
                        .iter()
                        .map(|(authority, (digest, _))| (*authority, digest.clone()))
                        .collect(),
                )
            })
            .collect();

        Self {
            last_committed_round: 0,
            last_committed: genesis_dag
                .get(&0)
                .unwrap()
                .iter()
                .map(|(x, (_, y))| (*x, y.round()))
                .collect(),
            dag: genesis_dag,
            dag_by_digest,
            // Genesis blocks already belong to the ordering DAG, so they must
            // not also appear in VDag.
            vdag: HashMap::new(),
            grade_two: HashSet::new(),
            dag_digests,
            observed,
            observed_by_round,
            strong_ancestors: HashMap::new(),
            strong_children: HashMap::new(),
            observed_strong_support: HashMap::new(),
            dag_strong_support: HashMap::new(),
            observed_direct_support: HashMap::new(),
            dag_direct_support: HashMap::new(),
            missing_dependencies: HashMap::new(),
            dependency_waiters: HashMap::new(),
            promotion_queue: VecDeque::new(),
            leaders: HashMap::new(),
            committed_leaders: [0].iter().cloned().collect(),
            skipped_leaders: HashSet::new(),
            pending_leaders: BTreeMap::new(),
            rule_ready_at_ms: HashMap::new(),
            leader_commit_rules: HashMap::new(),
            pending_order: HashMap::new(),
            ready_pending: BTreeSet::new(),
            rule_three_stacks: [BTreeSet::new(), BTreeSet::new(), BTreeSet::new()],
            rule_three_recovery: HashSet::new(),
            missing_leader_requests: HashSet::new(),
            forced_history_waiters: HashMap::new(),
            dirty_leaders: HashSet::new(),
            highest_advanced_round: 1,
            commit_tx: None,
        }
    }

    fn record_rule_ready(&mut self, round: Round) {
        self.rule_ready_at_ms.entry(round).or_insert_with(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("System clock is before Unix epoch")
                .as_millis()
        });
    }

    fn observe(&mut self, certificate: Certificate) -> HashSet<Round> {
        let digest = certificate.digest();
        self.observed_by_round
            .entry(certificate.round())
            .or_insert_with(HashMap::new)
            .insert(certificate.origin(), digest.clone());
        match self.observed.entry(digest.clone()) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                // Preserve the newest GRBC-stage representation (notably its
                // votes), but do not repeat waiter lookup/history promotion
                // for a digest that was already observed.
                entry.insert(certificate);
                return HashSet::new();
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(certificate.clone());
            }
        }
        self.index_strong_paths(&certificate);
        let owners = self
            .forced_history_waiters
            .remove(&digest)
            .unwrap_or_default();
        for owner_round in &owners {
            self.force_observed_history_to_dag(certificate.clone(), *owner_round);
        }
        owners
    }

    fn index_strong_paths(&mut self, certificate: &Certificate) {
        let digest = certificate.digest();
        let mut additions = HashSet::new();
        for parent in &certificate.header.parents {
            self.observed_direct_support
                .entry((certificate.round(), parent.clone()))
                .or_default()
                .insert(certificate.origin());
            self.strong_children
                .entry(parent.clone())
                .or_default()
                .insert(digest.clone());
            additions.insert(parent.clone());
            if let Some(ancestors) = self.strong_ancestors.get(parent) {
                additions.extend(ancestors.iter().cloned());
            }
        }
        self.propagate_strong_ancestors(digest, additions);
    }

    fn propagate_strong_ancestors(&mut self, source: Digest, additions: HashSet<Digest>) {
        let mut pending = vec![(source, additions)];
        while let Some((digest, candidates)) = pending.pop() {
            let ancestors = self.strong_ancestors.entry(digest.clone()).or_default();
            let fresh: HashSet<_> = candidates
                .into_iter()
                .filter(|ancestor| ancestors.insert(ancestor.clone()))
                .collect();
            if fresh.is_empty() {
                continue;
            }
            if let Some(block) = self.observed.get(&digest) {
                let round = block.round();
                let origin = block.origin();
                for ancestor in &fresh {
                    self.observed_strong_support
                        .entry((round, ancestor.clone()))
                        .or_default()
                        .insert(origin);
                    if self.dag_digests.contains(&digest) {
                        self.dag_strong_support
                            .entry((round, ancestor.clone()))
                            .or_default()
                            .insert(origin);
                    }
                }
            }
            if let Some(children) = self.strong_children.get(&digest).cloned() {
                for child in children {
                    pending.push((child, fresh.clone()));
                }
            }
        }
    }

    /// Rule 3 accepts verified GRBC data without waiting for grade 1/2. Insert
    /// the root and every currently known causal ancestor directly into Dag.
    fn force_observed_history_to_dag(&mut self, root: Certificate, owner_round: Round) {
        let mut pending = vec![root];
        let mut visited = HashSet::new();
        while let Some(certificate) = pending.pop() {
            let digest = certificate.digest();
            if !visited.insert(digest.clone()) {
                continue;
            }
            for dependency in certificate
                .header
                .parents
                .iter()
                .chain(&certificate.header.weak_edges)
                .chain(&certificate.header.virtual_edges)
            {
                // Dag membership is digest-idempotent. Shared strong/weak
                // ancestors commonly occur in several commit-ready histories;
                // never enqueue or insert an ancestor that is already present.
                if self.dag_digests.contains(dependency) {
                    continue;
                } else if let Some(ancestor) = self.observed.get(dependency).cloned() {
                    pending.push(ancestor);
                } else {
                    self.forced_history_waiters
                        .entry(dependency.clone())
                        .or_insert_with(HashSet::new)
                        .insert(owner_round);
                }
            }
            if !self.dag_digests.contains(&digest) {
                self.promote_to_dag(certificate);
            }
        }
    }

    /// Insert a block delivered by GRBC at grade 1 into the validated DAG.
    fn insert_grade_one(&mut self, certificate: Certificate) -> (HashSet<Round>, bool) {
        let round = certificate.round();
        let origin = certificate.origin();
        let digest = certificate.digest();

        // A commit-ready leader/history block may already have been promoted
        // directly from an earlier GRBC observation.  Grade 1 must never move
        // it back to VDag (or leave a duplicate in both structures).
        if self.dag_digests.contains(&digest) {
            return (HashSet::new(), false);
        }
        let inserted = self
            .vdag
            .get(&round)
            .and_then(|blocks| blocks.get(&origin))
            .map_or(true, |(candidate, _)| candidate != &digest);
        if inserted {
            let missing: HashSet<_> = certificate
                .header
                .parents
                .iter()
                .chain(&certificate.header.weak_edges)
                .filter(|dependency| !self.dag_digests.contains(*dependency))
                .cloned()
                .collect();
            for dependency in &missing {
                self.dependency_waiters
                    .entry(dependency.clone())
                    .or_default()
                    .insert(digest.clone());
            }
            self.missing_dependencies
                .insert(digest.clone(), missing.len());
            if missing.is_empty() && self.grade_two.contains(&digest) {
                self.promotion_queue.push_back(digest.clone());
            }
        }
        self.vdag
            .entry(round)
            .or_insert_with(HashMap::new)
            .insert(origin, (digest, certificate.clone()));

        // Observe only after the VDag insertion. If this digest is awaited by
        // a commit-ready leader, observe() promotes it immediately and
        // promote_to_dag() removes the just-inserted VDag copy.
        (self.observe(certificate), inserted)
    }

    /// Promote a grade-1 block into Tusk's ordering DAG. A block contained in
    /// Dag must never remain in VDag.
    fn promote_to_dag(&mut self, certificate: Certificate) {
        let round = certificate.round();
        let origin = certificate.origin();
        let digest = certificate.digest();

        self.observed_by_round
            .entry(round)
            .or_insert_with(HashMap::new)
            .insert(origin, digest.clone());
        self.observed
            .entry(digest.clone())
            .or_insert_with(|| certificate.clone());

        if let Some(authorities) = self.vdag.get_mut(&round) {
            let same_block = authorities
                .get(&origin)
                .map_or(false, |(vdag_digest, _)| vdag_digest == &digest);
            if same_block {
                authorities.remove(&origin);
            }
            if authorities.is_empty() {
                self.vdag.remove(&round);
            }
        }

        self.dag
            .entry(round)
            .or_insert_with(HashMap::new)
            .insert(origin, (digest.clone(), certificate.clone()));
        self.dag_by_digest.insert(digest.clone(), certificate);
        if !self.dag_digests.insert(digest.clone()) {
            return;
        }
        if let Some(ancestors) = self.strong_ancestors.get(&digest) {
            for ancestor in ancestors {
                self.dag_strong_support
                    .entry((round, ancestor.clone()))
                    .or_default()
                    .insert(origin);
            }
        }
        for parent in &self
            .dag_by_digest
            .get(&digest)
            .expect("new Dag block missing from digest index")
            .header
            .parents
        {
            self.dag_direct_support
                .entry((round, parent.clone()))
                .or_default()
                .insert(origin);
        }
        if let Some(waiters) = self.dependency_waiters.remove(&digest) {
            for waiter in waiters {
                if let Some(missing) = self.missing_dependencies.get_mut(&waiter) {
                    *missing = missing.saturating_sub(1);
                    if *missing == 0 && self.grade_two.contains(&waiter) {
                        self.promotion_queue.push_back(waiter);
                    }
                }
            }
        }
        self.wake_pending(round);
    }

    fn mark_grade_two(&mut self, digest: Digest) -> bool {
        let inserted = self.grade_two.insert(digest.clone());
        if inserted
            && self.missing_dependencies.get(&digest) == Some(&0)
            && self.observed.contains_key(&digest)
        {
            self.promotion_queue.push_back(digest);
        }
        inserted
    }

    /// Event-driven VDag promotion. A dependency insertion decrements only its
    /// direct waiters and queues newly ready grade-2 blocks.
    fn promote_ready(&mut self) -> Vec<Certificate> {
        let mut promoted = Vec::new();
        while let Some(digest) = self.promotion_queue.pop_front() {
            if self.dag_digests.contains(&digest)
                || !self.grade_two.contains(&digest)
                || self.missing_dependencies.get(&digest) != Some(&0)
            {
                continue;
            }
            let certificate = match self.observed.get(&digest).cloned() {
                Some(certificate) => certificate,
                None => continue,
            };
            self.promote_to_dag(certificate.clone());
            self.missing_dependencies.remove(&digest);
            promoted.push(certificate);
        }
        promoted
    }

    fn predecessor_resolved(&self, round: Round) -> bool {
        round > 0
            && (self.committed_leaders.contains(&(round - 1))
                || self.skipped_leaders.contains(&(round - 1)))
    }

    fn wake_pending(&mut self, round: Round) {
        if self.pending_leaders.contains_key(&round) && self.predecessor_resolved(round) {
            self.ready_pending.insert(round);
        }
    }

    fn mark_skipped(&mut self, round: Round) -> bool {
        let inserted = self.skipped_leaders.insert(round);
        if inserted {
            self.wake_pending(round + 1);
        }
        inserted
    }

    /// Update commit watermarks and garbage-collect once for the complete
    /// ordered sequence. Repeating full-map retain for every certificate made
    /// a large commit batch quadratic even after `order_dag` became linear.
    fn update(&mut self, certificates: &[Certificate], gc_depth: Round) {
        for certificate in certificates {
            self.last_committed
                .entry(certificate.origin())
                .and_modify(|r| *r = max(*r, certificate.round()))
                .or_insert_with(|| certificate.round());
        }

        let last_committed_round = *self.last_committed.values().max().unwrap();
        self.last_committed_round = last_committed_round;

        let last_committed = &self.last_committed;
        self.dag.retain(|r, authorities| {
            authorities.retain(|name, _| last_committed.get(name).map_or(true, |round| r >= round));
            !authorities.is_empty() && *r + gc_depth >= last_committed_round
        });
        self.vdag.retain(|r, authorities| {
            authorities.retain(|name, _| last_committed.get(name).map_or(true, |round| r >= round));
            !authorities.is_empty() && *r + gc_depth >= last_committed_round
        });
        self.observed
            .retain(|_, certificate| certificate.round() + gc_depth >= last_committed_round);
        let observed_digests: HashSet<_> = self.observed.keys().cloned().collect();
        self.strong_ancestors
            .retain(|digest, _| observed_digests.contains(digest));
        self.strong_children.retain(|digest, children| {
            children.retain(|child| observed_digests.contains(child));
            observed_digests.contains(digest) || !children.is_empty()
        });
        self.observed_strong_support
            .retain(|(round, _), _| *round + gc_depth >= last_committed_round);
        self.dag_strong_support
            .retain(|(round, _), _| *round + gc_depth >= last_committed_round);
        self.observed_direct_support
            .retain(|(round, _), _| *round + gc_depth >= last_committed_round);
        self.dag_direct_support
            .retain(|(round, _), _| *round + gc_depth >= last_committed_round);
        let live_digests: HashSet<_> = self
            .dag
            .values()
            .flat_map(|authorities| authorities.values())
            .map(|(digest, _)| digest.clone())
            .collect();
        self.dag_by_digest
            .retain(|digest, _| live_digests.contains(digest));
        self.dag_digests = live_digests;
        self.missing_dependencies
            .retain(|digest, _| observed_digests.contains(digest));
        self.dependency_waiters.retain(|_, waiters| {
            waiters.retain(|digest| observed_digests.contains(digest));
            !waiters.is_empty()
        });
        self.promotion_queue
            .retain(|digest| observed_digests.contains(digest));
        let dag_digests = &self.dag_digests;
        self.forced_history_waiters
            .retain(|digest, _| !dag_digests.contains(digest));
    }
}

pub struct Consensus {
    /// The committee information.
    committee: Committee,
    /// The depth of the garbage collector.
    gc_depth: Round,

    /// Receives new certificates from the primary. The primary should send us new certificates only
    /// if it already sent us its whole history.
    rx_primary: Receiver<ConsensusMessage>,
    /// Outputs the sequence of ordered certificates to the primary (for cleanup and feedback).
    tx_primary: Sender<ConsensusCommand>,
    /// Outputs the sequence of ordered certificates to the application layer.
    tx_output: OutputSender,

    /// The genesis certificates.
    genesis: Vec<Certificate>,
}

#[derive(Clone)]
enum OutputSender {
    Individual(Sender<Certificate>),
    Batch(Sender<Vec<Certificate>>),
}

impl Consensus {
    fn adversarial_schedule_enabled() -> bool {
        std::env::var("ORCA_FAULTS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .map_or(false, |faults| faults > 0)
    }

    fn scheduled_rule(&self, round: Round) -> u8 {
        if !Self::adversarial_schedule_enabled() {
            return 0;
        }
        let index = round.saturating_sub(1);
        ((index + index / 3) % 3 + 1) as u8
    }

    fn mark_rule_three_skipped(&self, round: Round, state: &mut State) {
        if state.mark_skipped(round) {
            #[cfg(feature = "benchmark")]
            info!(
                "Commit rule stats leader round-{} rule 3 outcome skip blocks 0",
                round
            );
        }
    }

    pub fn spawn(
        committee: Committee,
        gc_depth: Round,
        rx_primary: Receiver<ConsensusMessage>,
        tx_primary: Sender<ConsensusCommand>,
        tx_output: Sender<Certificate>,
    ) {
        tokio::spawn(async move {
            Self {
                committee: committee.clone(),
                gc_depth,
                rx_primary,
                tx_primary,
                tx_output: OutputSender::Individual(tx_output),
                genesis: Certificate::genesis(&committee),
            }
            .run()
            .await;
        });
    }

    /// Production entry point: one application-channel send per ordered DAG
    /// sequence. The certificate-at-a-time API remains available to tests and
    /// existing embedders through `spawn`.
    pub fn spawn_batch(
        committee: Committee,
        gc_depth: Round,
        rx_primary: Receiver<ConsensusMessage>,
        tx_primary: Sender<ConsensusCommand>,
        tx_output: Sender<Vec<Certificate>>,
    ) {
        tokio::spawn(async move {
            Self {
                committee: committee.clone(),
                gc_depth,
                rx_primary,
                tx_primary,
                tx_output: OutputSender::Batch(tx_output),
                genesis: Certificate::genesis(&committee),
            }
            .run()
            .await;
        });
    }

    async fn run(&mut self) {
        // The consensus state (everything else is immutable).
        let mut state = State::new(self.genesis.clone());

        // Consensus decides and updates its deterministic state without ever
        // waiting for bounded cleanup/application channels. One writer task
        // preserves the exact sequence in which commit batches are enqueued.
        let (commit_tx, mut commit_rx) = mpsc::unbounded_channel::<Vec<Certificate>>();
        state.commit_tx = Some(commit_tx);
        let tx_cleanup = self.tx_primary.clone();
        let tx_output = self.tx_output.clone();
        tokio::spawn(async move {
            while let Some(sequence) = commit_rx.recv().await {
                if tx_cleanup
                    .send(ConsensusCommand::CleanupBatch(sequence.clone()))
                    .await
                    .is_err()
                {
                    warn!("Commit cleanup channel closed");
                    return;
                }
                let failed = match &tx_output {
                    OutputSender::Batch(sender) => sender.send(sequence).await.is_err(),
                    OutputSender::Individual(sender) => {
                        let mut failed = false;
                        for certificate in sequence {
                            if sender.send(certificate).await.is_err() {
                                failed = true;
                                break;
                            }
                        }
                        failed
                    }
                };
                if failed {
                    warn!("Application output channel closed");
                    return;
                }
            }
        });

        // Keep ingestion independent from rule evaluation and commit output.
        // Rule 1/2 checks can walk local history and a successful check may
        // wait for the cleanup/output channels.  Draining the bounded Primary
        // channel in a dedicated task prevents either operation from applying
        // backpressure to GRBC block reception.  The single consumer below
        // still processes messages in FIFO order, so consensus state remains
        // deterministic.
        let (_placeholder_tx, placeholder_rx) = mpsc::channel(1);
        let mut primary_rx = std::mem::replace(&mut self.rx_primary, placeholder_rx);
        let (tx_ingress, mut rx_ingress) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(message) = primary_rx.recv().await {
                if tx_ingress.send(message).is_err() {
                    break;
                }
            }
        });

        // Retry recovery requests: a peer may not yet have observed the leader
        // when the first request reaches it.
        let mut recovery_tick = time::interval(Duration::from_secs(1));
        recovery_tick.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

        // Listen to incoming certificates and recovery timers.
        loop {
            let message = tokio::select! {
                message = rx_ingress.recv() => match message {
                    Some(message) => message,
                    None => break,
                },
                _ = recovery_tick.tick() => {
                    self.retry_missing_leaders(&state).await;
                    self.log_pending_blockers(&state);
                    continue;
                }
            };
            if let ConsensusMessage::RoundAdvanced(round) = message {
                self.advance_commit_checks(round, &mut state).await;
                continue;
            }
            let (
                observed_round,
                observed_origin,
                observed_digest,
                dirtied_by_history,
                promoted,
                first_observation,
            ) = match message {
                ConsensusMessage::RoundAdvanced(_) => unreachable!(),
                ConsensusMessage::Observed(header) => {
                    if header.round % 2 == 0 {
                        trace!("Observed valid pre-certificate block {:?}", header);
                    }
                    let round = header.round;
                    let origin = header.author;
                    // This is a local block container, not a quorum
                    // certificate: votes intentionally remain empty.
                    let block = Certificate {
                        header,
                        votes: Vec::new(),
                    };
                    let digest = block.digest();
                    let first = !state.observed.contains_key(&digest);
                    let dirty = state.observe(block);
                    (round, origin, digest, dirty, Vec::new(), first)
                }
                ConsensusMessage::GradeOne(certificate) => {
                    if certificate.round() % 2 == 0 {
                        trace!("Grade 1 delivered {:?}", certificate);
                    }
                    let round = certificate.round();
                    let origin = certificate.origin();
                    let digest = certificate.digest();
                    let first = !state.observed.contains_key(&digest);
                    let (dirty, _) = state.insert_grade_one(certificate);
                    (round, origin, digest, dirty, state.promote_ready(), first)
                }
                ConsensusMessage::GradeTwo(certificate) => {
                    if certificate.round() % 2 == 0 {
                        trace!("Grade 2 delivered {:?}", certificate);
                    }
                    let round = certificate.round();
                    let origin = certificate.origin();
                    let digest = certificate.digest();
                    let first = !state.observed.contains_key(&digest);
                    let dirty = state.observe(certificate.clone());
                    state.mark_grade_two(digest.clone());
                    (round, origin, digest, dirty, state.promote_ready(), first)
                }
            };

            self.refresh_pending_orders(&dirtied_by_history, &mut state);
            state.dirty_leaders.extend(dirtied_by_history);
            // A designated leader arriving after its normal checking round is
            // a precise reason to re-evaluate that leader, without scanning
            // unrelated history.
            if observed_origin == self.ordering_leader_authority(observed_round) {
                state.dirty_leaders.insert(observed_round);
            }

            // A leader that is already commit-ready/recovering no longer has
            // to wait for grade 1 or grade 2. As soon as verified GRBC data is
            // observed, place it and every available causal ancestor in Dag.
            self.promote_observed_pending_leader(observed_round, &mut state);

            // Designation happens as soon as the round is observed, even if
            // none of its grade-1 blocks is ready to enter Dag yet.
            let designated = self.leader_authority(observed_round);
            if state.leaders.insert(observed_round, designated).is_none() {
                if observed_round % 2 == 0 {
                    debug!("Round {} designated leader {}", observed_round, designated);
                }
            }

            // Re-evaluate only when a support set can actually change. A
            // repeated GRBC stage for the same digest updates its proof but
            // cannot add another authority to an observed-support set.
            let mut rule_one_rounds = BTreeSet::new();
            let mut rule_two_rounds = BTreeSet::new();
            if first_observation {
                if let Some(block) = state.observed.get(&observed_digest) {
                    if self.directly_supports_previous_leader(block, &state) {
                        rule_one_rounds.insert(observed_round);
                    }
                }
                rule_two_rounds.insert(observed_round);
            }
            for block in promoted {
                if self.directly_supports_previous_leader(&block, &state) {
                    rule_one_rounds.insert(block.round());
                }
                rule_two_rounds.insert(block.round());
            }
            for round in rule_one_rounds {
                self.evaluate_commit_rule_one(round, &mut state).await;
            }
            for round in rule_two_rounds {
                self.evaluate_commit_rule_two(round, &mut state).await;
            }
            self.process_dirty_leaders(&mut state).await;
        }
    }

    fn directly_supports_previous_leader(&self, block: &Certificate, state: &State) -> bool {
        if block.round() < 2 {
            return false;
        }
        self.observed_leader(block.round() - 1, state)
            .map_or(false, |leader| {
                block.header.parents.contains(&leader.digest())
            })
    }

    /// When a lagging node jumps to a higher round, evaluate every crossed
    /// round in order. Late data still reawakens individual leaders through
    /// `dirty_leaders`, so each jump range itself is processed only once.
    async fn advance_commit_checks(&mut self, target_round: Round, state: &mut State) {
        while state.highest_advanced_round < target_round {
            state.highest_advanced_round += 1;
            let round = state.highest_advanced_round;
            self.evaluate_commit_rule_one(round, state).await;
            self.evaluate_commit_rule_two(round, state).await;
        }
        self.process_dirty_leaders(state).await;
    }

    /// Returns the certificate (and the certificate's digest) originated by the leader of the
    /// specified round (if any).
    fn leader<'a>(&self, round: Round, dag: &'a Dag) -> Option<&'a (Digest, Certificate)> {
        // TODO: We should elect the leader of round r-2 using the common coin revealed at round r.
        // At this stage, we are guaranteed to have 2f+1 certificates from round r (which is enough to
        // compute the coin). We currently just use round-robin.
        let leader = self.ordering_leader_authority(round);

        // Return its certificate and the certificate's digest.
        dag.get(&round).map(|x| x.get(&leader)).flatten()
    }

    /// Deterministically designates one authority as leader for every round.
    /// Keeping this separate from `leader` means a round has a designated
    /// leader even when that authority's certificate has not arrived yet.
    fn leader_authority(&self, round: Round) -> PublicKey {
        let mut keys: Vec<_> = self.committee.authorities.keys().cloned().collect();
        keys.sort();

        let coin = round;

        keys[coin as usize % self.committee.size()]
    }

    /// Returns `(observed_stake, dag_stake)` for round-`round` blocks that
    /// strongly reference `leader_digest`. Observed support is the union of
    /// Dag and VDag and counts each authority at most once.
    fn strong_support_stake(
        &self,
        round: Round,
        leader_digest: &Digest,
        state: &State,
    ) -> (Stake, Stake) {
        let empty = HashSet::new();
        let dag_supporters = state
            .dag_direct_support
            .get(&(round, leader_digest.clone()))
            .unwrap_or(&empty);
        let observed_supporters = state
            .observed_direct_support
            .get(&(round, leader_digest.clone()))
            .unwrap_or(&empty);

        let observed_stake = observed_supporters
            .iter()
            .map(|authority| self.committee.stake(authority))
            .sum();
        let dag_stake = dag_supporters
            .iter()
            .map(|authority| self.committee.stake(authority))
            .sum();
        (observed_stake, dag_stake)
    }

    /// Evaluate commit rule 1 using round `r` as support for the leader of
    /// round `r-1`. A leader already marked commit-ready is never rechecked.
    async fn evaluate_commit_rule_one(&mut self, r: Round, state: &mut State) {
        if r < 2 {
            return;
        }
        let leader_round = r - 1;
        if Self::adversarial_schedule_enabled() && self.scheduled_rule(leader_round) != 1 {
            return;
        }
        if state.committed_leaders.contains(&leader_round)
            || state.pending_leaders.contains_key(&leader_round)
        {
            return;
        }
        let leader = match self.observed_leader(leader_round, state) {
            Some(leader) => leader,
            None => return,
        };
        let leader_digest = leader.digest();

        let (observed_stake, dag_stake) = self.strong_support_stake(r, &leader_digest, state);
        if observed_stake < self.committee.quorum_threshold()
            && dag_stake < self.committee.validity_threshold()
        {
            return;
        }

        sampled_debug!(
            leader_round,
            "Leader {:?} satisfies commit rule 1: observed {}, Dag {}",
            leader,
            observed_stake,
            dag_stake
        );
        // Rule 1 accepts the leader from any verified GRBC stage. Once the
        // quorum condition succeeds, recover it and its causal history into Dag.
        state.force_observed_history_to_dag(leader.clone(), leader_round);
        self.queue_leader_commit(leader, 1, state).await;
    }

    /// Evaluate commit rule 2 for observed round `q` and leader round `q-2`.
    async fn evaluate_commit_rule_two(&mut self, q: Round, state: &mut State) {
        if q < 3 {
            return;
        }
        let leader_round = q - 2;
        if Self::adversarial_schedule_enabled() && self.scheduled_rule(leader_round) != 2 {
            return;
        }
        if state.committed_leaders.contains(&leader_round)
            || state.pending_leaders.contains_key(&leader_round)
        {
            return;
        }

        let leader_authority = self.ordering_leader_authority(leader_round);
        let leader = state
            .dag
            .get(&leader_round)
            .and_then(|round| round.get(&leader_authority))
            .or_else(|| {
                state
                    .vdag
                    .get(&leader_round)
                    .and_then(|round| round.get(&leader_authority))
            })
            .map(|(_, certificate)| certificate.clone());
        let leader = match leader {
            Some(leader) => leader,
            None => return,
        };
        let leader_digest = leader.digest();
        let (observed_strong, dag_strong, dag_strong_or_virtual) =
            self.rule_two_support_stake(q, &leader_digest, state);

        let condition_one = observed_strong >= self.committee.quorum_threshold();
        let condition_two = dag_strong >= self.committee.validity_threshold();
        let condition_three = dag_strong_or_virtual >= self.committee.quorum_threshold();
        if !condition_one && !condition_two && !condition_three {
            return;
        }

        if condition_three && !state.grade_two.contains(&leader_digest) {
            sampled_debug!(
                leader_round,
                "Commit rule 2 forces grade 2 for {:?}",
                leader
            );
            state.mark_grade_two(leader_digest.clone());
            state.promote_ready();
        }

        // Conditions 1 and 2 normally operate on a grade-2 Leader already in
        // Dag. Condition 3 may promote it immediately above. If dependencies
        // still prevent promotion, the ordered pending queue retains it.
        sampled_debug!(leader_round,
            "Leader {:?} satisfies commit rule 2: observed-strong {}, Dag-strong {}, Dag-strong-or-virtual {}",
            leader, observed_strong, dag_strong, dag_strong_or_virtual
        );
        self.queue_leader_commit(leader, 2, state).await;
    }

    /// Counts rule-2 support in round `q`, with each authority counted once.
    fn rule_two_support_stake(
        &self,
        q: Round,
        leader_digest: &Digest,
        state: &State,
    ) -> (Stake, Stake, Stake) {
        let empty = HashSet::new();
        let dag_strong = state
            .dag_strong_support
            .get(&(q, leader_digest.clone()))
            .unwrap_or(&empty);
        let mut dag_strong_or_virtual = HashSet::new();
        let blocks: Vec<_> = state
            .dag
            .get(&q)
            .into_iter()
            .flat_map(|round| round.values())
            .map(|(_, block)| block)
            .collect();
        // At larger committee sizes, independent virtual-path checks are CPU
        // bound. Compute them in parallel over immutable state; merge results
        // back into the ordered consensus state on this thread.
        if blocks.len() >= 8 {
            let workers = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .min(blocks.len());
            let chunk_size = (blocks.len() + workers - 1) / workers;
            std::thread::scope(|scope| {
                let mut handles = Vec::new();
                for chunk in blocks.chunks(chunk_size) {
                    handles.push(scope.spawn(move || {
                        chunk
                            .iter()
                            .filter_map(|block| {
                                let origin = block.origin();
                                (dag_strong.contains(&origin)
                                    || self.has_two_hop_virtual_path(block, leader_digest, state))
                                .then_some(origin)
                            })
                            .collect::<Vec<_>>()
                    }));
                }
                for handle in handles {
                    dag_strong_or_virtual
                        .extend(handle.join().expect("Virtual-path worker panicked"));
                }
            });
        } else {
            for block in blocks {
                let origin = block.origin();
                if dag_strong.contains(&origin)
                    || self.has_two_hop_virtual_path(block, leader_digest, state)
                {
                    dag_strong_or_virtual.insert(origin);
                }
            }
        }

        let observed_strong = state
            .observed_strong_support
            .get(&(q, leader_digest.clone()))
            .unwrap_or(&empty);

        let stake = |authorities: &HashSet<PublicKey>| {
            authorities
                .iter()
                .map(|authority| self.committee.stake(authority))
                .sum()
        };
        (
            stake(observed_strong),
            stake(dag_strong),
            stake(&dag_strong_or_virtual),
        )
    }

    /// Strong-path reachability over blocks observed in Dag union VDag.
    fn has_strong_path(&self, block: &Certificate, target: &Digest, state: &State) -> bool {
        if let Some(ancestors) = state.strong_ancestors.get(&block.digest()) {
            return ancestors.contains(target);
        }
        let mut pending: Vec<_> = block.header.parents.iter().cloned().collect();
        let mut visited = HashSet::new();
        while let Some(digest) = pending.pop() {
            if &digest == target {
                return true;
            }
            if !visited.insert(digest.clone()) {
                continue;
            }
            if let Some(parent) = Self::observed_certificate(&digest, state) {
                pending.extend(parent.header.parents.iter().cloned());
            }
        }
        false
    }

    /// Whether `observer`'s causal history has ever referenced `target` using
    /// a strong parent or a virtual edge. Weak edges deliberately do not count
    /// as observing a leader for rule 3.
    fn history_references_strong_or_virtual(
        &self,
        observer: &Certificate,
        target: &Digest,
        state: &State,
    ) -> bool {
        let mut pending: Vec<_> = observer
            .header
            .parents
            .iter()
            .chain(&observer.header.virtual_edges)
            .cloned()
            .collect();
        let mut visited = HashSet::new();

        while let Some(digest) = pending.pop() {
            if &digest == target {
                return true;
            }
            if !visited.insert(digest.clone()) {
                continue;
            }
            if let Some(block) = Self::observed_certificate(&digest, state) {
                pending.extend(
                    block
                        .header
                        .parents
                        .iter()
                        .chain(&block.header.virtual_edges)
                        .cloned(),
                );
            }
        }
        false
    }

    /// Exactly two hops: one strong edge followed by one virtual edge to the
    /// target leader.
    fn has_two_hop_virtual_path(
        &self,
        block: &Certificate,
        target: &Digest,
        state: &State,
    ) -> bool {
        block.header.parents.iter().any(|parent_digest| {
            Self::observed_certificate(parent_digest, state)
                .map_or(false, |parent| parent.header.virtual_edges.contains(target))
        })
    }

    /// Counts exact three-edge virtual paths from `higher` to `lower`:
    /// higher --parent--> block --parent--> block --virtual--> lower.
    /// Paths are distinct when either intermediate block differs, so the
    /// identity of a path is `(first_digest, second_digest)`.
    fn three_edge_virtual_path_stake(
        &self,
        higher: &Certificate,
        lower: &Digest,
        state: &State,
    ) -> Stake {
        let mut paths = HashSet::new();
        for first_digest in &higher.header.parents {
            if let Some(first) = Self::observed_certificate(first_digest, state) {
                for second_digest in &first.header.parents {
                    if Self::observed_certificate(second_digest, state)
                        .map_or(false, |second| second.header.virtual_edges.contains(lower))
                    {
                        paths.insert((first_digest.clone(), second_digest.clone()));
                    }
                }
            }
        }
        paths.len() as Stake
    }

    /// Bridge a missing leader three rounds below `higher`. Accept a pure
    /// strong path, f+1 `(strong,strong,virtual) x 2` paths distinguished by
    /// their first (round h-1) vertex, or a three-strong prefix followed by
    /// f+1 `(strong,strong,virtual)` suffixes distinguished by the suffix's
    /// first strong vertex.
    fn missing_leader_bridge(&self, higher: &Certificate, target: &Digest, state: &State) -> bool {
        if self.has_strong_path(higher, target, state) {
            return true;
        }

        let threshold = self.committee.validity_threshold();
        let mut double_segment_first = HashSet::new();
        let mut suffix_first = HashSet::new();

        for first_digest in &higher.header.parents {
            let first = match Self::observed_certificate(first_digest, state) {
                Some(block) => block,
                None => continue,
            };
            for second_digest in &first.header.parents {
                let second = match Self::observed_certificate(second_digest, state) {
                    Some(block) => block,
                    None => continue,
                };

                // First alternative: (strong,strong,virtual) x 2. Only the
                // first vertex of the whole path must be distinct.
                for middle_digest in &second.header.virtual_edges {
                    if let Some(middle) = Self::observed_certificate(middle_digest, state) {
                        let mut qualifies = false;
                        for suffix_first_digest in &middle.header.parents {
                            if let Some(suffix_first_block) =
                                Self::observed_certificate(suffix_first_digest, state)
                            {
                                for suffix_second_digest in &suffix_first_block.header.parents {
                                    if Self::observed_certificate(suffix_second_digest, state)
                                        .map_or(false, |suffix_second| {
                                            suffix_second.header.virtual_edges.contains(target)
                                        })
                                    {
                                        qualifies = true;
                                    }
                                }
                            }
                        }
                        if qualifies {
                            double_segment_first.insert(first_digest.clone());
                        }
                    }
                }

                // Second alternative: exactly three strong edges, followed
                // by f+1 strong,strong,virtual suffixes. Suffixes are distinct
                // by their first strong vertex.
                for prefix_end_digest in &second.header.parents {
                    if let Some(prefix_end) = Self::observed_certificate(prefix_end_digest, state) {
                        for suffix_first_digest in &prefix_end.header.parents {
                            if let Some(suffix_first_block) =
                                Self::observed_certificate(suffix_first_digest, state)
                            {
                                if suffix_first_block.header.parents.iter().any(|digest| {
                                    Self::observed_certificate(digest, state).map_or(
                                        false,
                                        |suffix_second| {
                                            suffix_second.header.virtual_edges.contains(target)
                                        },
                                    )
                                }) {
                                    suffix_first.insert(suffix_first_digest.clone());
                                }
                            }
                        }
                    }
                }
            }
        }

        double_segment_first.len() as Stake >= threshold || suffix_first.len() as Stake >= threshold
    }

    /// Commit rule 3 resolves leaders that did not satisfy rules 1 or 2.
    /// A commit-ready leader at round h observes leaders h-3, h-6, ... .
    /// Every adjacent pair in that chain must have either a strong path or
    /// f+1 distinct three-edge virtual paths. The target is
    /// marked commit-ready when the whole chain succeeds, otherwise skipped.
    #[cfg(test)]
    async fn evaluate_commit_rule_three(&mut self, state: &mut State) {
        let observers: Vec<_> = state.pending_leaders.keys().cloned().collect();
        self.evaluate_commit_rule_three_for(observers, state).await;
    }

    async fn evaluate_commit_rule_three_for(&mut self, observers: Vec<Round>, state: &mut State) {
        for observer_round in observers {
            if observer_round < 4 {
                continue;
            }

            let mut target_round = observer_round - 3;
            loop {
                if !state.committed_leaders.contains(&target_round)
                    && !state.skipped_leaders.contains(&target_round)
                    && !state.pending_leaders.contains_key(&target_round)
                {
                    let target = self.observed_leader(target_round, state);
                    if target.is_none() {
                        sampled_debug!(
                            target_round,
                            "Skipping absent leader round {} during rule-3 backtracking",
                            target_round
                        );
                        self.mark_rule_three_skipped(target_round, state);
                        state.rule_three_recovery.remove(&target_round);
                        state.missing_leader_requests.remove(&target_round);
                        self.drain_ready_leaders(state).await;
                        if target_round < 4 {
                            break;
                        }
                        target_round -= 3;
                        continue;
                    }
                    let observer = self.observed_leader(observer_round, state);
                    let target_digest = target.as_ref().unwrap().digest();
                    if observer.as_ref().map_or(true, |observer| {
                        !self.history_references_strong_or_virtual(observer, &target_digest, state)
                    }) {
                        sampled_debug!(target_round,
                            "Skipping leader round {}: leader round {} history has no parents/virtual reference to its digest",
                            target_round, observer_round
                        );
                        self.mark_rule_three_skipped(target_round, state);
                        self.drain_ready_leaders(state).await;
                        if target_round < 4 {
                            break;
                        }
                        target_round -= 3;
                        continue;
                    }
                    let mut chain_round = observer_round;
                    let mut chain_valid = true;

                    while chain_valid && chain_round > target_round {
                        let lower_round = chain_round - 3;
                        let higher = self.observed_leader(chain_round, state);
                        let lower = if state.skipped_leaders.contains(&lower_round) {
                            None
                        } else {
                            self.observed_leader(lower_round, state)
                        };
                        let mut jump_to_target = false;
                        chain_valid = match (higher, lower) {
                            (Some(higher), Some(lower)) => {
                                let lower_digest = lower.digest();
                                self.has_strong_path(&higher, &lower_digest, state)
                                    || self.three_edge_virtual_path_stake(
                                        &higher,
                                        &lower_digest,
                                        state,
                                    ) >= self.committee.validity_threshold()
                            }
                            (Some(higher), None) => {
                                sampled_debug!(
                                    lower_round,
                                    "Skipping absent leader round {} in rule-3 chain",
                                    lower_round
                                );
                                self.mark_rule_three_skipped(lower_round, state);
                                let target_digest = target.as_ref().unwrap().digest();
                                if self.missing_leader_bridge(&higher, &target_digest, state) {
                                    sampled_debug!(lower_round,
                                        "Rule 3 bridges absent leader round {} from leader round {} history",
                                        lower_round, chain_round
                                    );
                                    jump_to_target = true;
                                    true
                                } else {
                                    false
                                }
                            }
                            (None, lower) => {
                                sampled_debug!(
                                    chain_round,
                                    "Skipping absent leader round {} in rule-3 chain",
                                    chain_round
                                );
                                self.mark_rule_three_skipped(chain_round, state);
                                if lower.is_none() {
                                    sampled_debug!(
                                        lower_round,
                                        "Skipping absent leader round {} in rule-3 chain",
                                        lower_round
                                    );
                                    self.mark_rule_three_skipped(lower_round, state);
                                }
                                false
                            }
                        };
                        chain_round = if jump_to_target {
                            target_round
                        } else {
                            lower_round
                        };
                    }

                    if chain_valid {
                        let target = target.unwrap();
                        state.force_observed_history_to_dag(target.clone(), target_round);
                        state.rule_three_recovery.remove(&target_round);
                        state.missing_leader_requests.remove(&target_round);
                        sampled_debug!(
                            target_round,
                            "Leader {:?} marked commit-ready by commit rule 3 through round {}",
                            target,
                            observer_round
                        );
                        let ordered = self.order_dag(&target, state);
                        state.pending_order.insert(target_round, ordered);
                        state.pending_leaders.insert(target_round, target);
                        state.leader_commit_rules.entry(target_round).or_insert(3);
                        state.record_rule_ready(target_round);
                        state.rule_three_stacks[(target_round % 3) as usize].insert(target_round);
                        state.wake_pending(target_round);
                    } else {
                        sampled_debug!(
                            target_round,
                            "Skipping leader round {} by commit rule 3 observed from round {}",
                            target_round,
                            observer_round
                        );
                        self.mark_rule_three_skipped(target_round, state);
                    }
                    self.drain_ready_leaders(state).await;
                }

                if target_round < 4 {
                    break;
                }
                target_round -= 3;
            }
        }
    }

    /// Re-evaluate only leaders whose inputs changed. A dirty target also
    /// wakes the pending rule-3 observers in the same three-round chain.
    async fn process_dirty_leaders(&mut self, state: &mut State) {
        while !state.dirty_leaders.is_empty() {
            let dirty: Vec<_> = state.dirty_leaders.drain().collect();
            let mut rule_three_observers = HashSet::new();

            for leader_round in dirty {
                if !state.committed_leaders.contains(&leader_round)
                    && !state.skipped_leaders.contains(&leader_round)
                    && !state.pending_leaders.contains_key(&leader_round)
                {
                    self.evaluate_commit_rule_one(leader_round + 1, state).await;
                    self.evaluate_commit_rule_two(leader_round + 2, state).await;
                }

                let stack = &state.rule_three_stacks[(leader_round % 3) as usize];
                rule_three_observers.extend(stack.range((leader_round + 3)..).cloned());
                if state.pending_leaders.contains_key(&leader_round) {
                    rule_three_observers.insert(leader_round);
                }
            }

            if !rule_three_observers.is_empty() {
                self.evaluate_commit_rule_three_for(
                    rule_three_observers.into_iter().collect(),
                    state,
                )
                .await;
            }
        }
    }

    fn observed_leader(&self, round: Round, state: &State) -> Option<Certificate> {
        let authority = self.ordering_leader_authority(round);
        state
            .dag
            .get(&round)
            .and_then(|blocks| blocks.get(&authority))
            .or_else(|| {
                state
                    .vdag
                    .get(&round)
                    .and_then(|blocks| blocks.get(&authority))
            })
            .map(|(_, certificate)| certificate.clone())
            .or_else(|| {
                state
                    .observed_by_round
                    .get(&round)
                    .and_then(|blocks| blocks.get(&authority))
                    .and_then(|digest| state.observed.get(digest))
                    .cloned()
            })
    }

    fn observed_certificate<'a>(digest: &Digest, state: &'a State) -> Option<&'a Certificate> {
        // Every Dag/VDag insertion passes through observe(), making this
        // digest index authoritative and avoiding repeated full-DAG scans in
        // every path-search hop.
        state.observed.get(digest)
    }

    async fn send_leader_request(&mut self, round: Round) {
        let authority = self.ordering_leader_authority(round);
        self.tx_primary
            .send(ConsensusCommand::LeaderRequest(round, authority))
            .await
            .expect("Failed to request rule-3 leader");
    }

    async fn retry_missing_leaders(&mut self, state: &State) {
        let rounds: Vec<_> = state.missing_leader_requests.iter().cloned().collect();
        for round in rounds {
            if self.observed_leader(round, state).is_none() {
                sampled_debug!(round, "Retrying request for missing leader round {}", round);
                self.send_leader_request(round).await;
            }
        }
    }

    fn promote_observed_pending_leader(&self, round: Round, state: &mut State) {
        if !state.pending_leaders.contains_key(&round)
            && !state.rule_three_recovery.contains(&round)
        {
            return;
        }
        if let Some(leader) = self.observed_leader(round, state) {
            state.force_observed_history_to_dag(leader, round);
            state.missing_leader_requests.remove(&round);
        }
    }

    fn log_pending_blockers(&self, state: &State) {
        for (round, leader) in &state.pending_leaders {
            let predecessor_ready = state.committed_leaders.contains(&(round - 1))
                || state.skipped_leaders.contains(&(round - 1));
            let in_dag = state.dag_digests.contains(&leader.digest());
            if !predecessor_ready || !in_dag {
                sampled_debug!(*round,
                    "Pending leader round {} blocked: predecessor round {} committed-or-skipped={}, leader-in-dag={}, unresolved-causal-dependencies={}",
                    round,
                    round - 1,
                    predecessor_ready,
                    in_dag,
                    state.forced_history_waiters.len()
                );
            }
        }
    }

    fn refresh_pending_orders(&self, rounds: &HashSet<Round>, state: &mut State) {
        for round in rounds {
            if let Some(leader) = state.pending_leaders.get(round).cloned() {
                let ordered = self.order_dag(&leader, state);
                state.pending_order.insert(*round, ordered);
            }
        }
    }

    /// Queue a leader once and commit ready leaders in consecutive round order.
    async fn queue_leader_commit(&mut self, leader: Certificate, rule: u8, state: &mut State) {
        let round = leader.round();
        if state.committed_leaders.contains(&round) {
            return;
        }
        // Entering pending authorizes early, verified GRBC data for the leader
        // and its complete causal history to be inserted directly into Dag.
        state.force_observed_history_to_dag(leader.clone(), round);
        state.record_rule_ready(round);
        state.leader_commit_rules.entry(round).or_insert(rule);
        let ordered = self.order_dag(&leader, state);
        #[cfg(feature = "benchmark")]
        for certificate in &ordered {
            if certificate.origin() != self.ordering_leader_authority(certificate.round()) {
                info!(
                    "Header rule-ordered round {} digest {:?}",
                    certificate.round(),
                    certificate.header.digest()
                );
            }
        }
        state.pending_order.insert(round, ordered);
        if let std::collections::btree_map::Entry::Vacant(entry) =
            state.pending_leaders.entry(round)
        {
            entry.insert(leader);
            state.rule_three_stacks[(round % 3) as usize].insert(round);
            state.dirty_leaders.insert(round);
        }
        state.wake_pending(round);

        self.drain_ready_leaders(state).await;
    }

    async fn drain_ready_leaders(&mut self, state: &mut State) {
        loop {
            let ready_round = match state.ready_pending.iter().next().cloned() {
                Some(round) => {
                    state.ready_pending.remove(&round);
                    round
                }
                None => break,
            };

            let leader_ready = state
                .pending_leaders
                .get(&ready_round)
                .map_or(false, |leader| {
                    state.predecessor_resolved(ready_round)
                        && state.dag_digests.contains(&leader.digest())
                });
            if !leader_ready {
                continue;
            }

            let leader = state.pending_leaders.remove(&ready_round).unwrap();
            state.rule_three_stacks[(ready_round % 3) as usize].remove(&ready_round);
            if !state.committed_leaders.insert(ready_round) {
                continue;
            }
            state.wake_pending(ready_round + 1);

            let mut sequence = state
                .pending_order
                .remove(&ready_round)
                .unwrap_or_else(|| self.order_dag(&leader, state));
            // A preordered successor may overlap with history committed by its
            // predecessor in the meantime. Apply only the cheap watermark
            // filter here; the expensive graph traversal remains precomputed.
            sequence.retain(|certificate| {
                state
                    .last_committed
                    .get(&certificate.origin())
                    .map_or(true, |round| certificate.round() > *round)
            });
            let commit_rule = state.leader_commit_rules.remove(&ready_round).unwrap_or(3);
            #[cfg(feature = "benchmark")]
            info!(
                "Commit rule stats leader {:?} rule {} outcome commit blocks {}",
                leader.header.digest(),
                commit_rule,
                sequence.len()
            );
            let _rule_ready_at_ms =
                state
                    .rule_ready_at_ms
                    .remove(&ready_round)
                    .unwrap_or_else(|| {
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .expect("System clock is before Unix epoch")
                            .as_millis()
                    });
            state.update(&sequence, self.gc_depth);
            for certificate in &sequence {
                #[cfg(not(feature = "benchmark"))]
                info!("Committed {}", certificate.header);
                #[cfg(feature = "benchmark")]
                info!(
                    "Header committed round {} digest {:?} leader {}",
                    certificate.round(),
                    certificate.header.digest(),
                    certificate.origin() == self.ordering_leader_authority(certificate.round())
                );
                #[cfg(feature = "benchmark")]
                for digest in certificate.header.payload.keys() {
                    info!(
                        "Committed {} -> {:?} @ {}",
                        certificate.header, digest, _rule_ready_at_ms
                    );
                }
            }
            if let Some(commit_tx) = &state.commit_tx {
                commit_tx
                    .send(sequence)
                    .expect("Commit writer stopped unexpectedly");
            } else {
                // Unit-level Consensus instances do not call `run`, so retain
                // a deterministic direct sink for those tests.
                for certificate in sequence {
                    self.tx_primary
                        .send(ConsensusCommand::Cleanup(certificate.clone()))
                        .await
                        .expect("Failed to send certificate to primary");
                    match &self.tx_output {
                        OutputSender::Individual(sender) => {
                            if let Err(error) = sender.send(certificate).await {
                                warn!("Failed to output certificate: {}", error);
                            }
                        }
                        OutputSender::Batch(sender) => {
                            if let Err(error) = sender.send(vec![certificate]).await {
                                warn!("Failed to output certificate batch: {}", error);
                            }
                        }
                    }
                }
            }
        }

        if log_enabled!(log::Level::Debug) && state.last_committed_round % 2 == 0 {
            for (name, round) in &state.last_committed {
                debug!("Latest commit of {}: Round {}", name, round);
            }
        }
    }

    fn ordering_leader_authority(&self, _round: Round) -> PublicKey {
        #[cfg(test)]
        {
            let mut keys: Vec<_> = self.committee.authorities.keys().cloned().collect();
            keys.sort();
            keys[0]
        }
        #[cfg(not(test))]
        {
            self.leader_authority(_round)
        }
    }

    /// Order the past leaders that we didn't already commit.
    #[allow(dead_code)]
    fn order_leaders(&self, leader: &Certificate, state: &State) -> Vec<Certificate> {
        let mut to_commit = vec![leader.clone()];
        let mut leader = leader;
        for r in (state.last_committed_round + 2..leader.round())
            .rev()
            .step_by(2)
        {
            // Get the certificate proposed by the previous leader.
            let (_, prev_leader) = match self.leader(r, &state.dag) {
                Some(x) => x,
                None => continue,
            };

            // Check whether there is a path between the last two leaders.
            if self.linked(leader, prev_leader, &state.dag) {
                to_commit.push(prev_leader.clone());
                leader = prev_leader;
            }
        }
        to_commit
    }

    /// Checks if there is a path between two leaders.
    #[allow(dead_code)]
    fn linked(&self, leader: &Certificate, prev_leader: &Certificate, dag: &Dag) -> bool {
        let mut parents = vec![leader];
        for r in (prev_leader.round()..leader.round()).rev() {
            parents = dag
                .get(&(r))
                .expect("We should have the whole history by now")
                .values()
                .filter(|(digest, _)| parents.iter().any(|x| x.header.parents.contains(digest)))
                .map(|(_, certificate)| certificate)
                .collect();
        }
        parents.contains(&prev_leader)
    }

    /// Checks whether `leader` reaches `prev_leader` through any combination
    /// of strong (`parents`) and weak (`weak_edges`) edges.
    ///
    /// Weak edges may skip rounds, so unlike `linked` this method performs a
    /// digest-based depth-first search rather than walking one round at a time.
    #[allow(dead_code)] // Available for the VDag-aware commit rule added next.
    fn linked_by_strong_or_weak(
        &self,
        leader: &Certificate,
        prev_leader: &Certificate,
        dag: &Dag,
    ) -> bool {
        let target = prev_leader.digest();
        if leader.digest() == target {
            return true;
        }

        let mut visited = HashSet::new();
        let mut pending: Vec<Digest> = leader
            .header
            .parents
            .iter()
            .chain(&leader.header.weak_edges)
            .cloned()
            .collect();

        while let Some(digest) = pending.pop() {
            if digest == target {
                return true;
            }
            if !visited.insert(digest.clone()) {
                continue;
            }

            let certificate = dag
                .values()
                .flat_map(|authorities| authorities.values())
                .find(|(candidate, _)| candidate == &digest)
                .map(|(_, certificate)| certificate);

            if let Some(certificate) = certificate {
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
        false
    }

    /// Flatten the dag referenced by the input certificate. This is a classic depth-first search (pre-order):
    /// https://en.wikipedia.org/wiki/Tree_traversal#Pre-order
    fn order_dag(&self, leader: &Certificate, state: &State) -> Vec<Certificate> {
        sampled_debug!(leader.round(), "Processing sub-dag of {:?}", leader);
        let mut ordered = Vec::new();
        let mut already_ordered = HashSet::new();

        let mut buffer = vec![leader];
        while let Some(x) = buffer.pop() {
            sampled_debug!(x.round(), "Sequencing {:?}", x);
            ordered.push(x.clone());
            // Strong parents point to the preceding round, while weak parents
            // may cross several rounds. Both belong to the committed causal
            // history and must be ordered; virtual edges are intentionally not
            // traversed here.
            for parent in x.header.parents.iter().chain(&x.header.weak_edges) {
                let certificate = match state.dag_by_digest.get(parent) {
                    Some(certificate) => certificate,
                    None => continue, // We already ordered or GC up to here.
                };

                // We skip the certificate if we (1) already processed it or (2) we reached a round that we already
                // committed for this authority.
                let mut skip = already_ordered.contains(parent);
                skip |= state
                    .last_committed
                    .get(&certificate.origin())
                    .map_or_else(|| false, |r| certificate.round() <= *r);
                if !skip {
                    buffer.push(certificate);
                    already_ordered.insert(parent.clone());
                }
            }
        }

        // Ensure we do not commit garbage collected certificates.
        ordered.retain(|x| x.round() + self.gc_depth >= state.last_committed_round);

        // Ordering the output by round is not really necessary but it makes the commit sequence prettier.
        ordered.sort_by_key(|x| x.round());
        ordered
    }
}
