// Copyright(C) Facebook, Inc. and its affiliates.
use super::*;
use config::{Authority, PrimaryAddresses};
use crypto::{generate_keypair, SecretKey};
use primary::Header;
use rand::rngs::StdRng;
use rand::SeedableRng as _;
use std::collections::{BTreeSet, VecDeque};
use tokio::sync::mpsc::{channel, Sender};

async fn deliver(tx: &Sender<ConsensusMessage>, certificate: Certificate) {
    tx.send(ConsensusMessage::GradeOne(certificate.clone()))
        .await
        .unwrap();
    tx.send(ConsensusMessage::GradeTwo(certificate))
        .await
        .unwrap();
}

// Fixture
fn keys() -> Vec<(PublicKey, SecretKey)> {
    let mut rng = StdRng::from_seed([0; 32]);
    (0..4).map(|_| generate_keypair(&mut rng)).collect()
}

// Fixture
pub fn mock_committee() -> Committee {
    Committee {
        authorities: keys()
            .iter()
            .map(|(id, _)| {
                (
                    *id,
                    Authority {
                        stake: 1,
                        primary: PrimaryAddresses {
                            primary_to_primary: "0.0.0.0:0".parse().unwrap(),
                            worker_to_primary: "0.0.0.0:0".parse().unwrap(),
                        },
                        workers: HashMap::default(),
                    },
                )
            })
            .collect(),
    }
}

// Fixture
fn mock_certificate(
    origin: PublicKey,
    round: Round,
    parents: BTreeSet<Digest>,
) -> (Digest, Certificate) {
    let certificate = Certificate {
        header: Header {
            author: origin,
            round,
            parents,
            ..Header::default()
        },
        ..Certificate::default()
    };
    (certificate.digest(), certificate)
}

// Creates one certificate per authority starting and finishing at the specified rounds (inclusive).
// Outputs a VecDeque of certificates (the certificate with higher round is on the front) and a set
// of digests to be used as parents for the certificates of the next round.
fn make_certificates(
    start: Round,
    stop: Round,
    initial_parents: &BTreeSet<Digest>,
    keys: &[PublicKey],
) -> (VecDeque<Certificate>, BTreeSet<Digest>) {
    let mut certificates = VecDeque::new();
    let mut parents = initial_parents.iter().cloned().collect::<BTreeSet<_>>();
    let mut next_parents = BTreeSet::new();

    for round in start..=stop {
        next_parents.clear();
        for name in keys {
            let (digest, certificate) = mock_certificate(*name, round, parents.clone());
            certificates.push_back(certificate);
            next_parents.insert(digest);
        }
        parents = next_parents.clone();
    }
    (certificates, next_parents)
}

#[test]
fn vdag_stores_grade_one_deliveries() {
    let committee = mock_committee();
    let mut state = State::new(Certificate::genesis(&committee));
    let origin = keys()[0].0;
    let (digest, certificate) = mock_certificate(origin, 1, BTreeSet::new());

    state.insert_grade_one(certificate.clone());

    let (stored_digest, stored) = state.vdag.get(&1).unwrap().get(&origin).unwrap();
    assert_eq!(stored_digest, &digest);
    assert_eq!(stored, &certificate);
    assert!(state.dag.get(&1).is_none());

    state.promote_to_dag(certificate.clone());

    assert!(state.vdag.get(&1).is_none());
    let (stored_digest, stored) = state.dag.get(&1).unwrap().get(&origin).unwrap();
    assert_eq!(stored_digest, &digest);
    assert_eq!(stored, &certificate);
}

#[test]
fn future_round_block_is_kept_through_all_grbc_stages() {
    let committee = mock_committee();
    let mut state = State::new(Certificate::genesis(&committee));
    let authority = keys()[0].0;
    let (digest, block) = mock_certificate(authority, 10, BTreeSet::new());

    // A node whose formal Dag only contains genesis may still receive and
    // retain a valid future-round block immediately.
    state.observe(block.clone());
    assert!(state.observed.contains_key(&digest));

    state.insert_grade_one(block.clone());
    assert!(state
        .vdag
        .get(&10)
        .and_then(|round| round.get(&authority))
        .is_some());

    state.mark_grade_two(digest.clone());
    state.promote_ready();
    assert!(state.dag_digests.contains(&digest));
    assert!(state.vdag.get(&10).map_or(true, |round| {
        !round.values().any(|(candidate, _)| candidate == &digest)
    }));
}

#[tokio::test]
async fn higher_round_jump_checks_each_crossed_round_once() {
    let committee = mock_committee();
    let mut consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary: channel(10).0,
        tx_output: OutputSender::Individual(channel(10).0),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));

    consensus.advance_commit_checks(8, &mut state).await;
    assert_eq!(state.highest_advanced_round, 8);

    // Repeated or stale advancement notifications never rescan old rounds.
    consensus.advance_commit_checks(5, &mut state).await;
    assert_eq!(state.highest_advanced_round, 8);
}

#[tokio::test]
async fn missing_leader_request_keeps_retrying_after_300_ms() {
    let committee = mock_committee();
    let (tx_primary, mut rx_primary) = channel(10);
    let mut consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary,
        tx_output: OutputSender::Individual(channel(10).0),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));

    consensus.request_missing_leader(2, &mut state).await;
    assert!(matches!(
        rx_primary.recv().await,
        Some(ConsensusCommand::LeaderRequest(2, _))
    ));

    consensus.retry_missing_leaders(&mut state).await;
    assert!(rx_primary.try_recv().is_err());

    tokio::time::sleep(LEADER_RETRY_DELAY + Duration::from_millis(20)).await;
    consensus.retry_missing_leaders(&mut state).await;
    assert!(matches!(
        rx_primary.recv().await,
        Some(ConsensusCommand::LeaderRequest(2, _))
    ));

    consensus.retry_missing_leaders(&mut state).await;
    assert!(rx_primary.try_recv().is_err());
    assert!(state.missing_leader_requests.contains_key(&2));

    tokio::time::sleep(LEADER_RETRY_DELAY + Duration::from_millis(20)).await;
    consensus.retry_missing_leaders(&mut state).await;
    assert!(matches!(
        rx_primary.recv().await,
        Some(ConsensusCommand::LeaderRequest(2, _))
    ));
}

#[tokio::test]
async fn skipped_leader_cancels_recovery_and_retries() {
    let committee = mock_committee();
    let (tx_primary, mut rx_primary) = channel(10);
    let mut consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary,
        tx_output: OutputSender::Individual(channel(10).0),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));

    consensus.request_missing_leader(2, &mut state).await;
    assert!(rx_primary.recv().await.is_some());
    consensus.mark_rule_three_skipped(2, &mut state);
    assert!(!state.rule_three_recovery.contains(&2));
    assert!(!state.missing_leader_requests.contains_key(&2));

    consensus.request_missing_leader(2, &mut state).await;
    assert!(rx_primary.try_recv().is_err());
}

#[test]
fn skipped_leader_vertex_can_still_be_force_admitted_as_causal_history() {
    let committee = mock_committee();
    let consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary: channel(10).0,
        tx_output: OutputSender::Individual(channel(10).0),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    let (skipped_digest, skipped) =
        mock_certificate(consensus.ordering_leader_authority(1), 1, BTreeSet::new());
    state.observe(skipped);
    consensus.mark_rule_three_skipped(1, &mut state);

    let (_, root) = mock_certificate(
        authorities[0],
        2,
        [skipped_digest.clone()].iter().cloned().collect(),
    );
    state.force_observed_history_to_dag(root, 2);

    assert!(state.skipped_leaders.contains(&1));
    assert!(state.dag_digests.contains(&skipped_digest));
}

#[test]
fn grade_two_waits_for_strong_and_weak_edges() {
    let committee = mock_committee();
    let genesis = Certificate::genesis(&committee);
    let genesis_parents = genesis.iter().map(|x| x.digest()).collect();
    let mut state = State::new(genesis);
    let authority = keys()[0].0;

    let (dependency_digest, dependency) = mock_certificate(authority, 1, genesis_parents);
    let (_, mut block) = mock_certificate(authority, 3, BTreeSet::new());
    block.header.weak_edges.insert(dependency_digest.clone());
    let block_digest = block.digest();

    state.insert_grade_one(block);
    state.mark_grade_two(block_digest.clone());
    assert!(state.promote_ready().is_empty());
    assert!(state.vdag.get(&3).unwrap().contains_key(&authority));

    state.insert_grade_one(dependency);
    state.mark_grade_two(dependency_digest);
    let promoted = state.promote_ready();
    assert_eq!(promoted.len(), 2);
    assert!(state.dag_digests.contains(&block_digest));
    assert!(state.vdag.get(&3).is_none());
}

