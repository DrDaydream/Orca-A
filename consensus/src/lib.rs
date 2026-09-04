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
use tokio::time::{self, Duration, Instant};

const LEADER_RETRY_DELAY: Duration = Duration::from_millis(300);

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

#[derive(Clone)]
struct FallbackHistorySnapshot {
    version: u64,
    complete: bool,
    by_round: HashMap<Round, Vec<Digest>>,
}

#[derive(Clone, Copy)]
struct FallbackDecision {
    version: u64,
    commit: bool,
}

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
    /// Cached per-round result of the dynamic adversary selection.
    adversarial_leaders: HashMap<Round, bool>,
    deferred_rule_one: HashMap<Round, bool>,
    /// Leader rounds already committed, preventing duplicate commits.
    committed_leaders: HashSet<Round>,
    /// Leader rounds explicitly skipped by commit rule 3.
    skipped_leaders: HashSet<Round>,
    /// Commit-ready leaders waiting for the previous round's leader.
    pending_leaders: BTreeMap<Round, Certificate>,
    /// Wall-clock time at which a leader first completed a commit rule. This
    /// intentionally excludes predecessor and output-channel waiting.
    rule_ready_at_ms: HashMap<Round, u128>,
    /// Earliest rule-ready time for every preordered non-leader header.
    #[cfg(feature = "benchmark")]
    rule_order_ready_at: HashMap<Digest, u128>,
    /// The single terminal rule selected for each leader (1, 2, or 3).
    leader_commit_rules: HashMap<Round, u8>,
    /// DAG sequences prepared as soon as a leader completes a rule, while
    /// predecessor ordering continues independently.
    pending_order: HashMap<Round, Vec<Certificate>>,
    /// Pending rounds whose direct predecessor is already committed/skipped.
    /// This avoids repeatedly scanning the complete pending map.
    ready_pending: BTreeSet<Round>,
    /// Leaders still awaiting pessimistic finalization, split into the three
    /// independent `r mod 3` fallback lanes.
    rule_three_stacks: [BTreeSet<Round>; 3],
    /// Latest Good/Deferred leader that may recover each fallback lane.
    rule_three_anchors: [Option<Round>; 3],
    /// Monotone revision of newly observed structural evidence.
    fallback_evidence_version: u64,
    /// Evidence revision assigned to each active recovery anchor.
    fallback_anchor_versions: HashMap<Round, u64>,
    /// Recovery anchors whose relevant evidence or lane contents changed.
    dirty_fallback_anchors: HashSet<Round>,
    /// Cached strong/weak causal-history traversal per recovery anchor.
    fallback_history_cache: HashMap<Round, FallbackHistorySnapshot>,
    /// Cached Rule-3 decision per `(anchor round, target round)`.
    fallback_decision_cache: HashMap<(Round, Round), FallbackDecision>,
    /// Reverse index from a traversed vertex to anchors using that vertex.
    fallback_vertex_users: HashMap<Digest, HashSet<Round>>,
    /// Missing causal or virtual evidence awaited by recovery anchors.
    fallback_evidence_waiters: HashMap<Digest, HashSet<Round>>,
    /// Rule-3 leaders whose data is being recovered from GRBC/other nodes.
    rule_three_recovery: HashSet<Round>,
    /// Missing leaders awaiting their next rate-limited retry.
    missing_leader_requests: HashMap<Round, Instant>,
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
            adversarial_leaders: HashMap::new(),
            deferred_rule_one: HashMap::new(),
            committed_leaders: [0].iter().cloned().collect(),
            skipped_leaders: HashSet::new(),
            pending_leaders: BTreeMap::new(),
            rule_ready_at_ms: HashMap::new(),
            #[cfg(feature = "benchmark")]
            rule_order_ready_at: HashMap::new(),
            leader_commit_rules: HashMap::new(),
            pending_order: HashMap::new(),
            ready_pending: BTreeSet::new(),
            rule_three_stacks: [BTreeSet::new(), BTreeSet::new(), BTreeSet::new()],
            rule_three_anchors: [None, None, None],
            fallback_evidence_version: 0,
            fallback_anchor_versions: HashMap::new(),
            dirty_fallback_anchors: HashSet::new(),
            fallback_history_cache: HashMap::new(),
            fallback_decision_cache: HashMap::new(),
            fallback_vertex_users: HashMap::new(),
            fallback_evidence_waiters: HashMap::new(),
            rule_three_recovery: HashSet::new(),
            missing_leader_requests: HashMap::new(),
            forced_history_waiters: HashMap::new(),
            dirty_leaders: HashSet::new(),
            highest_advanced_round: 1,
            commit_tx: None,
        }
    }

    fn record_rule_ready(&mut self, round: Round) -> Option<u128> {
        if self.rule_ready_at_ms.contains_key(&round) {
            return None;
        }
        let ready_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("System clock is before Unix epoch")
            .as_millis();
        self.rule_ready_at_ms.insert(round, ready_at);
        Some(ready_at)
    }

    fn mark_fallback_anchor_dirty(&mut self, anchor_round: Round) {
        let lane = (anchor_round % 3) as usize;
        if self.rule_three_anchors[lane] != Some(anchor_round) {
            return;
        }
        self.fallback_anchor_versions
            .insert(anchor_round, self.fallback_evidence_version);
        self.dirty_fallback_anchors.insert(anchor_round);
    }

    fn mark_fallback_digest_changed(&mut self, digest: &Digest) {
        let mut anchors = self
            .fallback_vertex_users
            .get(digest)
            .cloned()
            .unwrap_or_default();
        anchors.extend(
            self.fallback_evidence_waiters
                .remove(digest)
                .unwrap_or_default(),
        );
        for anchor in anchors {
            self.mark_fallback_anchor_dirty(anchor);
        }
    }

    fn fallback_history_snapshot(&mut self, anchor: &Certificate) -> FallbackHistorySnapshot {
        let anchor_round = anchor.round();
        let version = self
            .fallback_anchor_versions
            .get(&anchor_round)
            .copied()
            .unwrap_or(self.fallback_evidence_version);
        if let Some(snapshot) = self.fallback_history_cache.get(&anchor_round) {
            if snapshot.version == version {
                return snapshot.clone();
            }
        }

        let mut snapshot = FallbackHistorySnapshot {
            version,
            complete: true,
            by_round: HashMap::new(),
        };
        let mut pending: Vec<_> = anchor
            .header
            .parents
            .iter()
            .chain(&anchor.header.weak_edges)
            .cloned()
            .collect();
        let mut visited = HashSet::new();

        for digest in &anchor.header.virtual_edges {
            if !self.observed.contains_key(digest) {
                snapshot.complete = false;
                self.fallback_evidence_waiters
                    .entry(digest.clone())
                    .or_default()
                    .insert(anchor_round);
            }
        }
        while let Some(digest) = pending.pop() {
            if !visited.insert(digest.clone()) {
                continue;
            }
            let (vertex_round, virtual_edges, dependencies) = match self.observed.get(&digest) {
                Some(vertex) => (
                    vertex.round(),
                    vertex
                        .header
                        .virtual_edges
                        .iter()
                        .cloned()
                        .collect::<Vec<_>>(),
                    vertex
                        .header
                        .parents
                        .iter()
                        .chain(&vertex.header.weak_edges)
                        .cloned()
                        .collect::<Vec<_>>(),
                ),
                None => {
                    snapshot.complete = false;
                    self.fallback_evidence_waiters
                        .entry(digest)
                        .or_default()
                        .insert(anchor_round);
                    continue;
                }
            };
            self.fallback_vertex_users
                .entry(digest.clone())
                .or_default()
                .insert(anchor_round);
            snapshot
                .by_round
                .entry(vertex_round)
                .or_default()
                .push(digest.clone());
            for virtual_digest in virtual_edges {
                if !self.observed.contains_key(&virtual_digest) {
                    snapshot.complete = false;
                    self.fallback_evidence_waiters
                        .entry(virtual_digest)
                        .or_default()
                        .insert(anchor_round);
                }
            }
            pending.extend(dependencies);
        }
        self.fallback_history_cache
            .insert(anchor_round, snapshot.clone());
        snapshot
    }

    fn fallback_decision(
        &self,
        anchor_round: Round,
        target_round: Round,
        version: u64,
    ) -> Option<bool> {
        self.fallback_decision_cache
            .get(&(anchor_round, target_round))
            .filter(|decision| decision.version == version)
            .map(|decision| decision.commit)
    }

    fn observe(&mut self, certificate: Certificate) -> HashSet<Round> {
        let digest = certificate.digest();
        let certificate_round = certificate.round();
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
        self.fallback_evidence_version = self.fallback_evidence_version.wrapping_add(1);
        self.mark_fallback_digest_changed(&digest);
        let mut dirty = self.index_strong_paths(&certificate);
        // The certificate may be a late leader or the endpoint of virtual
        // paths that were already present in descendants.
        dirty.insert(certificate_round);
        for target in &certificate.header.virtual_edges {
            if let Some(block) = self.observed.get(target) {
                dirty.insert(block.round());
            }
        }
        let owners = self
            .forced_history_waiters
            .remove(&digest)
            .unwrap_or_default();
        for owner_round in &owners {
            self.force_observed_history_to_dag(certificate.clone(), *owner_round);
        }
        dirty.extend(owners);
        dirty
    }

    fn index_strong_paths(&mut self, certificate: &Certificate) -> HashSet<Round> {
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
        self.propagate_strong_ancestors(digest, additions)
    }

    fn propagate_strong_ancestors(
        &mut self,
        source: Digest,
        additions: HashSet<Digest>,
    ) -> HashSet<Round> {
        let mut pending = vec![(source, additions)];
        let mut dirty = HashSet::new();
        while let Some((digest, candidates)) = pending.pop() {
            let ancestors = self.strong_ancestors.entry(digest.clone()).or_default();
            let fresh: HashSet<_> = candidates
                .into_iter()
                .filter(|ancestor| ancestors.insert(ancestor.clone()))
                .collect();
            if fresh.is_empty() {
                continue;
            }
            self.mark_fallback_digest_changed(&digest);
            if let Some(block) = self.observed.get(&digest) {
                let round = block.round();
                let origin = block.origin();
                for ancestor in &fresh {
                    if let Some(target) = self.observed.get(ancestor) {
                        dirty.insert(target.round());
                    }
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
        dirty
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
        if round >= 2 {
            self.dirty_leaders.insert(round - 1);
        }
        if round >= 3 {
            self.dirty_leaders.insert(round - 2);
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
        self.adversarial_leaders
            .retain(|round, _| *round + gc_depth >= last_committed_round);
        self.deferred_rule_one
            .retain(|round, _| *round + gc_depth >= last_committed_round);
        let active_anchors: HashSet<_> =
            self.rule_three_anchors.iter().flatten().cloned().collect();
        let keep_anchor = |round: &Round| {
            active_anchors.contains(round) || *round + gc_depth >= last_committed_round
        };
        self.fallback_anchor_versions
            .retain(|round, _| keep_anchor(round));
        self.dirty_fallback_anchors
            .retain(|round| keep_anchor(round));
        self.fallback_history_cache
            .retain(|round, _| keep_anchor(round));
        self.fallback_decision_cache.retain(|(anchor, target), _| {
            keep_anchor(anchor) && *target + gc_depth >= last_committed_round
        });
        self.fallback_vertex_users.retain(|digest, anchors| {
            anchors.retain(|round| keep_anchor(round));
            observed_digests.contains(digest) && !anchors.is_empty()
        });
        self.fallback_evidence_waiters.retain(|_, anchors| {
            anchors.retain(|round| keep_anchor(round));
            !anchors.is_empty()
        });
        self.leader_commit_rules
            .retain(|round, _| *round + gc_depth >= last_committed_round);
        #[cfg(feature = "benchmark")]
        self.rule_order_ready_at
            .retain(|digest, _| observed_digests.contains(digest));
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
    fn adversarial_leader(&self, round: Round, state: &mut State) -> bool {
        if let Some(selected) = state.adversarial_leaders.get(&round) {
            return *selected;
        }

        let faults = std::env::var("ORCA_FAULTS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let seed = std::env::var("ORCA_ADVERSARY_SEED")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        let authorities: Vec<_> = self.committee.authorities.keys().cloned().collect();
        let leader = self.ordering_leader_authority(round);
        let selected =
            primary::adversary::selected(&leader, &authorities, round, faults, seed, None);
        state.adversarial_leaders.insert(round, selected);
        selected
    }

    fn defer_rule_one_to_rule_two(&self, round: Round, state: &mut State) -> bool {
        if let Some(deferred) = state.deferred_rule_one.get(&round) {
            return *deferred;
        }
        let faults = std::env::var("ORCA_FAULTS")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let seed = std::env::var("ORCA_ADVERSARY_SEED")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0);
        let leader = self.ordering_leader_authority(round);
        let deferred = faults > 0 && primary::adversary::defer_to_rule_two(&leader, round, seed);
        state.deferred_rule_one.insert(round, deferred);
        deferred
    }

    fn mark_rule_three_skipped(&self, round: Round, state: &mut State) {
        if state.leader_commit_rules.contains_key(&round)
            || state.committed_leaders.contains(&round)
        {
            return;
        }
        state.rule_three_stacks[(round % 3) as usize].remove(&round);
        state.rule_three_recovery.remove(&round);
        state.missing_leader_requests.remove(&round);
        state.dirty_leaders.remove(&round);
        if state.mark_skipped(round) {
            state.leader_commit_rules.insert(round, 3);
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

        let mut diagnostic_tick = time::interval(Duration::from_secs(1));
        diagnostic_tick.set_missed_tick_behavior(time::MissedTickBehavior::Delay);

        // Listen to incoming certificates and recovery timers.
        loop {
            let next_leader_retry = state.missing_leader_requests.values().min().copied();
            let message = tokio::select! {
                message = rx_ingress.recv() => match message {
                    Some(message) => message,
                    None => break,
                },
                _ = async {
                    match next_leader_retry {
                        Some(deadline) => time::sleep_until(deadline).await,
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    self.retry_missing_leaders(&mut state).await;
                    continue;
                }
                _ = diagnostic_tick.tick() => {
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
                if let Some(anchor) = state.rule_three_anchors[(observed_round % 3) as usize] {
                    state.mark_fallback_anchor_dirty(anchor);
                }
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
            if round >= 4 {
                let fallback_round = round - 3;
                if !state.committed_leaders.contains(&fallback_round)
                    && !state.skipped_leaders.contains(&fallback_round)
                    && !state.leader_commit_rules.contains_key(&fallback_round)
                    && !state.pending_leaders.contains_key(&fallback_round)
                {
                    let lane = (fallback_round % 3) as usize;
                    if state.rule_three_stacks[lane].insert(fallback_round) {
                        if let Some(anchor) = state.rule_three_anchors[lane] {
                            state.mark_fallback_anchor_dirty(anchor);
                        }
                    }
                }
            }
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
        if self.adversarial_leader(leader_round, state)
            || self.defer_rule_one_to_rule_two(leader_round, state)
        {
            return;
        }
        if state.committed_leaders.contains(&leader_round)
            || state.skipped_leaders.contains(&leader_round)
            || state.leader_commit_rules.contains_key(&leader_round)
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
        if self.adversarial_leader(leader_round, state) {
            return;
        }
        if state.committed_leaders.contains(&leader_round)
            || state.skipped_leaders.contains(&leader_round)
            || state.leader_commit_rules.contains_key(&leader_round)
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

    fn virtual_path_to_leader(
        &self,
        block: &Certificate,
        leader_round: Round,
        leader: PublicKey,
        state: &State,
    ) -> bool {
        let mut frontier: Vec<_> = block.header.parents.iter().cloned().collect();
        for _ in 0..2 {
            let mut next = Vec::new();
            for digest in frontier {
                let vertex = match Self::observed_certificate(&digest, state) {
                    Some(vertex) => vertex,
                    None => continue,
                };
                if vertex.header.virtual_edges.iter().any(|target| {
                    Self::observed_certificate(target, state).map_or(false, |endpoint| {
                        endpoint.round() == leader_round && endpoint.origin() == leader
                    })
                }) {
                    return true;
                }
                next.extend(vertex.header.parents.iter().cloned());
            }
            frontier = next;
        }
        false
    }

    /// Direct fallback votes are anchor strong parents, not individual paths.
    /// A proposer contributes its stake at most once even if its parent has
    /// several qualifying virtual paths.
    fn direct_fallback_stake(
        &self,
        anchor: &Certificate,
        leader_round: Round,
        leader: PublicKey,
        state: &State,
    ) -> Stake {
        let mut voters = HashSet::new();
        let mut stake = 0;
        for digest in &anchor.header.parents {
            let parent = match Self::observed_certificate(digest, state) {
                Some(parent) => parent,
                None => continue,
            };
            let proposer = parent.origin();
            if self.virtual_path_to_leader(parent, leader_round, leader, state)
                && voters.insert(proposer)
            {
                stake += self.committee.stake(&proposer);
                if stake >= self.committee.validity_threshold() {
                    break;
                }
            }
        }
        stake
    }

    fn indirect_inner_stake(
        &self,
        vertex: &Certificate,
        leader_round: Round,
        leader: PublicKey,
        state: &State,
    ) -> Stake {
        let mut voters = HashSet::new();
        let mut stake = 0;
        for digest in &vertex.header.parents {
            let parent = match Self::observed_certificate(digest, state) {
                Some(parent) => parent,
                None => continue,
            };
            let proposer = parent.origin();
            if self.virtual_path_to_leader(parent, leader_round, leader, state)
                && voters.insert(proposer)
            {
                stake += self.committee.stake(&proposer);
                if stake >= self.committee.validity_threshold() {
                    break;
                }
            }
        }
        stake
    }

    /// Indirect fallback votes are distinct round-(r+3) proposers in the
    /// anchor's strong/weak causal history.
    fn indirect_fallback_stake(
        &self,
        history: &FallbackHistorySnapshot,
        target: Option<&Certificate>,
        leader_round: Round,
        leader: PublicKey,
        state: &State,
    ) -> Stake {
        let history_round = leader_round + 3;
        let mut voters = HashSet::new();
        let mut stake = 0;

        for digest in history.by_round.get(&history_round).into_iter().flatten() {
            let vertex = match Self::observed_certificate(&digest, state) {
                Some(vertex) => vertex,
                None => continue,
            };
            if vertex.round() == history_round {
                let strong = target.map_or(false, |target| {
                    self.has_strong_path(vertex, &target.digest(), state)
                });
                let virtual_stake = self.indirect_inner_stake(vertex, leader_round, leader, state);
                let proposer = vertex.origin();
                if (strong || virtual_stake >= self.committee.validity_threshold())
                    && voters.insert(proposer)
                {
                    stake += self.committee.stake(&proposer);
                    if stake >= self.committee.validity_threshold() {
                        break;
                    }
                }
            }
        }
        stake
    }

    fn stage_leader_commit(&self, leader: Certificate, rule: u8, state: &mut State) {
        let round = leader.round();
        if state.committed_leaders.contains(&round)
            || state.skipped_leaders.contains(&round)
            || state.pending_leaders.contains_key(&round)
            || state.leader_commit_rules.contains_key(&round)
        {
            return;
        }
        state.leader_commit_rules.insert(round, rule);
        state.force_observed_history_to_dag(leader.clone(), round);
        state.rule_three_stacks[(round % 3) as usize].remove(&round);
        state.rule_three_recovery.remove(&round);
        state.missing_leader_requests.remove(&round);
        if let Some(_ready_at) = state.record_rule_ready(round) {
            #[cfg(feature = "benchmark")]
            info!(
                "Leader commit-ready round {} digest {:?} at {}",
                round,
                leader.header.digest(),
                _ready_at
            );
        }
        let ordered = self.order_dag(&leader, state);
        #[cfg(feature = "benchmark")]
        {
            let ready_at = state.rule_ready_at_ms[&round];
            for certificate in &ordered {
                if certificate.origin() != self.ordering_leader_authority(certificate.round()) {
                    let digest = certificate.header.digest();
                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        state.rule_order_ready_at.entry(digest)
                    {
                        entry.insert(ready_at);
                        info!(
                            "Header rule-ordered round {} digest {:?}",
                            certificate.round(),
                            certificate.header.digest()
                        );
                    }
                }
            }
        }
        state.pending_order.insert(round, ordered);
        state.pending_leaders.entry(round).or_insert(leader);
        state.wake_pending(round);
    }

    /// Resolve one lane exactly as Algorithm 3's fallback stack: pop the
    /// newest unresolved leader below the anchor, update the anchor only after
    /// a commit, and leave it unchanged after a skip.
    async fn finalize_fallback(&mut self, anchor_round: Round, state: &mut State) {
        let lane = (anchor_round % 3) as usize;
        let mut anchor = match self.observed_leader(anchor_round, state) {
            Some(anchor) => anchor,
            None => return,
        };

        let mut candidate = anchor.round().saturating_sub(3);
        while candidate > 0 {
            if !state.committed_leaders.contains(&candidate)
                && !state.skipped_leaders.contains(&candidate)
                && !state.leader_commit_rules.contains_key(&candidate)
                && !state.pending_leaders.contains_key(&candidate)
            {
                state.rule_three_stacks[lane].insert(candidate);
            }
            if candidate < 3 {
                break;
            }
            candidate -= 3;
        }

        loop {
            let target_round = match state.rule_three_stacks[lane]
                .range(..anchor.round())
                .next_back()
                .cloned()
            {
                Some(round) => round,
                None => break,
            };
            if state.committed_leaders.contains(&target_round)
                || state.skipped_leaders.contains(&target_round)
                || state.leader_commit_rules.contains_key(&target_round)
                || state.pending_leaders.contains_key(&target_round)
            {
                state.rule_three_stacks[lane].remove(&target_round);
                continue;
            }
            let leader = self.observed_leader(target_round, state);
            let history = state.fallback_history_snapshot(&anchor);
            if !history.complete {
                if leader.is_none() {
                    self.request_missing_leader(target_round, state).await;
                }
                break;
            }
            let leader_authority = self.ordering_leader_authority(target_round);
            let decision_key = (anchor.round(), target_round);
            let commit = if let Some(commit) =
                state.fallback_decision(anchor.round(), target_round, history.version)
            {
                commit
            } else if anchor.round() <= target_round + 3 {
                leader.as_ref().map_or(false, |target| {
                    self.has_strong_path(&anchor, &target.digest(), state)
                }) || self.direct_fallback_stake(&anchor, target_round, leader_authority, state)
                    >= self.committee.validity_threshold()
            } else {
                self.indirect_fallback_stake(
                    &history,
                    leader.as_ref(),
                    target_round,
                    leader_authority,
                    state,
                ) >= self.committee.validity_threshold()
            };
            state.fallback_decision_cache.insert(
                decision_key,
                FallbackDecision {
                    version: history.version,
                    commit,
                },
            );

            if commit {
                let target = match leader {
                    Some(target) => target,
                    None => {
                        self.request_missing_leader(target_round, state).await;
                        break;
                    }
                };
                sampled_debug!(
                    target_round,
                    "Leader {:?} marked commit-ready by fallback anchor round {}",
                    target,
                    anchor.round()
                );
                self.stage_leader_commit(target.clone(), 3, state);
                anchor = target;
                state.rule_three_anchors[lane] = Some(anchor.round());
                state.mark_fallback_anchor_dirty(anchor.round());
                // This invocation immediately continues with the new anchor.
                state.dirty_fallback_anchors.remove(&anchor.round());
            } else {
                sampled_debug!(
                    target_round,
                    "Skipping leader round {} by fallback anchor round {}",
                    target_round,
                    anchor.round()
                );
                self.mark_rule_three_skipped(target_round, state);
            }
        }
        self.drain_ready_leaders(state).await;
    }

    async fn evaluate_dirty_fallback_anchors(&mut self, state: &mut State) {
        while let Some(anchor) = state.dirty_fallback_anchors.iter().next().cloned() {
            state.dirty_fallback_anchors.remove(&anchor);
            let lane = (anchor % 3) as usize;
            if state.rule_three_anchors[lane] == Some(anchor) {
                self.finalize_fallback(anchor, state).await;
            }
        }
    }

    #[cfg(test)]
    async fn evaluate_commit_rule_three(&mut self, state: &mut State) {
        let anchors: Vec<_> = state.rule_three_anchors.iter().flatten().cloned().collect();
        for anchor in anchors {
            state.mark_fallback_anchor_dirty(anchor);
        }
        self.evaluate_dirty_fallback_anchors(state).await;
    }

    /// Retry only leaders and fallback anchors whose indexed evidence changed.
    async fn process_dirty_leaders(&mut self, state: &mut State) {
        loop {
            let dirty: Vec<_> = state.dirty_leaders.drain().collect();
            for leader_round in dirty {
                if !state.committed_leaders.contains(&leader_round)
                    && !state.skipped_leaders.contains(&leader_round)
                    && !state.leader_commit_rules.contains_key(&leader_round)
                    && !state.pending_leaders.contains_key(&leader_round)
                {
                    self.evaluate_commit_rule_one(leader_round + 1, state).await;
                    self.evaluate_commit_rule_two(leader_round + 2, state).await;
                }
            }
            self.evaluate_dirty_fallback_anchors(state).await;
            if state.dirty_leaders.is_empty() && state.dirty_fallback_anchors.is_empty() {
                break;
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

    async fn request_missing_leader(&mut self, round: Round, state: &mut State) {
        if state.skipped_leaders.contains(&round) || self.observed_leader(round, state).is_some() {
            state.rule_three_recovery.remove(&round);
            state.missing_leader_requests.remove(&round);
            return;
        }
        if !state.rule_three_recovery.insert(round) {
            return;
        }
        self.send_leader_request(round).await;
        state
            .missing_leader_requests
            .insert(round, Instant::now() + LEADER_RETRY_DELAY);
    }

    async fn retry_missing_leaders(&mut self, state: &mut State) {
        let now = Instant::now();
        let rounds: Vec<_> = state
            .missing_leader_requests
            .iter()
            .filter_map(|(round, deadline)| (*deadline <= now).then_some(*round))
            .collect();
        for round in rounds {
            if state.skipped_leaders.contains(&round)
                || self.observed_leader(round, state).is_some()
                || !state.rule_three_recovery.contains(&round)
            {
                state.rule_three_recovery.remove(&round);
                state.missing_leader_requests.remove(&round);
                continue;
            }
            sampled_debug!(round, "Retrying request for missing leader round {}", round);
            self.send_leader_request(round).await;
            state
                .missing_leader_requests
                .insert(round, Instant::now() + LEADER_RETRY_DELAY);
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
        if state.committed_leaders.contains(&round)
            || state.skipped_leaders.contains(&round)
            || state.leader_commit_rules.contains_key(&round)
        {
            return;
        }
        // A Good/Deferred leader is the only valid initial recovery anchor.
        // Force its strong/weak causal history before deciding any stack item.
        if rule == 1 || rule == 2 {
            state.force_observed_history_to_dag(leader.clone(), round);
            let lane = (round % 3) as usize;
            if state.rule_three_anchors[lane].map_or(true, |previous| round > previous) {
                state.rule_three_anchors[lane] = Some(round);
            }
            let anchor_round = state.rule_three_anchors[lane].unwrap_or(round);
            state.mark_fallback_anchor_dirty(anchor_round);
            self.evaluate_dirty_fallback_anchors(state).await;
        }
        if state.committed_leaders.contains(&round)
            || state.skipped_leaders.contains(&round)
            || state.pending_leaders.contains_key(&round)
            || state.leader_commit_rules.contains_key(&round)
        {
            return;
        }
        self.stage_leader_commit(leader, rule, state);
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
            // Final ordering follows only strong and weak causal history.
            // Virtual edges remain protocol metadata and do not pull blocks
            // into the ordered output.
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
