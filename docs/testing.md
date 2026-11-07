# Testing wasm-git

The maintained verification methodology is available in
[English](../documents/DEVELOPMENT.eu_US.md) and
[简体中文](../documents/DEVELOPMENT.zh_CN.md).

Run builds, tests, and profiling within the configured memory cgroup. Keep all
outputs, downloaded tools/corpora, temporary repositories, and logs under the
project-local ignored `.tmp/` directory. Set `CARGO_TARGET_DIR`, `TMPDIR`, and the
Python cache location before launching tools. Do not commit generated reports.

The guide covers the client/server build matrix, ordinary regression, independent
byte replay, compiler-based intent oracles, arbitrary-input fuzzing, valid-module
random campaigns, monitoring, and existing-installation VTune analysis. It also
explains how to record ignored tests, interruptions, coverage limitations, and
resource events without overstating the result.

Durable test programs and minimal regression inputs remain in `tests/`, `fuzz/`,
and `tools/`. Historical run-specific transcripts are not the current product
contract and are excluded from the published source history where identified.
