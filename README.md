# Five-protocol EC2 experiment tools

This independent distribution branch contains reviewed experiment tools. It does not change the protocol branches.

- `stop_after_current.py`: For the released v2.1 controller, wait for the active case archive to complete, then request controller exit and wait for recorded resource cleanup. Run as the experiment user on Ubuntu. The following case can begin before the stop is observed; retry unfinished cases when resuming.
- `five-protocol-tools-v3.1.zip`: Serial Orca-A, Orca-B, Bullshark, Mysticeti and Mahi-Mahi experiments. Create exactly 10, 20 or 35 instances per pool, install once, run pending cases, archive and destroy. Includes instructions, the 480-case plan, verification metadata and offline tests.
- `SHA256SUMS`: File checksums.

Update Orca-A to `57a6ccc0b4a0ce21baa59081192bc8bb5f0208ba` and Orca-B to `a970a8d6bec6198658f23eef6090e6332b43b0eb` on their `adversary-experiments` branches. Bullshark `63fe54b`, Mysticeti `b813f80`, and Mahi-Mahi `db58361` already separate install from remote.

Stop and clean up the old run before replacing tools or updating controller repositories. To preserve compatible completed results, resume v3.1 in the same output directory with `--upgrade-pool-policy --retry-failed`.

No AWS credentials, private keys, settings files or experiment measurements are included.
