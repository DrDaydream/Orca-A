// Copyright(C) Facebook, Inc. and its affiliates.
use crate::error::{DagError, DagResult};
use crate::messages::{Certificate, Grade, GradeVote, GradedCertificate, Header, Vote};
use config::{Committee, Stake};
use crypto::Hash as _;
use crypto::{Digest, PublicKey, Signature};
use std::collections::HashSet;

#[cfg(test)]
#[path = "tests/grbc_tests.rs"]
pub mod grbc_tests;

/// Aggregates votes for a particular header into a certificate.
pub struct VotesAggregator {
    weight: Stake,
    votes: Vec<(PublicKey, Signature)>,
    used: HashSet<PublicKey>,
}

/// Aggregates signed grade-1 deliveries into a grade-2 proof.
pub struct GradeVotesAggregator {
    weight: Stake,
    votes: Vec<(PublicKey, Signature)>,
    used: HashSet<PublicKey>,
}

impl GradeVotesAggregator {
    pub fn new() -> Self {
        Self {
            weight: 0,
            votes: Vec::new(),
            used: HashSet::new(),
        }
    }

    pub fn append(
        &mut self,
        vote: GradeVote,
        committee: &Committee,
        certificate: &Certificate,
    ) -> DagResult<Option<GradedCertificate>> {
        ensure!(
            vote.id == certificate.digest()
                && vote.round == certificate.round()
                && vote.origin == certificate.origin(),
            DagError::UnexpectedGradeVote(vote.id)
        );
        ensure!(
            self.used.insert(vote.author),
            DagError::AuthorityReuse(vote.author)
        );
        self.weight += committee.stake(&vote.author);
        self.votes.push((vote.author, vote.signature));
        if self.weight >= committee.quorum_threshold() {
            self.weight = 0;
            return Ok(Some(GradedCertificate {
                certificate: certificate.clone(),
                grade: Grade::Two,
                votes: self.votes.clone(),
            }));
        }
        Ok(None)
    }
}

impl VotesAggregator {
    pub fn new() -> Self {
        Self {
            weight: 0,
            votes: Vec::new(),
            used: HashSet::new(),
        }
    }

    pub fn append(
        &mut self,
        vote: Vote,
        committee: &Committee,
        header: &Header,
    ) -> DagResult<Option<Certificate>> {
        let author = vote.author;

        ensure!(
            vote.id == header.id && vote.round == header.round && vote.origin == header.author,
            DagError::UnexpectedVote(vote.id)
        );

        // Ensure it is the first time this authority votes.
        ensure!(self.used.insert(author), DagError::AuthorityReuse(author));

        self.votes.push((author, vote.signature));
        self.weight += committee.stake(&author);
        if self.weight >= committee.quorum_threshold() {
            self.weight = 0; // Ensures quorum is only reached once.
            return Ok(Some(Certificate {
                header: header.clone(),
                votes: self.votes.clone(),
            }));
        }
        Ok(None)
    }
}

/// Aggregate certificates and check if we reach a quorum.
pub struct CertificatesAggregator {
    weight: Stake,
    certificates: Vec<Digest>,
    used: HashSet<PublicKey>,
}

impl CertificatesAggregator {
    pub fn new() -> Self {
        Self {
            weight: 0,
            certificates: Vec::new(),
            used: HashSet::new(),
        }
    }

    pub fn append(
        &mut self,
        certificate: Certificate,
        committee: &Committee,
    ) -> DagResult<Option<Vec<Digest>>> {
        let origin = certificate.origin();

        // Ensure it is the first time this authority votes.
        if !self.used.insert(origin) {
            return Ok(None);
        }

        self.certificates.push(certificate.digest());
        self.weight += committee.stake(&origin);
        if self.weight >= committee.quorum_threshold() {
            self.weight = 0; // Ensures quorum is only reached once.
            return Ok(Some(self.certificates.drain(..).collect()));
        }
        Ok(None)
    }
}
