# Reuse complete proposal repair payloads

Status: experimental; not promoted as a demonstrated latency improvement.
Recommended operational branch: orca-a-grbc-echo-ready at
bf69ba4305ae4752d223cf8203f40f4c261c47c5.

Follow-up correctness finding: a new late-join smoke stalls at committed
round 52 in BOTH this candidate and the exact 008ad5d baseline using the
same keys. The DAG keeps advancing. The recommended branch now preserves
already-observed, never-output history across GC and filters output by exact
delivered-digest membership. Two new regressions, a same-key late-join smoke
and a sustained-load check validate that follow-up within its documented
scope. Never-output metadata and indexes can remain indefinitely, so this
does not establish bounded memory or general asynchronous liveness.

This experiment intentionally retains the original comparison base and does
not contain that follow-up. Its measured implementation is bab47db; any
future promotion must first reapply and evaluate it on the current corrected
base. Do not use this historical experiment as the operational checkout.

## Observed repeated work

A ten-node, 12-second Windows loopback diagnostic at 50,000 tx/s recorded
3,896 full proposal replies. Of these, 3,135 immediately followed a reply
for the same digest (80.5%); 3,131 also changed recipient. Each reply
previously reread the proposal header and every batch, then serialized the
same immutable content again. The diagnostic had 233 client rate warnings,
so it identifies repeated work rather than establishing comparative speed.

The candidate retains only the last successfully serialized complete
proposal while requests remain queued. Exact digest matches clone the Bytes
handle and use the current requestor's address. The Helper drops its entry
before waiting on an empty channel and before loading a different digest,
including a missing/error result. Network queues can still retain already
sent Bytes references, as before. Saved votes are reread on every request;
a later READY is never hidden by a cached proposal. Vote-only requests still
read/send votes without touching the payload path.

This reuses the immutability already enforced by verified, atomic
write_batch_if_absent storage. It adds no persistent cache, thread, timer,
protocol message or quorum change. recv().await and existing bounded-channel
backpressure remain in place. Temporary per-repair tracing was removed from
the final source and performance executable.

A separate candidate diagnostic observed 907 reconstructions and 3,203 cache
hits (77.9% reuse). It had 257 client rate warnings and is not performance
evidence.

## Correctness checks

The regression first failed against the original code with two header reads
where one suffices. With the candidate it checks both complete proposal wire
responses, newly saved READY votes on the second request, single reads of the
header/batches while requests are queued, and renewed reads after idle.
A second test sends A, missing, B, A and checks the exact three complete,
valid outputs without substituting a cached payload for the missing digest.
The pre-existing recovery-before-local-READY test still passes.

Release workspace tests with benchmark enabled: 144 passed, 0 failed,
one manual dependency benchmark ignored. The release node build and
independent read-only review pass. The review's optional coverage notes:
missing-result eviction and alternate-recipient hits are also supported by
code inspection, but are not isolated by their own counted branch tests.

## Performance measurements

All following short runs use four nodes, one worker/node, 50,000 tx/s,
512-byte transactions, header size 20, GC depth 50, batch size 500,000,
header/batch delays 200 ms, default Tokio threads and serial startup.
The client binary and signing keys are identical; every run has fresh
databases. Tracing is absent. The baseline is the GC-corrected 008ad5d.
Order: baseline A, candidate A, candidate B, baseline B, candidate C, baseline C.
Each row uses the fixed 20-second final ordered comparison window.

| Run | Final TPS | Final consensus mean ms | Final E2E mean ms | Final E2E p95 ms | Completed/sent | Rate warnings |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Baseline A | 47,904.25 | 291.29 | 335.08 | 668.00 | 1,529/1,600 | 0 |
| Candidate A | 47,433.35 | 276.51 | 315.76 | 510.00 | 1,516/1,600 | 0 |
| Candidate B | 43,281.10 | 273.36 | 317.93 | 653.00 | 1,383/1,464 | 20 |
| Baseline B | 49,417.90 | 176.96 | 216.82 | 390.00 | 1,581/1,599 | 0 |
| Candidate C | 46,470.00 | 260.64 | 300.99 | 575.00 | 1,499/1,600 | 0 |
| Baseline C | 47,286.80 | 280.04 | 320.68 | 608.00 | 1,513/1,600 | 0 |

In the two pairs without rate warnings (A and C), candidate final E2E mean
is 5.8% and 6.1% lower, while TPS is 1.0% and 1.7% lower. Those results do
not establish unchanged throughput. Pair B is retained in full: its
candidate misses the configured input rate and has no commit in its final
2.61 seconds. Its shared ordered prefix still agrees across replicas, but
that alone must not be called continuous near-shutdown progress.

A subsequent 60-second baseline run itself produced 753 rate warnings,
48,430.9 fixed-window TPS, 998.8 ms final consensus mean, 1,885.9 ms E2E mean
and 5,251.0 ms E2E p95. Its minimum host available memory was 1.80 GiB.
The cause of this host/load variation was not isolated. No candidate result
is compared against that degraded baseline as a claimed improvement.

The candidate reduces proven duplicate work with a small implementation,
but the requested combined lower-latency/maintained-throughput outcome has
not been demonstrated. Keep it on its separate experiment branch. These
Windows loopback results provide no Ubuntu or WAN performance conclusion.
