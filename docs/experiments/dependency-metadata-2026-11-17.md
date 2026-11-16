# Compact dependency metadata experiment

Status: experimental; **not selected as the latency-improvement baseline**.
The recommended `orca-a-grbc-echo-ready` branch retains the GC-boundary fix
`008ad5d2a8774a2d057474774d38697bf845aa02`. This experiment is kept separately
on `orca-a-dependency-metadata-experiment`.

## Change and correctness boundary

`Synchronizer::get_parents` returns immutable digest/round/author metadata.
Cache and genesis hits reuse their established lookup digest instead of
cloning full certificate headers. Core's three edge-class passes consume
those fields, avoiding repeated certificate-digest calculations. Persistent
fallback still decodes once and checks the decoded certificate's digest
against its lookup key once. No additional persistent cache is introduced.

Strong/weak completion gates, distinct strong-parent authors, quorum stake,
edge-round restrictions, virtual metadata resolution, missing-dependency
repair, wire/storage formats and experiment parameters remain unchanged.
The cache-key invariant depends on inserting the same immutable certificate
under its computed digest; future mutable cache entries must preserve it.

The release workspace suite with `benchmark` enabled passes 143 tests, with
one manual benchmark ignored. A new stored-key mismatch regression covers
both completion records and virtual header markers. Temporarily removing
the shared guard makes the regression fail; the guard is restored. The
release node build and independent read-only code review also pass. Real
12-second process tests at GC depth 50 pass with one absent node, one node
joining after three seconds, and ten active nodes. Their common ordered
prefixes contain 571, 676 and 1,653 headers respectively; each has zero
shutdown round lag, no duplicate output and ongoing commit progress.

## Local dependency-processing measurement

| References | Before, microseconds | Candidate, microseconds |
| --- | ---: | ---: |
| 4 | 3.16 | 0.42 |
| 10 | 7.79 | 1.01 |
| 20 | 18.54 | 2.05 |
| 30 | 28.77 | 3.02 |

These timings include lookup, three edge-class passes and readiness checking.
The synthetic all-strong fixtures repeat a proposer and omit Core's
distinct-author HashSet check. They measure local clone/hash work, not full
Core execution or consensus/network latency.

## Real-process comparison

Windows x86_64 loopback, Rust 1.85.0, four nodes, one worker each, 50,000 tx/s,
512-byte transactions, header size 20, GC depth 50, 500,000-byte batches,
200 ms header/batch delays, default Tokio threads and serial process startup.
Every run used fresh databases, the same keys and the same client executable.
The baseline binary is the already-corrected GC revision above, not the
earlier version that stalled. Execution order was baseline A, candidate A,
candidate B, baseline B. Each row uses the fixed 20-second final-order window
beginning at the first client's Start event.

| Run | Final TPS | Final consensus mean, ms | Final E2E mean, ms | Final E2E p95, ms | Completed/sent | Mean busy CPU cores |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Baseline A | 47,140.25 | 260.77 | 300.29 | 577.00 | 1,508/1,600 | 4.438 |
| Candidate A | 48,605.75 | 294.21 | 332.73 | 646.00 | 1,553/1,600 | 3.959 |
| Candidate B | 47,935.50 | 285.68 | 330.44 | 669.00 | 1,535/1,600 | 4.123 |
| Baseline B | 47,482.20 | 255.01 | 297.97 | 556.00 | 1,520/1,600 | 4.800 |

All four runs have zero client rate warnings, identical shared commit order
across replicas and commits within 71 ms of shutdown. The two-run arithmetic
means show 2.0% higher TPS and 12.5% lower busy CPU, but 12.4% higher final
consensus mean, 10.8% higher final E2E mean and 16.1% higher run p95. The mean
of the two run p95 values is not a pooled percentile. With two repetitions
and shared-host noise, these are observations, not statistical guarantees.

A subsequent 60-second candidate run sustains 48,390.6 fixed-window TPS and
completes 4,648/4,799 samples. Final consensus/E2E means are 322.0/405.3 ms
and E2E p95 is 885.0 ms. It has ten client rate warnings. An earlier baseline
60-second run with the same keys/client reports 47,389.1 TPS, 292.5/341.0 ms
means, 701.0 ms p95 and no rate warnings. This is a nonadjacent diagnostic
comparison, not an additional controlled pair; it does not demonstrate a
latency benefit.

The recommended branch therefore keeps the GC correctness repair without this
candidate. Reduced local work is established; a lower consensus or end-to-end
latency is not. No Ubuntu or WAN performance conclusion follows from these
Windows measurements. All runs retain the original protocol thresholds and
default thread settings.
