use super::*;
use crate::common::committee;
use serde_json::json;

fn policy(mode: &str) -> Policy {
    let committee = committee();
    let keys: Vec<_> = committee.authorities.keys().copied().collect();
    Policy::parse(
        &json!({"mode": mode, "byzantine_authorities": if mode == "none" { Vec::<String>::new() } else { vec![keys[0].encode_base64()] }}).to_string(),
        keys[0],
        &committee,
    )
    .unwrap()
}

#[test]
fn fixed_membership_never_rotates_and_silence_is_round_bounded() {
    let mut policy = policy("silent");
    policy.config.start_round = 2;
    policy.config.rounds = 3;
    for round in 0..20 {
        assert_eq!(policy.silent(policy.name, round), (2..5).contains(&round));
        assert!(!policy.silent(policy.leaders[1], round));
    }
}

#[test]
fn strict_configuration_rejects_unknown_keys_modes_members_and_unsafe_limits() {
    let committee = committee();
    let name = *committee.authorities.keys().next().unwrap();
    let keys: Vec<_> = committee
        .authorities
        .keys()
        .map(PublicKey::encode_base64)
        .collect();
    for value in [
        json!({"mode":"A3"}),
        json!({"mode":"silent", "typo":1}),
        json!({"mode":"silent", "byzantine_authorities":["AA=="]}),
        json!({"mode":"silent", "byzantine_authorities":[PublicKey([77;32]).encode_base64()]}),
        json!({"mode":"network", "delay_ms":0}),
        json!({"mode":"network", "max_pending_messages":0}),
        json!({"mode":"network", "max_pending_bytes":0}),
        json!({"mode":"network", "start_round":0}),
        json!({"mode":"network", "start_round":u64::MAX, "rounds":2}),
        json!({"mode":"network", "delay_ms":u64::MAX}),
        json!({"mode":"silent"}),
        json!({"mode":"targeted"}),
        json!({"mode":"none", "byzantine_authorities":[keys[0]]}),
        json!({"mode":"targeted", "byzantine_authorities":[keys[0], keys[1]]}),
        json!({"mode":"targeted", "byzantine_authorities":[keys[0], keys[0]]}),
    ] {
        assert!(
            Policy::parse(&value.to_string(), name, &committee).is_err(),
            "accepted {}",
            value
        );
    }
}

#[test]
fn none_is_noop_and_targeted_cannot_delay_an_honest_relay() {
    let mut policy = policy("none");
    let leader = policy.leaders[1];
    for peer in &policy.leaders {
        assert!(!policy.should_delay(1, &[leader], *peer));
    }
    policy.config.mode = Mode::Targeted;
    policy.byzantine.insert(policy.leaders[0]);
    policy.name = policy.leaders[2];
    for round in 1..12 {
        for peer in &policy.leaders {
            assert!(!policy.should_delay(round, &[policy.leader(round)], *peer));
        }
    }
}

#[test]
fn target_is_actual_leader_and_recipient_groups_are_deterministic() {
    let policy = policy("targeted");
    for round in 1..12 {
        let leader = policy.leader(round);
        let recipients = policy
            .leaders
            .iter()
            .filter(|peer| policy.should_delay(round, &[leader], **peer))
            .count();
        assert!(recipients > 0 && recipients < policy.leaders.len());
        let other = policy.leaders[(round as usize + 1) % policy.leaders.len()];
        for peer in &policy.leaders {
            assert!(!policy.should_delay(round, &[other], *peer));
        }
    }
}

#[test]
fn network_can_delay_honest_evidence_but_never_outside_the_window() {
    let mut policy = policy("network");
    policy.name = policy.leaders[2];
    policy.config.start_round = 3;
    policy.config.rounds = 2;
    for round in 1..8 {
        let count = policy
            .leaders
            .iter()
            .filter(|peer| policy.should_delay(round, &[policy.leader(round)], **peer))
            .count();
        assert_eq!(count > 0, (3..5).contains(&round));
    }
}