#[test]
fn finds_paths_over_strong_and_weak_edges() {
    let committee = mock_committee();
    let consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary: channel(1).0,
        tx_output: OutputSender::Individual(channel(1).0),
        genesis: Certificate::genesis(&committee),
    };
    let authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();

    let (target_digest, target) = mock_certificate(authorities[0], 1, BTreeSet::new());
    let (middle_digest, middle) = mock_certificate(
        authorities[1],
        3,
        [target_digest.clone()].iter().cloned().collect(),
    );
    let (_, mut leader) = mock_certificate(authorities[2], 5, BTreeSet::new());
    leader.header.weak_edges.insert(middle_digest.clone());
    let leader_digest = leader.digest();

    let dag: Dag = [
        (
            1,
            [(target.origin(), (target_digest, target.clone()))]
                .iter()
                .cloned()
                .collect(),
        ),
        (
            3,
            [(middle.origin(), (middle_digest, middle))]
                .iter()
                .cloned()
                .collect(),
        ),
        (
            5,
            [(leader.origin(), (leader_digest, leader.clone()))]
                .iter()
                .cloned()
                .collect(),
        ),
    ]
    .iter()
    .cloned()
    .collect();

    assert!(consensus.linked_by_strong_or_weak(&leader, &target, &dag));

    let (_, unreachable) = mock_certificate(authorities[3], 2, BTreeSet::new());
    assert!(!consensus.linked_by_strong_or_weak(&leader, &unreachable, &dag));
}

#[test]
fn order_dag_includes_parents_and_weak_but_excludes_virtual_history() {
    let committee = mock_committee();
    let consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary: channel(1).0,
        tx_output: OutputSender::Individual(channel(1).0),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();

    let (weak_digest, weak_parent) = mock_certificate(authorities[1], 1, BTreeSet::new());
    let (virtual_digest, virtual_parent) = mock_certificate(authorities[2], 2, BTreeSet::new());
    let (parent_digest, parent) = mock_certificate(authorities[3], 2, BTreeSet::new());
    let (_, mut leader) = mock_certificate(authorities[0], 3, BTreeSet::new());
    leader.header.parents.insert(parent_digest.clone());
    leader.header.weak_edges.insert(weak_digest.clone());
    leader.header.virtual_edges.insert(virtual_digest.clone());
    leader.header.id = leader.header.digest();

    state.promote_to_dag(weak_parent.clone());
    state.promote_to_dag(virtual_parent.clone());
    state.promote_to_dag(parent.clone());
    state.promote_to_dag(leader.clone());
    let ordered = consensus.order_dag(&leader, &state);

    assert!(ordered.iter().any(|block| block.digest() == parent_digest));
    assert!(ordered.iter().any(|block| block.digest() == weak_digest));
    assert!(!ordered.iter().any(|block| block.digest() == virtual_digest));
    assert_eq!(ordered.last().unwrap().digest(), leader.digest());
}

#[test]
fn order_dag_breaks_same_round_ties_by_author_instead_of_digest_traversal() {
    let committee = mock_committee();
    let consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary: channel(1).0,
        tx_output: OutputSender::Individual(channel(1).0),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let mut authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    authorities.sort();
    let mut parents = BTreeSet::new();
    let mut ancestors = Vec::new();
    for authority in &authorities {
        let (digest, ancestor) = mock_certificate(*authority, 1, BTreeSet::new());
        parents.insert(digest.clone());
        ancestors.push((digest, ancestor.clone()));
        state.promote_to_dag(ancestor);
    }
    // DFS visits these BTreeSet parents in reverse digest order. The seeded
    // fixture deliberately makes that differ from the paper's author tie break.
    ancestors.sort_by(|left, right| right.0.cmp(&left.0));
    let digest_order: Vec<_> = ancestors.iter().map(|(_, block)| block.origin()).collect();
    assert_ne!(digest_order, authorities);
    let (_, leader) = mock_certificate(authorities[0], 2, parents);
    let ordered = consensus.order_dag(&leader, &state);

    let expected: Vec<_> = authorities
        .iter()
        .map(|author| (1, *author))
        .chain(std::iter::once((2, authorities[0])))
        .collect();
    let actual: Vec<_> = ordered
        .iter()
        .map(|block| (block.round(), block.origin()))
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn designates_one_leader_every_round() {
    let committee = mock_committee();
    let consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary: channel(1).0,
        tx_output: OutputSender::Individual(channel(1).0),
        genesis: Certificate::genesis(&committee),
    };
    let mut expected: Vec<_> = committee.authorities.keys().cloned().collect();
    expected.sort();

    for round in 0..8 {
        assert_eq!(
            consensus.leader_authority(round),
            expected[round as usize % expected.len()]
        );
    }
}

#[test]
fn commit_rule_counts_observed_and_dag_strong_support_separately() {
    let committee = mock_committee();
    let consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary: channel(1).0,
        tx_output: OutputSender::Individual(channel(1).0),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    let (leader_digest, leader) = mock_certificate(authorities[0], 1, BTreeSet::new());
    state.promote_to_dag(leader);

    let parents: BTreeSet<_> = [leader_digest.clone()].iter().cloned().collect();
    let support: Vec<_> = authorities
        .iter()
        .take(3)
        .map(|authority| mock_certificate(*authority, 2, parents.clone()).1)
        .collect();
    for certificate in &support {
        state.insert_grade_one(certificate.clone());
    }

    // Three out of four authorities are observed in Dag union VDag, while no
    // supporter has entered the formal Dag yet.
    assert_eq!(
        consensus.strong_support_stake(2, &leader_digest, &state),
        (3, 0)
    );

    state.promote_to_dag(support[0].clone());
    state.promote_to_dag(support[1].clone());
    assert_eq!(
        consensus.strong_support_stake(2, &leader_digest, &state),
        (3, 2)
    );
}

#[test]
fn commit_rule_one_counts_pre_grade_one_grbc_observations() {
    let committee = mock_committee();
    let consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary: channel(1).0,
        tx_output: OutputSender::Individual(channel(1).0),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    let (leader_digest, leader) = mock_certificate(authorities[0], 1, BTreeSet::new());
    state.promote_to_dag(leader);
    let parents: BTreeSet<_> = [leader_digest.clone()].iter().cloned().collect();
    for authority in authorities.iter().take(3) {
        state.observe(mock_certificate(*authority, 2, parents.clone()).1);
    }

    assert_eq!(
        consensus.strong_support_stake(2, &leader_digest, &state),
        (committee.quorum_threshold(), 0)
    );
}

#[test]
fn rule_one_does_not_count_locally_present_unreferenced_leader() {
    let committee = mock_committee();
    let consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary: channel(10).0,
        tx_output: OutputSender::Individual(channel(10).0),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    let (leader_digest, leader) = mock_certificate(authorities[0], 1, BTreeSet::new());
    state.observe(leader);

    // These blocks and the leader are all locally observed, but none of their
    // strong parents actually contains the leader digest.
    for authority in authorities.iter().take(3) {
        state.observe(mock_certificate(*authority, 2, BTreeSet::new()).1);
    }

    assert_eq!(
        consensus.strong_support_stake(2, &leader_digest, &state),
        (0, 0)
    );
}

#[tokio::test]
async fn rule_one_accepted_votes_promote_observed_leader_and_causal_history_to_dag() {
    let committee = mock_committee();
    let (tx_output, _rx_output) = channel(20);
    let (tx_primary, _rx_primary) = channel(20);
    let mut consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary,
        tx_output: OutputSender::Individual(tx_output),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let mut authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    authorities.sort();

    let (dependency_digest, dependency) = mock_certificate(authorities[1], 0, BTreeSet::new());
    let leader_parents = [dependency_digest.clone()].iter().cloned().collect();
    let (leader_digest, leader) = mock_certificate(authorities[0], 1, leader_parents);
    state.observe(dependency);
    state.observe(leader);

    let support_parents: BTreeSet<_> = [leader_digest.clone()].iter().cloned().collect();
    for authority in authorities.iter().take(2) {
        let block = mock_certificate(*authority, 2, support_parents.clone()).1;
        state.observe(block.clone());
        state.promote_to_dag(block);
    }

    consensus.evaluate_commit_rule_one(2, &mut state).await;

    assert!(state.committed_leaders.contains(&1));
    assert!(state.dag_digests.contains(&leader_digest));
    assert!(state.dag_digests.contains(&dependency_digest));
}

