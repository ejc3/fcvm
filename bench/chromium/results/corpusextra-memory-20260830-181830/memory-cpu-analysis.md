# Withdrawn memory and CPU campaign

This campaign is withdrawn. Do not quote or derive performance claims from its
records.

The two sides ran in separate time blocks, the workload changed with the
concurrency cell, and the run lacks the snapshot and schedule identity required
to attribute a difference. The frozen provenance is also incomplete: the
sibling `corpusextra-hostcdp-20260830-172413/harness/SHA256SUMS` file contains
absolute paths and a self-entry, and that sibling's `provenance.json` names
different `hostcdp.sh` bytes from its frozen copy.

`WITHDRAWN` is the disposition. The raw records, frozen harness, provenance,
and `memory/summary.json` remain unchanged so the failed campaign can be audited.
Correcting a statistic inside that sealed output would not repair the design or
make the campaign publishable. A replacement result requires a new run through
the current interleaved, identity-bound harness.