#[test]
fn native_evidence_and_repairs_follow_the_round_policy() {
    use crate::common::{certificate, votes};
    use crate::primary::PrimaryMessage;

    let mut policy = policy("targeted");
    let header = signed_leader_header(&policy);
    let vote = votes(&header).remove(0);
    let target = policy
        .leaders
        .iter()
        .copied()
        .find(|peer| policy.should_delay(header.round, &[header.author], *peer))
        .unwrap();
    let address = committee().primary(&target).unwrap().primary_to_primary;
    let frames = [
        PrimaryMessage::Header(header.clone()),
        PrimaryMessage::GradeOneVote(vote.clone()),
        PrimaryMessage::GradeOneVoteBatch(vec![vote]),
        PrimaryMessage::Certificate(certificate(&header)),
    ];
    for frame in &frames {
        let wire = Bytes::from(bincode::serialize(frame).unwrap());
        assert!(matches!(
            policy.delivery(address, &wire),
            Delivery::Delay(..)
        ));
    }
    let request = PrimaryMessage::LeaderRequest(header.round, header.author, target);
    let wire = Bytes::from(bincode::serialize(&request).unwrap());
    assert!(matches!(policy.delivery(address, &wire), Delivery::Send));

    policy.name = policy.leaders[1];
    for frame in &frames {
        let wire = Bytes::from(bincode::serialize(frame).unwrap());
        assert!(matches!(policy.delivery(address, &wire), Delivery::Send));
    }
    policy.config.mode = Mode::Network;
    for frame in &frames {
        let wire = Bytes::from(bincode::serialize(frame).unwrap());
        assert!(matches!(
            policy.delivery(address, &wire),
            Delivery::Delay(..)
        ));
    }
}

fn signed_leader_header(policy: &Policy) -> crate::messages::Header {
    use crypto::{Hash as _, Signature};
    let mut header = crate::common::header();
    let index = policy
        .leaders
        .iter()
        .position(|key| *key == header.author)
        .unwrap();
    header.round = if index == 0 {
        policy.leaders.len() as u64
    } else {
        index as u64
    };
    header.id = header.digest();
    let (_, secret) = crate::common::keys()
        .into_iter()
        .find(|(key, _)| *key == header.author)
        .unwrap();
    header.signature = Signature::new(&header.id, &secret);
    header.verify(&committee()).unwrap();
    header
}

#[test]
fn default_configuration_is_a_direct_noop_and_round_zero_is_outside_window() {
    let committee = committee();
    let name = *committee.authorities.keys().next().unwrap();
    let policy = Policy::parse("{}", name, &committee).unwrap();
    let address = committee.primary(&name).unwrap().primary_to_primary;
    assert!(matches!(
        policy.delivery(address, &Bytes::new()),
        Delivery::Send
    ));
    assert!(!policy.active(0));
    assert!(policy.active(u64::MAX));
    assert_eq!(policy.config.delay_ms, 200);
    assert_eq!(policy.config.max_pending_messages, 4096);
}

#[tokio::test]
async fn honest_certificate_repair_obeys_network_delay_and_keeps_signature_bytes() {
    use crate::primary::PrimaryMessage;
    use futures::{SinkExt, StreamExt};
    use tokio::net::TcpListener;
    use tokio::time::{timeout, Instant};
    use tokio_util::codec::{Framed, LengthDelimitedCodec};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut policy = policy("network");
    policy.name = policy.leaders[2];
    let header = signed_leader_header(&policy);
    let recipient = policy
        .leaders
        .iter()
        .copied()
        .find(|peer| {
            *peer != policy.name && policy.should_delay(header.round, &[header.author], *peer)
        })
        .unwrap();
    policy.peers.insert(address, recipient);
    policy.config.delay_ms = 150;
    let mut sender =
        SimpleSender::with_schedule(Arc::new(policy), DelayBudget::new(4, 1024 * 1024));
    let wire = Bytes::from(
        bincode::serialize(&PrimaryMessage::Certificate(crate::common::certificate(
            &header,
        )))
        .unwrap(),
    );
    let started = Instant::now();
    sender.send(address, wire.clone()).await;
    assert!(timeout(Duration::from_millis(40), listener.accept())
        .await
        .is_err());
    let (socket, _) = timeout(Duration::from_secs(2), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut stream = Framed::new(socket, LengthDelimitedCodec::new());
    let received = stream.next().await.unwrap().unwrap();
    assert_eq!(received.as_ref(), wire.as_ref());
    assert!(started.elapsed() >= Duration::from_millis(150));
    match bincode::deserialize::<PrimaryMessage>(&received).unwrap() {
        PrimaryMessage::Certificate(certificate) => certificate.verify(&committee()).unwrap(),
        _ => panic!("wrong message kind"),
    }
    stream.send(Bytes::from_static(b"Ack")).await.unwrap();
}