#[test]
fn commit_ready_leader_promotes_from_observe_with_strong_and_weak_history() {
    let committee = mock_committee();
    let consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary: channel(10).0,
        tx_output: OutputSender::Individual(channel(10).0),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();

    let (strong_digest, strong_parent) = mock_certificate(authorities[1], 1, BTreeSet::new());
    let (weak_digest, weak_parent) = mock_certificate(authorities[2], 1, BTreeSet::new());
    let (late_weak_digest, late_weak_parent) = mock_certificate(authorities[3], 1, BTreeSet::new());
    state.observe(strong_parent);
    state.observe(weak_parent);

    let (_, mut leader) = mock_certificate(
        consensus.ordering_leader_authority(2),
        2,
        [strong_digest.clone()].iter().cloned().collect(),
    );
    leader.header.weak_edges.insert(weak_digest.clone());
    leader.header.weak_edges.insert(late_weak_digest.clone());
    let leader_digest = leader.digest();

    // The commit decision exists before this node receives the leader.
    state.rule_three_recovery.insert(2);
    state.observe(leader.clone());
    consensus.promote_observed_pending_leader(2, &mut state);

    assert!(state.dag_digests.contains(&leader_digest));
    assert!(state.dag_digests.contains(&strong_digest));
    assert!(state.dag_digests.contains(&weak_digest));
    assert!(!state.dag_digests.contains(&late_weak_digest));

    // A missing causal dependency is promoted immediately when any verified
    // GRBC observation of it arrives, without waiting for grade 1 or grade 2.
    state.observe(late_weak_parent.clone());
    assert!(state.dag_digests.contains(&late_weak_digest));

    // A later grade-1 notification cannot put an already promoted block back
    // into VDag.
    state.insert_grade_one(leader);
    state.insert_grade_one(late_weak_parent);
    assert!(state.vdag.values().all(|round| round
        .values()
        .all(|(digest, _)| { digest != &leader_digest && digest != &late_weak_digest })));
}

#[test]
fn force_admission_does_not_follow_virtual_edges() {
    let committee = mock_committee();
    let mut state = State::new(Certificate::genesis(&committee));
    let authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    let (virtual_digest, virtual_block) = mock_certificate(authorities[1], 1, BTreeSet::new());
    state.observe(virtual_block);

    let (_, mut root) = mock_certificate(authorities[0], 2, BTreeSet::new());
    root.header.virtual_edges.insert(virtual_digest.clone());
    root.header.id = root.header.digest();
    state.force_observed_history_to_dag(root, 2);

    assert!(!state.dag_digests.contains(&virtual_digest));
}

#[test]
fn commit_rule_two_counts_strong_and_two_hop_virtual_paths() {
    let committee = mock_committee();
    let consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary: channel(10).0,
        tx_output: OutputSender::Individual(channel(10).0),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    let (leader_digest, leader) = mock_certificate(authorities[0], 1, BTreeSet::new());
    state.promote_to_dag(leader);

    // The middle block virtually references the leader.
    let (_, mut middle) = mock_certificate(authorities[1], 2, BTreeSet::new());
    middle.header.virtual_edges.insert(leader_digest.clone());
    let middle_digest = middle.digest();
    state.promote_to_dag(middle);

    // Three formal-Dag blocks use one strong edge to the middle block, making
    // an exact strong+virtual two-hop path to the leader.
    let parents: BTreeSet<_> = [middle_digest].iter().cloned().collect();
    for authority in authorities.iter().take(3) {
        let block = mock_certificate(*authority, 3, parents.clone()).1;
        state.promote_to_dag(block);
    }

    assert_eq!(
        consensus.rule_two_support_stake(3, &leader_digest, &state),
        (0, 0, 3)
    );
}

#[test]
fn rule_two_does_not_count_locally_present_unreferenced_leader() {
    let committee = mock_committee();
    let consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary: channel(10).0,
        tx_output: OutputSender::Individual(channel(10).0),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    let (leader_digest, leader) = mock_certificate(authorities[0], 1, BTreeSet::new());
    state.promote_to_dag(leader);

    // The intermediate block is locally present but does not virtually
    // reference the leader. Merely storing the leader cannot create a path.
    let (middle_digest, middle) = mock_certificate(authorities[1], 2, BTreeSet::new());
    state.promote_to_dag(middle);
    let parents: BTreeSet<_> = [middle_digest].iter().cloned().collect();
    for authority in authorities.iter().take(3) {
        state.promote_to_dag(mock_certificate(*authority, 3, parents.clone()).1);
    }

    assert_eq!(
        consensus.rule_two_support_stake(3, &leader_digest, &state),
        (0, 0, 0)
    );
}

#[test]
fn direct_fallback_counts_each_anchor_parent_once() {
    let committee = mock_committee();
    let consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary: channel(10).0,
        tx_output: OutputSender::Individual(channel(10).0),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let mut authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    authorities.sort();

    let (lower_digest, lower) = mock_certificate(authorities[0], 1, BTreeSet::new());
    state.promote_to_dag(lower);

    // Two different round-2 blocks virtually reference the lower leader.
    let mut second_digests = Vec::new();
    for authority in authorities.iter().take(2) {
        let (_, mut second) = mock_certificate(*authority, 2, BTreeSet::new());
        second.header.virtual_edges.insert(lower_digest.clone());
        second_digests.push(second.digest());
        state.promote_to_dag(second);
    }

    // The same anchor parent points to both round-2 blocks. It is one vote,
    // irrespective of how many qualifying virtual paths it carries.
    let second_parents = second_digests.into_iter().collect();
    let (first_digest, first) = mock_certificate(authorities[0], 3, second_parents);
    state.promote_to_dag(first);
    let first_digests = [first_digest].iter().cloned().collect();
    let (_, higher) = mock_certificate(authorities[3], 4, first_digests);

    assert_eq!(
        consensus.direct_fallback_stake(&higher, 1, authorities[0], &state),
        committee.stake(&authorities[0])
    );
}

#[test]
fn direct_fallback_counts_distinct_anchor_parent_proposers() {
    let committee = mock_committee();
    let consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary: channel(1).0,
        tx_output: OutputSender::Individual(channel(1).0),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let mut authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    authorities.sort();
    let (lower_digest, lower) = mock_certificate(authorities[0], 1, BTreeSet::new());
    state.promote_to_dag(lower);
    let (_, mut second) = mock_certificate(authorities[1], 2, BTreeSet::new());
    second.header.virtual_edges.insert(lower_digest.clone());
    let second_digest = second.digest();
    state.promote_to_dag(second);

    let mut first_digests = BTreeSet::new();
    for authority in authorities.iter().take(2) {
        let parents = [second_digest.clone()].iter().cloned().collect();
        let (digest, first) = mock_certificate(*authority, 3, parents);
        first_digests.insert(digest);
        state.promote_to_dag(first);
    }
    let (_, higher) = mock_certificate(authorities[3], 4, first_digests);
    assert_eq!(
        consensus.direct_fallback_stake(&higher, 1, authorities[0], &state),
        committee.validity_threshold()
    );
}

#[test]
fn unrelated_observation_does_not_dirty_fallback_anchor() {
    let committee = mock_committee();
    let mut state = State::new(Certificate::genesis(&committee));
    let authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    let (_, anchor) = mock_certificate(authorities[0], 4, BTreeSet::new());
    state.observe(anchor.clone());
    state.rule_three_anchors[1] = Some(4);
    state.mark_fallback_anchor_dirty(4);
    let snapshot = state.fallback_history_snapshot(&anchor);
    state.dirty_fallback_anchors.clear();

    state.observe(mock_certificate(authorities[1], 2, BTreeSet::new()).1);

    assert!(!state.dirty_fallback_anchors.contains(&4));
    assert_eq!(
        state.fallback_anchor_versions.get(&4),
        Some(&snapshot.version)
    );
}

