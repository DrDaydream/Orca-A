use super::WorkerPrimaryMessage;
use config::WorkerId;
use crypto::Digest;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

#[derive(Debug, Serialize, Deserialize)]
enum LegacyWorkerPrimaryMessage {
    OurBatch(Vec<u8>, WorkerId),
    OthersBatch(Digest, WorkerId),
}

#[test]
fn worker_batch_codec_reads_and_writes_payload_in_bulk() {
    struct Reader<'a> {
        remaining: &'a [u8],
        calls: usize,
    }
    impl Read for Reader<'_> {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            self.calls += 1;
            self.remaining.read(bytes)
        }
    }
    #[derive(Default)]
    struct Writer {
        bytes: Vec<u8>,
        calls: usize,
    }
    impl Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.calls += 1;
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let payload: Vec<_> = (0..500_000).map(|i| (i % 251) as u8).collect();
    let message = WorkerPrimaryMessage::OurBatch(payload.clone(), 37);
    let mut writer = Writer::default();
    bincode::serialize_into(&mut writer, &message).unwrap();
    let mut reader = Reader {
        remaining: &writer.bytes,
        calls: 0,
    };
    let decoded: WorkerPrimaryMessage = bincode::deserialize_from(&mut reader).unwrap();
    match decoded {
        WorkerPrimaryMessage::OurBatch(bytes, worker) => {
            assert_eq!(bytes, payload);
            assert_eq!(worker, 37);
        }
        _ => panic!("Batch message changed variant"),
    }
    assert!(reader.remaining.is_empty());
    eprintln!(
        "worker_batch_bulk_io reads={} writes={}",
        reader.calls, writer.calls
    );
    assert!(
        reader.calls < 16 && writer.calls < 16,
        "Batch payload should use bulk IO; reads={}, writes={}",
        reader.calls,
        writer.calls
    );
}

#[test]
fn worker_batch_codec_preserves_legacy_wire_and_bounds() {
    for size in [0, 1, 31, 500_000, crate::MAX_PROPOSAL_BYTES] {
        let payload: Vec<_> = (0..size).map(|i| (i % 251) as u8).collect();
        let old = LegacyWorkerPrimaryMessage::OurBatch(payload.clone(), 37);
        let new = WorkerPrimaryMessage::OurBatch(payload.clone(), 37);
        let old_wire = bincode::serialize(&old).unwrap();
        let new_wire = bincode::serialize(&new).unwrap();
        assert_eq!(new_wire, old_wire);
        assert_eq!(
            bincode::serialized_size(&new).unwrap(),
            old_wire.len() as u64
        );
        match bincode::deserialize::<WorkerPrimaryMessage>(&old_wire).unwrap() {
            WorkerPrimaryMessage::OurBatch(bytes, worker) => {
                assert_eq!(bytes, payload);
                assert_eq!(worker, 37);
            }
            _ => panic!("Legacy batch changed variant"),
        }
        match bincode::deserialize::<LegacyWorkerPrimaryMessage>(&new_wire).unwrap() {
            LegacyWorkerPrimaryMessage::OurBatch(bytes, worker) => {
                assert_eq!(bytes, payload);
                assert_eq!(worker, 37);
            }
            _ => panic!("New batch changed variant"),
        }
        assert!(
            bincode::deserialize::<WorkerPrimaryMessage>(&new_wire[..new_wire.len() - 1]).is_err()
        );
        if size > 0 {
            // Four-byte enum tag and eight-byte declared length, but only part
            // of the normally sized payload remains in the input slice.
            assert!(
                bincode::deserialize::<WorkerPrimaryMessage>(&new_wire[..12 + size / 2]).is_err()
            );
        }
    }
    let digest = Digest([7; 32]);
    let wire = bincode::serialize(&WorkerPrimaryMessage::OthersBatch(digest.clone(), 37)).unwrap();
    assert_eq!(
        wire,
        bincode::serialize(&LegacyWorkerPrimaryMessage::OthersBatch(digest.clone(), 37)).unwrap()
    );
    match bincode::deserialize::<WorkerPrimaryMessage>(&wire).unwrap() {
        WorkerPrimaryMessage::OthersBatch(bytes, worker) => {
            assert_eq!(bytes, digest);
            assert_eq!(worker, 37);
        }
        _ => panic!("Digest-only message changed variant"),
    }
    // Actual receivers decode slices: an impossible length must fail before
    // allocating its claimed body. The enum discriminant and length are fixed LE.
    let mut malformed = 0u32.to_le_bytes().to_vec();
    malformed.extend_from_slice(&u64::MAX.to_le_bytes());
    assert!(bincode::deserialize::<WorkerPrimaryMessage>(&malformed).is_err());
}

#[test]
#[ignore = "manual worker batch codec measurement"]
fn worker_batch_codec_benchmark() {
    use std::hint::black_box;
    use std::time::Instant;
    let payload: Vec<_> = (0..500_000).map(|i| (i % 251) as u8).collect();
    let old = LegacyWorkerPrimaryMessage::OurBatch(payload.clone(), 37);
    let new = WorkerPrimaryMessage::OurBatch(payload, 37);
    let wire = bincode::serialize(&old).unwrap();
    assert_eq!(wire, bincode::serialize(&new).unwrap());
    for (pass, legacy) in [true, false, false, true].iter().enumerate() {
        let start = Instant::now();
        for _ in 0..1000 {
            if *legacy {
                black_box(bincode::serialize(black_box(&old)).unwrap());
            } else {
                black_box(bincode::serialize(black_box(&new)).unwrap());
            }
        }
        let encode_ms = start.elapsed().as_secs_f64() * 1000.0;
        let start = Instant::now();
        for _ in 0..1000 {
            if *legacy {
                black_box(
                    bincode::deserialize::<LegacyWorkerPrimaryMessage>(black_box(&wire)).unwrap(),
                );
            } else {
                black_box(bincode::deserialize::<WorkerPrimaryMessage>(black_box(&wire)).unwrap());
            }
        }
        eprintln!("worker_batch_codec pass={} legacy={} iterations=1000 payload_bytes=500000 encode_ms={:.3} decode_ms={:.3}",
            pass, legacy, encode_ms, start.elapsed().as_secs_f64()*1000.0);
    }
}
