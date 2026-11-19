# Five-protocol EC2 experiment tools

Latest: **v3.5** changes Orca-A, Orca-B and Bullshark A1 to whole-run authority crashes: selected primary, worker and client processes are absent. Orca-B delays ABA only in A5; A3 uses ordinary ABA delivery. The tool archives active/crashed IDs and requested/configured offered rates, and plots the load of the clients actually started.

Use [the v3.5 EC2 update and batch-run commands](EC2_v3.5_更新与批量运行.md). `update_ec2_v35.py` updates the three existing controller directories and figure-tools, preserves local parameters that merge cleanly, and checks all local merges before replacing files. An overlap that cannot be merged stops before changing the checkouts; no conflict markers are written. The reviewed protocol revisions are recorded in [release-v3.5.json](release-v3.5.json).

- Orca-A: 7a93ba7e433728653fe9eee3fb714c62911ab53a on adversary-experiments.
- Orca-B: d47cedd565376be499bf2f6a5e9b24bbcec290d1 on adversary-experiments.
- Bullshark: 44d59d324e8cb097b70f11a470376f1d4d7c1149 on adversary-experiments.
- Mysticeti and Mahi-Mahi retain their previous versions and 20-50 second attack windows.

Each case runs once for 60 seconds. Orca-A/B/Bullshark use a 20-byte header_size threshold. All five protocols run serially. Each protocol uses separate 10/20/35-instance pools, following create -> SSH readiness -> install -> repeated remote -> destroy. Remote never builds or installs.

Start a new output directory: `/home/ubuntu/experiments/paper-results-v3.5-60s-h20-crash-a1`. Old A1 and Orca-B A3 results describe different behavior and must not be relabeled as v3.5. Completed v3.5 cases resume with `--retry-failed` after a failure is investigated.

For A1, rate still divides over the complete committee before crashed clients are omitted. Ten authorities with three crashes and rate=10000 therefore configure seven clients at 1000 tx/s each. The x-axis is configured offered load, not measured ingress. Bullshark A1 disables alternating leader faults; A3/A5 retain them.

`five-protocol-tools-v3.5.zip` includes the plan, runners, archive/plot tools, guide and portable tests. `SHA256SUMS` covers the distribution files. Earlier v3.1-v3.4 ZIPs and guides remain unchanged as historical releases. The old `stop_after_current.py` is retained for its documented older controller workflow.

No AWS credentials, private keys, settings files, conversation transcripts or experiment measurements are included. Validation scope and environment limits are recorded inside the toolkit's verification.json.