#[test]
fn fallback_history_is_reused_at_the_same_version() {
    let committee = mock_committee();
    let mut state = State::new(Certificate::genesis(&committee));
    let authority = keys()[0].0;
    let (parent_digest, parent) = mock_certificate(authority, 3, BTreeSet::new());
    state.observe(parent);
    let (_, anchor) = mock_certificate(authority, 4, [parent_digest].iter().cloned().collect());
    state.observe(anchor.clone());
    state.rule_three_anchors[1] = Some(4);
    state.mark_fallback_anchor_dirty(4);
    let first = state.fallback_history_snapshot(&anchor);
    let marker = Digest::default();
    state
        .fallback_history_cache
        .get_mut(&4)
        .unwrap()
        .by_round
        .insert(99, vec![marker.clone()]);

    let second = state.fallback_history_snapshot(&anchor);

    assert_eq!(first.version, second.version);
    assert_eq!(second.by_round.get(&99), Some(&vec![marker]));
}

#[tokio::test]
async fn fallback_keeps_finalizing_after_committed_history_is_garbage_collected() {
    let committee = mock_committee();
    let (tx_primary, _rx_primary) = channel(1000);
    let (tx_output, mut rx_output) = channel(1000);
    let mut consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary,
        tx_output: OutputSender::Individual(tx_output),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let mut authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    authorities.sort();
    let mut parents: BTreeSet<_> = consensus.genesis.iter().map(|c| c.digest()).collect();
    let mut leaders = BTreeMap::new();
    let mut first_round = Vec::new();
    for round in 1..=56 {
        let mut next_parents = BTreeSet::new();
        for authority in &authorities {
            let (digest, certificate) = mock_certificate(*authority, round, parents.clone());
            if round == 1 {
                first_round.push(digest.clone());
            }
            if *authority == consensus.ordering_leader_authority(round) {
                leaders.insert(round, certificate.clone());
            }
            state.observe(certificate.clone());
            state.promote_to_dag(certificate);
            next_parents.insert(digest);
        }
        if round <= 52 {
            consensus
                .queue_leader_commit(leaders[&round].clone(), 3, &mut state)
                .await;
        }
        parents = next_parents;
    }
    while rx_output.try_recv().is_ok() {}
    assert_eq!(state.finalized_round, 52);
    assert!(first_round
        .iter()
        .all(|digest| !state.observed.contains_key(digest)));

    // Round 56 is a Good anchor for pending round 53 on the same lane.
    // Its causal suffix is complete; deleted ancestors below the finalized
    // prefix cannot change the direct strong-path decision for round 53.
    consensus
        .queue_leader_commit(leaders[&56].clone(), 1, &mut state)
        .await;

    assert!(
        state.committed_leaders.contains(&53),
        "GC must not strand the next fallback leader"
    );
    assert_eq!(state.finalized_round, 53);
    let mut emitted = Vec::new();
    while let Ok(certificate) = rx_output.try_recv() {
        emitted.push(certificate);
    }
    assert!(emitted
        .iter()
        .any(|certificate| certificate.digest() == leaders[&53].digest()));
    assert!(
        !state.committed_leaders.contains(&56),
        "the unresolved 54/55 prefix still blocks 56"
    );
}

#[test]
fn forced_history_does_not_retrieve_an_already_ordered_pruned_ancestor() {
    let committee = mock_committee();
    let mut state = State::new(Certificate::genesis(&committee));
    let authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    let genesis = state.dag_digests.iter().cloned().collect();
    let (old_digest, old) = mock_certificate(authorities[0], 1, genesis);
    state.observe(old.clone());
    state.promote_to_dag(old.clone());
    state.update(&[old], 2);
    let (_, newer) = mock_certificate(authorities[0], 4, BTreeSet::new());
    state.observe(newer.clone());
    state.promote_to_dag(newer.clone());
    state.update(&[newer], 2);
    assert!(!state.observed.contains_key(&old_digest));
    assert!(!state.dag_by_digest.contains_key(&old_digest));

    let (_, mut leader) = mock_certificate(authorities[1], 5, BTreeSet::new());
    leader.header.weak_edges.insert(old_digest);
    state.force_observed_history_to_dag(leader, 5);

    assert!(
        state.forced_history_waiters.is_empty(),
        "an already emitted ancestor needs neither retrieval nor repeated traversal"
    );
}

#[tokio::test]
async fn late_weak_history_survives_gc_until_its_exact_vertex_is_output() {
    let committee = mock_committee();
    let (tx_primary, _rx_primary) = channel(1000);
    let (tx_output, mut rx_output) = channel(1000);
    let mut consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary,
        tx_output: OutputSender::Individual(tx_output),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let mut authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    authorities.sort();
    let genesis: BTreeSet<_> = consensus.genesis.iter().map(Certificate::digest).collect();
    let (old_digest, old) = mock_certificate(authorities[3], 1, genesis.clone());
    state.observe(old.clone());
    state.promote_to_dag(old.clone());
    let mut parents = genesis;
    for round in 1..=52 {
        let mut next = BTreeSet::new();
        let mut leader = None;
        for authority in &authorities[..3] {
            let (digest, block) = mock_certificate(*authority, round, parents.clone());
            if *authority == consensus.ordering_leader_authority(round) {
                leader = Some(block.clone());
            }
            state.observe(block.clone());
            state.promote_to_dag(block);
            next.insert(digest);
        }
        consensus
            .queue_leader_commit(leader.unwrap(), 3, &mut state)
            .await;
        parents = next;
    }
    while rx_output.try_recv().is_ok() {}
    assert_eq!(state.finalized_round, 52);
    assert!(!state.ordered_digests.contains(&old_digest));
    let (_, mut leader) = mock_certificate(authorities[0], 53, parents);
    leader.header.weak_edges.insert(old_digest.clone());
    consensus
        .queue_leader_commit(leader.clone(), 3, &mut state)
        .await;
    let mut emitted = Vec::new();
    while let Ok(block) = rx_output.try_recv() {
        emitted.push(block.digest());
    }
    assert!(
        emitted.contains(&old_digest),
        "an unoutput old weak ancestor must be retained and actually emitted, not skipped by age"
    );
    assert!(emitted.contains(&leader.digest()));
    assert!(state.ordered_digests.contains(&old_digest));
    assert_eq!(state.finalized_round, 53);
}

#[tokio::test]
async fn older_unoutput_vertex_is_not_hidden_by_its_authors_newer_watermark() {
    let committee = mock_committee();
    let (tx_primary, _rx_primary) = channel(100);
    let (tx_output, mut rx_output) = channel(100);
    let mut consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary,
        tx_output: OutputSender::Individual(tx_output),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let origin = keys()[1].0;
    let (old_digest, old) = mock_certificate(origin, 1, BTreeSet::new());
    let (_, newer) = mock_certificate(origin, 4, BTreeSet::new());
    state.observe(old.clone());
    state.promote_to_dag(old);
    state.observe(newer.clone());
    state.promote_to_dag(newer.clone());
    state.update(&[newer], 50);
    state.finalized_round = 4;
    let (_, mut leader) =
        mock_certificate(consensus.ordering_leader_authority(5), 5, BTreeSet::new());
    leader.header.weak_edges.insert(old_digest.clone());
    consensus
        .queue_leader_commit(leader.clone(), 3, &mut state)
        .await;
    let emitted: Vec<_> = std::iter::from_fn(|| rx_output.try_recv().ok())
        .map(|c| c.digest())
        .collect();
    assert_eq!(
        emitted,
        vec![old_digest.clone(), leader.digest()],
        "only exact prior output can suppress an ancestor, not a newer round from the same author"
    );
    assert!(state.ordered_digests.contains(&old_digest));
    consensus.queue_leader_commit(leader, 3, &mut state).await;
    assert!(rx_output.try_recv().is_err());
}

