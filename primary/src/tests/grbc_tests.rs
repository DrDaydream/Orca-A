use super::GradeVotesAggregator;
use crate::common::{certificate, committee, header, keys};
use crate::messages::{Grade, GradeVote};
use crypto::{Hash as _, Signature, SignatureService};

#[tokio::test]
async fn grade_two_requires_a_quorum() {
    let committee = committee();
    let certificate = certificate(&header());
    let mut aggregator = GradeVotesAggregator::new();
    let mut proof = None;

    for (name, secret) in keys() {
        let mut signature_service = SignatureService::new(secret);
        let vote = GradeVote::new(&certificate, &name, &mut signature_service).await;
        proof = aggregator.append(vote, &committee, &certificate).unwrap();
        if proof.is_some() {
            break;
        }
    }

    let proof = proof.expect("a quorum must produce a grade-2 proof");
    assert_eq!(proof.grade, Grade::Two);
    assert_eq!(proof.certificate.digest(), certificate.digest());
    proof.verify(&committee).unwrap();
}

#[tokio::test]
async fn duplicate_grade_votes_are_rejected() {
    let committee = committee();
    let certificate = certificate(&header());
    let (name, secret) = keys().pop().unwrap();
    let mut signature_service = SignatureService::new(secret);
    let vote = GradeVote::new(&certificate, &name, &mut signature_service).await;
    let mut aggregator = GradeVotesAggregator::new();

    assert!(aggregator
        .append(vote.clone(), &committee, &certificate)
        .is_ok());
    assert!(aggregator.append(vote, &committee, &certificate).is_err());
}

#[tokio::test]
async fn grade_vote_is_bound_to_one_certificate() {
    let committee = committee();
    let certificate = certificate(&header());
    let (name, secret) = keys().pop().unwrap();
    let mut signature_service = SignatureService::new(secret);
    let mut vote = GradeVote::new(&certificate, &name, &mut signature_service).await;
    vote.id = Default::default();
    vote.signature = Signature::default();
    let mut aggregator = GradeVotesAggregator::new();

    assert!(aggregator.append(vote, &committee, &certificate).is_err());
}
