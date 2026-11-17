# Weak-parent selection and retained history

Base: `bf69ba4305ae4752d223cf8203f40f4c261c47c5`. Candidate branch:
`orca-a-weak-history-selection`. This is a separate correctness/performance
candidate; it does not include the earlier dependency, repair or validated
advance performance experiments.

## Problem and resulting behavior

The supplied Orca draft's Algorithm 1 lines 43–44 and Lemma 14 require a public,
deterministic weak-parent order and exclusion of vertices already reachable by
strong/weak paths. Previously Core selected the first entries of a HashMap,
removed candidates when it sent a parent notification, and removed old
candidates during GC. A notification can be superseded before Proposer uses
it. Consequently an old vertex could lose its inclusion opportunity without
ever being output, while an already covered ancestor could consume a weak slot.

Core now maintains one ordered set of `(round, author, certificate digest)`.
Every local FINISH registers a candidate. Only validated actual consensus
output retires its exact tuple; notifications and GC retain candidates. At the
existing parent-quorum snapshot, selection chooses at most `f` older candidates
absent from the strong parents' transitive strong/weak history. Virtual evidence
does not contribute to that history.

The selector borrows completed metadata and falls back to existing completion
records after GC. It deduplicates each traversal and stops at the oldest
eligible round or an already output frontier. The latter reuses an invariant:
every non-output completion registers a candidate before selection, and only
output removes one. A COMPLETE record absent from the full candidate set is
therefore already output, including its causal ancestors. READY without local
completion cannot support this inference. This coupling is documented at the
FINISH insertion and selector; any future completion or restart-recovery path
must preserve it. There is no second output ledger or transitive-closure cache.

The existing certificate aggregator now lends its parent slice and consumes it
only after selection succeeds. Missing or mismatched history retains the same
quorum in the existing retry mechanism. Duplicate completions do not produce a
second notification. Aggregation no longer clones the entire certificate.

## Verification

Eight new functional regressions cover causal coverage, superseded notifications,
GC retention and exact retirement, deterministic arrival-independent order,
virtual exclusion after metadata GC, failed selection/retry, output-frontier
pruning, and READY-only rejection. The existing weak-edge budget test now uses
real stored parent completion records. The three original failure cases,
quorum-loss case, frontier case and READY-only case were observed failing before
their respective fixes.

The final release workspace suite passes 152 tests with no failures and two
manual benchmarks ignored. The newly added manual backlog benchmark was also
run explicitly and passes. Release node compilation and rustfmt checks pass;
independent read-only review found no blocking issue.

Final executable SHA256:
`4395667e1699e9db655871897e2663cfb3dbb868fbacb0cd3c9de8276e271f71`.
Same-key, fresh-store, default-thread real-process smoke results:

| Scenario | Active nodes | Common ordered-header prefix | Final round lag |
| --- | ---: | ---: | ---: |
| One joins after 3 seconds | 4 | 702 | 0 |
| One absent throughout | 3 | 562 | 0 |
| All present, ten-node committee | 10 | 1,731 | 0 |

These 12-second protocol smokes use the smoke runner's lightweight payload and
32-byte header settings. The separate load comparison uses 20-byte headers and
the user's 50,000 tx/s configuration.

An actual Core backlog fixture repeatedly supersedes weak notifications for
one round-1 orphan, while the other three authorities extend/output their own
history and GC uses depth 5. Before frontier pruning, the last 200 of 400 rounds
took 900.276 ms; after pruning they took 247.047 ms. The candidate remains
retained throughout. This local fixture measures processing cost, not network
consensus latency or throughput. Both measurements and all initial smoke
results are retained separately from final-build verification.

## Scope and limits

Parent selection still occurs at Core's existing quorum snapshot, before the
proposer constructs its actual header. The change does not prove complete
Algorithm 1 snapshot equivalence, BAB safety/liveness, or eventual coverage
under arbitrary Byzantine schedules. Deterministic leader selection, missing
threshold GPC, ancient unseen ingress and lack of restart recovery retain the
existing experimental limitations.

Unoutput candidates and associated consensus history can remain indefinitely.
If all output stalls while rounds keep advancing, the unoutput causal suffix
may grow and repeated traversals can still have quadratic cumulative cost.
Frontier pruning removes repeated traversal through already output history;
it does not establish universally bounded memory, work or latency.

## Fixed-window load comparison

Windows loopback, default Tokio threads, serial launch, 20-second windows,
50,000 tx/s, 512-byte transactions, header=20, GC=50, batch=500,000 bytes,
header/batch delay=200 ms. Four nodes use baseline–candidate–candidate–baseline,
identical keys/client/parameters and fresh ports/stores. A 30-second quiet
interval precedes each trial; one-second process samples record foreign
benchmark overlap. Completed old directories are losslessly compressed only
between trials; new measurement directories are uncompressed.

Each row is the mean of two runs; rate warnings are retained individually.
The initial ten-node baseline exhausted disk space and was stopped; its
storage errors and partial logs are retained, excluded from these means.
After lossless compression of completed task data/build caches, the
ten-node baseline was restarted in a fresh directory with a 3.75 GiB start
minimum and 1.5 GiB runtime disk floor. It subsequently hit the existing
1.5 GiB available-memory floor (last resource sample at 17.636 seconds).
Both partial attempts are retained. There is no completed ten-node load
comparison; the successful ten-node protocol smoke is not a substitute.
Measured executable/client files and new run directories remain uncompressed.

| Nodes | Version | Final TPS | Final consensus ms | Final E2E ms | Rate warnings |
| --- | --- | ---: | ---: | ---: | --- |
| 4 | baseline | 49,485.05 | 261.98 | 300.21 | 0/0 |
| 4 | candidate | 49,230.03 | 285.77 | 325.65 | 0/0 |

Keep this candidate independent of the recommended `orca-a-grbc-echo-ready`
at bf69ba4. Correctness and local processing evidence do not prove the combined
throughput/latency objective in the target environment. All trials, unfinished
sample counts, background-overlap observations and resource measurements are
preserved in the companion evidence. No benchmark parameters, default threads,
protocol thresholds or serial launch procedure were changed.