#[test]
fn finish_after_gc_accepts_an_exact_ordered_dependency_but_waits_for_unknown_data() {
    let committee = mock_committee();
    let mut state = State::new(Certificate::genesis(&committee));
    let authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    let (old_digest, old) = mock_certificate(authorities[0], 1, BTreeSet::new());
    state.observe(old.clone());
    state.promote_to_dag(old.clone());
    state.update(&[old], 2);
    let (_, newer) = mock_certificate(authorities[0], 4, BTreeSet::new());
    state.observe(newer.clone());
    state.promote_to_dag(newer.clone());
    state.update(&[newer], 2);
    assert!(!state.observed.contains_key(&old_digest));

    let (_, mut child) = mock_certificate(authorities[1], 5, BTreeSet::new());
    child.header.weak_edges.insert(old_digest);
    let child_digest = child.digest();
    state.insert_grade_one(child);
    state.mark_grade_two(child_digest.clone());
    state.promote_ready();
    assert!(
        state.dag_digests.contains(&child_digest),
        "pruning metadata must not undo acceptance of an ordered dependency"
    );

    let (unknown_digest, _) = mock_certificate(authorities[0], 2, BTreeSet::new());
    let (_, mut unknown_child) = mock_certificate(authorities[2], 5, BTreeSet::new());
    unknown_child.header.weak_edges.insert(unknown_digest);
    let unknown_child_digest = unknown_child.digest();
    state.insert_grade_one(unknown_child);
    state.mark_grade_two(unknown_child_digest.clone());
    state.promote_ready();
    assert!(
        !state.dag_digests.contains(&unknown_child_digest),
        "an author's later watermark is not evidence for an unknown digest"
    );
}

#[tokio::test]
async fn ordered_frontier_rechecks_cached_fallback_without_another_message() {
    let committee = mock_committee();
    let (tx_primary, _rx_primary) = channel(100);
    let (tx_output, _rx_output) = channel(100);
    let mut consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary,
        tx_output: OutputSender::Individual(tx_output),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let mut authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    authorities.sort();
    let (_, mut first) = mock_certificate(authorities[0], 1, BTreeSet::new());
    // Virtual evidence is not causal output history. It blocks a snapshot
    // before this vertex is ordered, then becomes irrelevant to later targets.
    first.header.virtual_edges.insert(Digest::default());
    let mut parent = first.digest();
    state.observe(first.clone());
    state.promote_to_dag(first.clone());
    for round in 2..=4 {
        let (digest, vertex) =
            mock_certificate(authorities[1], round, [parent].iter().cloned().collect());
        state.observe(vertex.clone());
        state.promote_to_dag(vertex);
        parent = digest;
    }
    let (_, anchor) = mock_certificate(authorities[0], 5, [parent].iter().cloned().collect());
    state.observe(anchor.clone());
    state.promote_to_dag(anchor.clone());
    consensus
        .queue_leader_commit(anchor.clone(), 1, &mut state)
        .await;
    assert!(!state.fallback_history_snapshot(&anchor).complete);
    assert!(!state.skipped_leaders.contains(&2));

    consensus.stage_leader_commit(first, 3, &mut state);
    consensus.process_dirty_leaders(&mut state).await;

    assert!(
        state.skipped_leaders.contains(&2),
        "ordering a traversed vertex must invalidate and retry its cached incomplete snapshot"
    );
    assert_eq!(
        state.finalized_round, 2,
        "the existing event must drain work caused by the new ordered frontier"
    );
}

#[test]
fn awaited_fallback_digest_dirties_only_its_anchor() {
    let committee = mock_committee();
    let mut state = State::new(Certificate::genesis(&committee));
    let authority = keys()[0].0;
    let (missing_digest, missing) = mock_certificate(authority, 3, BTreeSet::new());
    let (_, anchor) = mock_certificate(
        authority,
        4,
        [missing_digest.clone()].iter().cloned().collect(),
    );
    state.observe(anchor.clone());
    state.rule_three_anchors[1] = Some(4);
    state.mark_fallback_anchor_dirty(4);
    let first = state.fallback_history_snapshot(&anchor);
    assert!(!first.complete);
    state.dirty_fallback_anchors.clear();

    state.observe(missing);

    let next_version = *state.fallback_anchor_versions.get(&4).unwrap();
    assert!(next_version > first.version);
    assert!(state.dirty_fallback_anchors.contains(&4));
    assert!(!state
        .fallback_evidence_waiters
        .contains_key(&missing_digest));
}

#[test]
fn late_strong_ancestor_dirties_anchor_using_the_descendant() {
    let committee = mock_committee();
    let mut state = State::new(Certificate::genesis(&committee));
    let authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    let (ancestor_digest, ancestor) = mock_certificate(authorities[0], 1, BTreeSet::new());
    state.observe(ancestor);
    let (parent_digest, parent) = mock_certificate(
        authorities[1],
        2,
        [ancestor_digest].iter().cloned().collect(),
    );
    let (child_digest, child) = mock_certificate(
        authorities[2],
        3,
        [parent_digest.clone()].iter().cloned().collect(),
    );
    state.observe(child);
    let (_, anchor) = mock_certificate(
        authorities[3],
        4,
        [child_digest.clone()].iter().cloned().collect(),
    );
    state.observe(anchor.clone());
    state.rule_three_anchors[1] = Some(4);
    state.mark_fallback_anchor_dirty(4);
    assert!(!state.fallback_history_snapshot(&anchor).complete);
    assert!(state
        .fallback_vertex_users
        .get(&child_digest)
        .unwrap()
        .contains(&4));
    // Isolate the reverse dependency on the already traversed child. The
    // parent's own missing-digest waiter is a separate wake-up path.
    state.fallback_evidence_waiters.remove(&parent_digest);
    state.dirty_fallback_anchors.clear();

    state.observe(parent);

    assert!(state.dirty_fallback_anchors.contains(&4));
}

#[test]
fn fallback_decision_cache_requires_the_anchor_version() {
    let committee = mock_committee();
    let mut state = State::new(Certificate::genesis(&committee));
    state.fallback_decision_cache.insert(
        (7, 1),
        FallbackDecision {
            version: 5,
            commit: true,
        },
    );

    assert_eq!(state.fallback_decision(7, 1, 5), Some(true));
    assert_eq!(state.fallback_decision(7, 1, 6), None);
}

#[tokio::test]
async fn rule_three_requests_a_leader_without_any_observed_certificate() {
    let committee = mock_committee();
    let (tx_primary, mut rx_primary) = channel(10);
    let (tx_output, _rx_output) = channel(10);
    let mut consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary,
        tx_output: OutputSender::Individual(tx_output),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let mut authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    authorities.sort();
    let leader_authority = consensus.ordering_leader_authority(1);
    let (_, mut dependency) = mock_certificate(authorities[1], 0, BTreeSet::new());
    dependency.header.weak_edges.insert(Digest::default());
    dependency.header.id = dependency.header.digest();
    let dependency_digest = dependency.digest();
    let leader_parents = [dependency_digest.clone()].iter().cloned().collect();
    let (_, mut leader) = mock_certificate(leader_authority, 1, leader_parents);
    leader.header.id = leader.header.digest();
    let leader_digest = leader.digest();
    let (_, mut higher) = mock_certificate(
        consensus.ordering_leader_authority(4),
        4,
        [leader_digest.clone()].iter().cloned().collect(),
    );
    higher.header.id = higher.header.digest();
    state.promote_to_dag(higher.clone());
    state.pending_leaders.insert(4, higher);
    state.rule_three_stacks[1].insert(1);
    state.rule_three_anchors[1] = Some(4);

    consensus.evaluate_commit_rule_three(&mut state).await;
    assert!(!state.skipped_leaders.contains(&1));
    assert!(matches!(
        rx_primary.try_recv(),
        Ok(ConsensusCommand::LeaderRequest(1, authority)) if authority == leader_authority
    ));
    assert!(!state.dag_digests.contains(&leader_digest));
    assert!(!state.dag_digests.contains(&dependency_digest));
}

#[tokio::test]
async fn direct_fallback_skips_missing_leader_when_complete_history_has_no_vote() {
    let committee = mock_committee();
    let (tx_primary, mut rx_primary) = channel(10);
    let mut consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary,
        tx_output: OutputSender::Individual(channel(10).0),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));

    // The direct anchor has a complete, empty causal view. The round-1
    // leader is absent and receives no strong or virtual fallback vote.
    let (_, anchor) = mock_certificate(consensus.ordering_leader_authority(4), 4, BTreeSet::new());
    state.observe(anchor.clone());
    state.promote_to_dag(anchor);
    state.rule_three_stacks[1].insert(1);
    state.rule_three_anchors[1] = Some(4);

    consensus.evaluate_commit_rule_three(&mut state).await;

    assert!(state.skipped_leaders.contains(&1));
    assert!(!state.rule_three_recovery.contains(&1));
    assert!(!state.missing_leader_requests.contains_key(&1));
    assert!(rx_primary.try_recv().is_err());
}

