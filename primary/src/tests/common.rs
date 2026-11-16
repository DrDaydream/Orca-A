// Copyright(C) Facebook, Inc. and its affiliates.
use crate::messages::{Certificate, GrbcPhase, GrbcVote, Header, Proposal};
use config::{Authority, Committee, PrimaryAddresses, WorkerAddresses};
use crypto::Hash as _;
use crypto::{generate_keypair, PublicKey, SecretKey, Signature};
use rand::rngs::StdRng;
use rand::SeedableRng as _;

impl PartialEq for GrbcVote {
    fn eq(&self, other: &Self) -> bool {
        self.digest() == other.digest()
    }
}

// Fixture
pub fn keys() -> Vec<(PublicKey, SecretKey)> {
    let mut rng = StdRng::from_seed([0; 32]);
    (0..4).map(|_| generate_keypair(&mut rng)).collect()
}

// Fixture
pub fn committee() -> Committee {
    Committee {
        authorities: keys()
            .iter()
            .enumerate()
            .map(|(i, (id, _))| {
                let primary = PrimaryAddresses {
                    primary_to_primary: format!("127.0.0.1:{}", 1_100 + i).parse().unwrap(),
                    worker_to_primary: format!("127.0.0.1:{}", 1_200 + i).parse().unwrap(),
                };
                let workers = vec![(
                    0,
                    WorkerAddresses {
                        primary_to_worker: format!("127.0.0.1:{}", 1_300 + i).parse().unwrap(),
                        transactions: format!("127.0.0.1:{}", 1_400 + i).parse().unwrap(),
                        worker_to_worker: format!("127.0.0.1:{}", 1_500 + i).parse().unwrap(),
                    },
                )]
                .iter()
                .cloned()
                .collect();
                (
                    *id,
                    Authority {
                        stake: 1,
                        primary,
                        workers,
                    },
                )
            })
            .collect(),
    }
}

// Fixture
pub fn header() -> Header {
    let (author, secret) = keys().pop().unwrap();
    let header = Header {
        author,
        round: 1,
        parents: Certificate::genesis(&committee())
            .iter()
            .map(|x| x.digest())
            .collect(),
        ..Header::default()
    };
    Header {
        id: header.digest(),
        signature: Signature::new(&header.digest(), &secret),
        ..header
    }
}

// Fixture
pub fn headers() -> Vec<Header> {
    keys()
        .into_iter()
        .map(|(author, secret)| {
            let header = Header {
                author,
                round: 1,
                parents: Certificate::genesis(&committee())
                    .iter()
                    .map(|x| x.digest())
                    .collect(),
                ..Header::default()
            };
            Header {
                id: header.digest(),
                signature: Signature::new(&header.digest(), &secret),
                ..header
            }
        })
        .collect()
}

// Fixture
pub fn votes(header: &Header, phase: GrbcPhase) -> Vec<GrbcVote> {
    keys()
        .into_iter()
        .map(|(author, secret)| {
            let vote = GrbcVote {
                id: header.id.clone(),
                round: header.round,
                origin: header.author,
                phase,
                author,
                signature: Signature::default(),
            };
            GrbcVote {
                signature: Signature::new(&vote.digest(), &secret),
                ..vote
            }
        })
        .collect()
}

pub fn proposal() -> Proposal {
    Proposal {
        header: header(),
        batches: Default::default(),
    }
}