#[tokio::test]
async fn rule_three_skips_locally_known_leader_not_referenced_by_observer_history() {
    let committee = mock_committee();
    let (tx_primary, _rx_primary) = channel(10);
    let (tx_output, _rx_output) = channel(10);
    let mut consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary,
        tx_output: OutputSender::Individual(tx_output),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));

    let (_, target) = mock_certificate(consensus.ordering_leader_authority(1), 1, BTreeSet::new());
    state.observe(target);
    let (_, observer) =
        mock_certificate(consensus.ordering_leader_authority(4), 4, BTreeSet::new());
    state.observe(observer.clone());
    state.promote_to_dag(observer.clone());
    state.pending_leaders.insert(4, observer);
    state.rule_three_stacks[1].insert(1);
    state.rule_three_anchors[1] = Some(4);

    consensus.evaluate_commit_rule_three(&mut state).await;

    assert!(state.skipped_leaders.contains(&1));
    assert!(!state.committed_leaders.contains(&1));
}

#[tokio::test]
async fn fallback_does_not_jump_over_a_missing_stack_entry() {
    let committee = mock_committee();
    let (tx_primary, _rx_primary) = channel(20);
    let (tx_output, _rx_output) = channel(20);
    let mut consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary,
        tx_output: OutputSender::Individual(tx_output),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let mut authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    authorities.sort();

    let (target_digest, target) =
        mock_certificate(consensus.ordering_leader_authority(1), 1, BTreeSet::new());
    state.observe(target);

    // Build a three-strong prefix L7 -> B6 -> B5 -> B4, followed by
    // f+1 strong,strong,virtual suffixes distinguished by their B3 vertex.
    let (_, mut b2) = mock_certificate(authorities[1], 2, BTreeSet::new());
    b2.header.virtual_edges.insert(target_digest);
    b2.header.id = b2.header.digest();
    let b2_digest = b2.digest();
    state.observe(b2);

    let b2_parent: BTreeSet<_> = [b2_digest].iter().cloned().collect();
    let mut b3_digests = BTreeSet::new();
    for authority in authorities.iter().skip(2).take(2) {
        let (digest, b3) = mock_certificate(*authority, 3, b2_parent.clone());
        b3_digests.insert(digest);
        state.observe(b3);
    }
    let (b4_digest, b4) = mock_certificate(authorities[1], 4, b3_digests);
    state.observe(b4);
    let (b5_digest, b5) =
        mock_certificate(authorities[2], 5, [b4_digest].iter().cloned().collect());
    state.observe(b5);
    let (_, mut b6) = mock_certificate(authorities[3], 6, [b5_digest].iter().cloned().collect());
    b6.header.weak_edges.insert(Digest::default());
    b6.header.id = b6.header.digest();
    let b6_digest = b6.digest();
    state.observe(b6);

    let (_, higher) = mock_certificate(
        consensus.ordering_leader_authority(7),
        7,
        [b6_digest].iter().cloned().collect(),
    );
    state.observe(higher.clone());
    state.promote_to_dag(higher.clone());
    state.pending_leaders.insert(7, higher);
    state.rule_three_stacks[1].extend([1, 4]);
    state.rule_three_anchors[1] = Some(7);

    consensus.evaluate_commit_rule_three(&mut state).await;

    assert!(!state.skipped_leaders.contains(&4));
    assert!(!state.committed_leaders.contains(&1));
    assert!(state.rule_three_recovery.contains(&4));
}

#[tokio::test]
async fn fallback_requests_only_the_newest_missing_stack_entry() {
    let committee = mock_committee();
    let (tx_primary, mut rx_primary) = channel(20);
    let (tx_output, _rx_output) = channel(20);
    let mut consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary,
        tx_output: OutputSender::Individual(tx_output),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let mut authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    authorities.sort();

    let (target_digest, target) =
        mock_certificate(consensus.ordering_leader_authority(1), 1, BTreeSet::new());
    state.observe(target);

    let mut parent = target_digest;
    for round in 2..10 {
        let authority = if round == 4 || round == 7 {
            authorities
                .iter()
                .copied()
                .find(|authority| *authority != consensus.ordering_leader_authority(round))
                .unwrap()
        } else {
            authorities[(round as usize) % authorities.len()]
        };
        let (digest, block) =
            mock_certificate(authority, round, [parent].iter().cloned().collect());
        state.observe(block);
        parent = digest;
    }

    let (_, mut observer) = mock_certificate(
        consensus.ordering_leader_authority(10),
        10,
        [parent].iter().cloned().collect(),
    );
    observer.header.weak_edges.insert(Digest::default());
    observer.header.id = observer.header.digest();
    state.observe(observer.clone());
    state.promote_to_dag(observer.clone());
    state.pending_leaders.insert(10, observer);
    state.rule_three_stacks[1].extend([1, 4, 7]);
    state.rule_three_anchors[1] = Some(10);

    consensus.evaluate_commit_rule_three(&mut state).await;

    assert!(!state.skipped_leaders.contains(&7));
    assert!(!state.skipped_leaders.contains(&4));
    assert!(!state.committed_leaders.contains(&1));
    assert!(matches!(
        rx_primary.try_recv(),
        Ok(ConsensusCommand::LeaderRequest(7, _))
    ));
}

#[tokio::test]
async fn commit_rule_two_virtual_votes_force_accept_without_fabricating_finish() {
    let committee = mock_committee();
    let (tx_primary, _rx_primary) = channel(100);
    let (tx_output, _rx_output) = channel(100);
    let mut consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary,
        tx_output: OutputSender::Individual(tx_output),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let mut authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    authorities.sort();

    let (leader_digest, leader) = mock_certificate(authorities[0], 1, BTreeSet::new());
    state.insert_grade_one(leader);
    let (_, mut middle) = mock_certificate(authorities[1], 2, BTreeSet::new());
    middle.header.virtual_edges.insert(leader_digest.clone());
    let middle_digest = middle.digest();
    state.promote_to_dag(middle);
    let parents: BTreeSet<_> = [middle_digest].iter().cloned().collect();
    for authority in authorities.iter().take(3) {
        state.promote_to_dag(mock_certificate(*authority, 3, parents.clone()).1);
    }

    assert!(!state.grade_two.contains(&leader_digest));
    consensus.evaluate_commit_rule_two(3, &mut state).await;
    assert!(!state.grade_two.contains(&leader_digest));
    assert!(state.dag_digests.contains(&leader_digest));
    assert!(state.committed_leaders.contains(&1));
}

#[tokio::test]
async fn leader_commits_wait_for_the_previous_leader() {
    let committee = mock_committee();
    let (tx_primary, _rx_primary) = channel(100);
    let (tx_output, _rx_output) = channel(100);
    let mut consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary,
        tx_output: OutputSender::Individual(tx_output),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    let (_, leader_one) = mock_certificate(authorities[0], 1, BTreeSet::new());
    let (_, leader_two) = mock_certificate(authorities[0], 2, BTreeSet::new());
    state.promote_to_dag(leader_one.clone());
    state.promote_to_dag(leader_two.clone());

    consensus
        .queue_leader_commit(leader_two, 1, &mut state)
        .await;
    assert!(!state.committed_leaders.contains(&2));
    assert!(state.pending_leaders.contains_key(&2));

    consensus
        .queue_leader_commit(leader_one, 1, &mut state)
        .await;
    assert!(state.committed_leaders.contains(&1));
    assert!(state.committed_leaders.contains(&2));
    assert!(state.pending_leaders.is_empty());
}

#[tokio::test]
async fn good_observed_quorum_waits_for_leader_acceptance() {
    let committee = mock_committee();
    let (tx_primary, _rx_primary) = channel(100);
    let (tx_output, mut rx_output) = channel(100);
    let mut consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary,
        tx_output: OutputSender::Individual(tx_output),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let mut authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    authorities.sort();
    let (leader_digest, leader) = mock_certificate(authorities[0], 1, BTreeSet::new());
    state.insert_grade_one(leader.clone());
    for authority in authorities.iter().take(3) {
        state.observe(
            mock_certificate(
                *authority,
                2,
                [leader_digest.clone()].iter().cloned().collect(),
            )
            .1,
        );
    }

    consensus.evaluate_commit_rule_one(2, &mut state).await;
    assert!(!state.committed_leaders.contains(&1));
    assert!(!state.pending_leaders.contains_key(&1));
    assert!(rx_output.try_recv().is_err());

    state.mark_grade_two(leader_digest);
    state.promote_ready();
    // The ingestion loop dirties the designated leader on its FINISH event.
    state.dirty_leaders.insert(1);
    consensus.process_dirty_leaders(&mut state).await;
    assert!(state.committed_leaders.contains(&1));
    assert_eq!(rx_output.try_recv().unwrap(), leader);
}

#[tokio::test]
async fn deferred_strong_votes_commit_observed_only_leader_on_late_middle_arrival() {
    let committee = mock_committee();
    let (tx_primary, _rx_primary) = channel(100);
    let (tx_output, mut rx_output) = channel(100);
    let mut consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary,
        tx_output: OutputSender::Individual(tx_output),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let mut authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    authorities.sort();
    let (leader_digest, leader) = mock_certificate(authorities[0], 1, BTreeSet::new());
    state.observe(leader.clone());
    let (middle_digest, middle) = mock_certificate(
        authorities[1],
        2,
        [leader_digest.clone()].iter().cloned().collect(),
    );
    for authority in authorities.iter().take(3) {
        state.observe(
            mock_certificate(
                *authority,
                3,
                [middle_digest.clone()].iter().cloned().collect(),
            )
            .1,
        );
    }
    consensus.evaluate_commit_rule_two(3, &mut state).await;
    assert!(!state.committed_leaders.contains(&1));

    let dirty = state.observe(middle);
    state.dirty_leaders.extend(dirty);
    consensus.process_dirty_leaders(&mut state).await;
    assert!(state.committed_leaders.contains(&1));
    assert!(!state.grade_two.contains(&leader_digest));
    assert_eq!(rx_output.try_recv().unwrap(), leader);
}

#[tokio::test]
async fn skipped_successor_does_not_release_past_undecided_prefix() {
    let committee = mock_committee();
    let (tx_primary, _rx_primary) = channel(100);
    let (tx_output, mut rx_output) = channel(100);
    let mut consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary,
        tx_output: OutputSender::Individual(tx_output),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let authority = consensus.ordering_leader_authority(1);
    let (_, leader_one) = mock_certificate(authority, 1, BTreeSet::new());
    let (_, leader_three) = mock_certificate(authority, 3, BTreeSet::new());
    consensus
        .queue_leader_commit(leader_three.clone(), 3, &mut state)
        .await;
    consensus.mark_rule_three_skipped(2, &mut state);
    consensus.drain_ready_leaders(&mut state).await;
    assert!(!state.committed_leaders.contains(&3));
    assert!(rx_output.try_recv().is_err());

    consensus
        .queue_leader_commit(leader_one.clone(), 3, &mut state)
        .await;
    assert!(state.committed_leaders.contains(&1));
    assert!(state.committed_leaders.contains(&3));
    assert_eq!(rx_output.try_recv().unwrap(), leader_one);
    assert_eq!(rx_output.try_recv().unwrap(), leader_three);
}

#[tokio::test]
async fn pending_commit_waits_for_late_causal_history_then_releases_immediately() {
    let committee = mock_committee();
    let (tx_primary, mut rx_primary) = channel(100);
    let (tx_output, mut rx_output) = channel(100);
    let mut consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary,
        tx_output: OutputSender::Individual(tx_output),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let authority = consensus.ordering_leader_authority(2);
    let (ancestor_digest, ancestor) = mock_certificate(authority, 1, BTreeSet::new());
    let (_, mut leader) = mock_certificate(authority, 2, BTreeSet::new());
    leader.header.weak_edges.insert(ancestor_digest.clone());
    consensus.mark_rule_three_skipped(1, &mut state);
    consensus
        .queue_leader_commit(leader.clone(), 3, &mut state)
        .await;
    assert!(!state.committed_leaders.contains(&2));
    assert!(rx_output.try_recv().is_err());
    assert!(rx_primary.try_recv().is_err());

    let dirty = state.observe(ancestor.clone());
    consensus.refresh_pending_orders(&dirty, &mut state);
    state.dirty_leaders.extend(dirty);
    consensus.process_dirty_leaders(&mut state).await;
    assert!(state.committed_leaders.contains(&2));
    assert_eq!(rx_output.try_recv().unwrap(), ancestor);
    assert_eq!(rx_output.try_recv().unwrap(), leader);
    assert!(matches!(
        rx_primary.try_recv(),
        Ok(ConsensusCommand::Cleanup(_))
    ));
    assert!(matches!(
        rx_primary.try_recv(),
        Ok(ConsensusCommand::Cleanup(_))
    ));
}

#[tokio::test]
async fn shared_force_admitted_history_keeps_each_pending_leader_waiting() {
    let committee = mock_committee();
    let (tx_primary, _rx_primary) = channel(100);
    let (tx_output, mut rx_output) = channel(100);
    let mut consensus = Consensus {
        committee: committee.clone(),
        gc_depth: 50,
        rx_primary: channel(1).1,
        tx_primary,
        tx_output: OutputSender::Individual(tx_output),
        genesis: Certificate::genesis(&committee),
    };
    let mut state = State::new(Certificate::genesis(&committee));
    let mut authorities: Vec<_> = keys().into_iter().map(|(key, _)| key).collect();
    authorities.sort();
    let (ancestor_digest, ancestor) = mock_certificate(authorities[2], 1, BTreeSet::new());
    let (shared_digest, shared) = mock_certificate(
        authorities[1],
        2,
        [ancestor_digest].iter().cloned().collect(),
    );
    state.observe(shared);
    let (_, leader_four) = mock_certificate(
        authorities[0],
        4,
        [shared_digest.clone()].iter().cloned().collect(),
    );
    consensus
        .queue_leader_commit(leader_four.clone(), 3, &mut state)
        .await;
    assert!(state.dag_digests.contains(&shared_digest));

    consensus.mark_rule_three_skipped(1, &mut state);
    consensus.mark_rule_three_skipped(2, &mut state);
    let (_, leader_three) = mock_certificate(
        authorities[0],
        3,
        [shared_digest.clone()].iter().cloned().collect(),
    );
    consensus
        .queue_leader_commit(leader_three.clone(), 3, &mut state)
        .await;
    assert!(!state.committed_leaders.contains(&3));
    assert!(rx_output.try_recv().is_err());

    let dirty = state.observe(ancestor.clone());
    consensus.refresh_pending_orders(&dirty, &mut state);
    state.dirty_leaders.extend(dirty);
    consensus.process_dirty_leaders(&mut state).await;
    assert_eq!(rx_output.try_recv().unwrap(), ancestor);
    assert_eq!(rx_output.try_recv().unwrap().digest(), shared_digest);
    assert_eq!(rx_output.try_recv().unwrap(), leader_three);
    assert_eq!(rx_output.try_recv().unwrap(), leader_four);
}

// Run for 4 dag rounds in ideal conditions (all nodes reference all other nodes). We should commit
// the leader of round 2.
#[tokio::test]
async fn commit_one() {
    // Make certificates for rounds 1 to 4.
    let keys: Vec<_> = keys().into_iter().map(|(x, _)| x).collect();
    let genesis = Certificate::genesis(&mock_committee())
        .iter()
        .map(|x| x.digest())
        .collect::<BTreeSet<_>>();
    let (mut certificates, next_parents) = make_certificates(1, 4, &genesis, &keys);

    // Make one certificate with round 5 to trigger the commits.
    let (_, certificate) = mock_certificate(keys[0], 5, next_parents);
    certificates.push_back(certificate);

    // Spawn the consensus engine and sink the primary channel.
    let (tx_waiter, rx_waiter) = channel(1);
    let (tx_primary, mut rx_primary) = channel(1);
    let (tx_output, mut rx_output) = channel(1);
    Consensus::spawn(
        mock_committee(),
        /* gc_depth */ 50,
        rx_waiter,
        tx_primary,
        tx_output,
    );
    tokio::spawn(async move { while rx_primary.recv().await.is_some() {} });

    // Feed all certificates to the consensus. Only the last certificate should trigger
    // commits, so the task should not block.
    tokio::spawn(async move {
        while let Some(certificate) = certificates.pop_front() {
            deliver(&tx_waiter, certificate).await;
        }
    });

    // Ensure the first 4 ordered certificates are from round 1 (they are the parents of the committed
    // leader); then the leader's certificate should be committed.
    for _ in 1..=4 {
        let certificate = rx_output.recv().await.unwrap();
        assert_eq!(certificate.round(), 1);
    }
    let certificate = rx_output.recv().await.unwrap();
    assert_eq!(certificate.round(), 2);
}

// Run for 8 dag rounds with one dead node node (that is not a leader). We should commit the leaders of
// rounds 2, 4, and 6.
#[tokio::test]
async fn dead_node() {
    // Make the certificates.
    let mut keys: Vec<_> = keys().into_iter().map(|(x, _)| x).collect();
    keys.sort(); // Ensure we don't remove one of the leaders.
    let _ = keys.pop().unwrap();

    let genesis = Certificate::genesis(&mock_committee())
        .iter()
        .map(|x| x.digest())
        .collect::<BTreeSet<_>>();

    let (mut certificates, _) = make_certificates(1, 9, &genesis, &keys);

    // Spawn the consensus engine and sink the primary channel.
    let (tx_waiter, rx_waiter) = channel(1);
    let (tx_primary, mut rx_primary) = channel(1);
    let (tx_output, mut rx_output) = channel(1);
    Consensus::spawn(
        mock_committee(),
        /* gc_depth */ 50,
        rx_waiter,
        tx_primary,
        tx_output,
    );
    tokio::spawn(async move { while rx_primary.recv().await.is_some() {} });

    // Feed all certificates to the consensus.
    tokio::spawn(async move {
        while let Some(certificate) = certificates.pop_front() {
            deliver(&tx_waiter, certificate).await;
        }
    });

    // We should commit 3 leaders (rounds 2, 4, and 6).
    for i in 1..=15 {
        let certificate = rx_output.recv().await.unwrap();
        let expected = ((i - 1) / keys.len() as u64) + 1;
        assert_eq!(certificate.round(), expected);
    }
    let certificate = rx_output.recv().await.unwrap();
    assert_eq!(certificate.round(), 6);
}

// Run for 6 dag rounds. The leaders of round 2 does not have enough support, but the leader of
// round 4 does. The leader of rounds 2 and 4 should thus be committed upon entering round 6.
#[tokio::test]
async fn not_enough_support() {
    let mut keys: Vec<_> = keys().into_iter().map(|(x, _)| x).collect();
    keys.sort();

    let genesis = Certificate::genesis(&mock_committee())
        .iter()
        .map(|x| x.digest())
        .collect::<BTreeSet<_>>();

    let mut certificates = VecDeque::new();

    // Round 1: Fully connected graph.
    let nodes: Vec<_> = keys.iter().cloned().take(3).collect();
    let (out, parents) = make_certificates(1, 1, &genesis, &nodes);
    certificates.extend(out);

    // Round 2: Fully connect graph. But remember the digest of the leader. Note that this
    // round is the only one with 4 certificates.
    let (leader_2_digest, certificate) = mock_certificate(keys[0], 2, parents.clone());
    certificates.push_back(certificate);

    let nodes: Vec<_> = keys.iter().cloned().skip(1).collect();
    let (out, mut parents) = make_certificates(2, 2, &parents, &nodes);
    certificates.extend(out);

    // Round 3: Only node 0 links to the leader of round 2.
    let mut next_parents = BTreeSet::new();

    let name = &keys[1];
    let (digest, certificate) = mock_certificate(*name, 3, parents.clone());
    certificates.push_back(certificate);
    next_parents.insert(digest);

    let name = &keys[2];
    let (digest, certificate) = mock_certificate(*name, 3, parents.clone());
    certificates.push_back(certificate);
    next_parents.insert(digest);

    let name = &keys[0];
    parents.insert(leader_2_digest);
    let (digest, certificate) = mock_certificate(*name, 3, parents.clone());
    certificates.push_back(certificate);
    next_parents.insert(digest);

    parents = next_parents.clone();

    // Rounds 4, 5, and 6: Fully connected graph.
    let nodes: Vec<_> = keys.iter().cloned().take(3).collect();
    let (out, parents) = make_certificates(4, 6, &parents, &nodes);
    certificates.extend(out);

    // Round 7: Send a single certificate to trigger the commits.
    let (_, certificate) = mock_certificate(keys[0], 7, parents);
    certificates.push_back(certificate);

    // Spawn the consensus engine and sink the primary channel.
    let (tx_waiter, rx_waiter) = channel(1);
    let (tx_primary, mut rx_primary) = channel(1);
    let (tx_output, mut rx_output) = channel(1);
    Consensus::spawn(
        mock_committee(),
        /* gc_depth */ 50,
        rx_waiter,
        tx_primary,
        tx_output,
    );
    tokio::spawn(async move { while rx_primary.recv().await.is_some() {} });

    // Feed all certificates to the consensus. Only the last certificate should trigger
    // commits, so the task should not block.
    tokio::spawn(async move {
        while let Some(certificate) = certificates.pop_front() {
            deliver(&tx_waiter, certificate).await;
        }
    });

    // We should commit 2 leaders (rounds 2 and 4).
    for _ in 1..=3 {
        let certificate = rx_output.recv().await.unwrap();
        assert_eq!(certificate.round(), 1);
    }
    for _ in 1..=4 {
        let certificate = rx_output.recv().await.unwrap();
        assert_eq!(certificate.round(), 2);
    }
    for _ in 1..=3 {
        let certificate = rx_output.recv().await.unwrap();
        assert_eq!(certificate.round(), 3);
    }
    let certificate = rx_output.recv().await.unwrap();
    assert_eq!(certificate.round(), 4);
}

// Complete anchor histories without a vote for the missing leaders allow Rule 3
// to skip them. Missing-leader retrieval is required only by a positive recovery
// decision or by incomplete causal/virtual evidence (covered above).
#[tokio::test]
async fn missing_leaders_with_complete_vote_free_history_do_not_block_output() {
    let mut keys: Vec<_> = keys().into_iter().map(|(x, _)| x).collect();
    keys.sort();

    let genesis = Certificate::genesis(&mock_committee())
        .iter()
        .map(|x| x.digest())
        .collect::<BTreeSet<_>>();

    let mut certificates = VecDeque::new();

    // Remove the leader for rounds 1 and 2.
    let nodes: Vec<_> = keys.iter().cloned().skip(1).collect();
    let (out, parents) = make_certificates(1, 2, &genesis, &nodes);
    certificates.extend(out);

    // Add back the leader for rounds 3, 4, 5 and 6.
    let (out, parents) = make_certificates(3, 6, &parents, &keys);
    certificates.extend(out);

    // Add a certificate of round 7 to commit the leader of round 4.
    let (_, certificate) = mock_certificate(keys[0], 7, parents.clone());
    certificates.push_back(certificate);

    // Spawn the consensus engine and sink the primary channel.
    let (tx_waiter, rx_waiter) = channel(1);
    let (tx_primary, mut rx_primary) = channel(1);
    let (tx_output, mut rx_output) = channel(1);
    Consensus::spawn(
        mock_committee(),
        /* gc_depth */ 50,
        rx_waiter,
        tx_primary,
        tx_output,
    );
    // Feed all certificates to the consensus.
    tokio::spawn(async move {
        while let Some(certificate) = certificates.pop_front() {
            deliver(&tx_waiter, certificate).await;
        }
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            tokio::select! {
                command = rx_primary.recv() => match command {
                    Some(ConsensusCommand::LeaderRequest(..)) => panic!("vote-free complete history must permit skipping without retrieval"),
                    Some(_) => {},
                    None => panic!("consensus command channel closed before output"),
                },
                block = rx_output.recv() => match block {
                    Some(block) if block.round() >= 4 => break,
                    Some(_) => {},
                    None => panic!("application output channel closed"),
                },
            }
        }
    })
    .await
    .expect("complete fallback history did not release the finalized prefix");
}
