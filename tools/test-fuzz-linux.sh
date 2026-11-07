#!/usr/bin/env bash
# All compilation and coverage-guided execution stay in the existing Linux cgroup.
set -euo pipefail
repo_dir=$(cd "$(dirname "$0")/.." && pwd)
remote_host=${WASM_GIT_SSH_HOST:-linux}
container=${WASM_GIT_CONTAINER:-uwvm-debug-core3-v48-20261011}
remote_root=${WASM_GIT_REMOTE_ROOT:-/tmp/wasm-git-20261019}
runs=${WASM_GIT_LIBFUZZER_RUNS:-20000}
[[ $runs =~ ^[1-9][0-9]*$ && ${#runs} -le 7 && $runs -le 1000000 ]] || { echo 'invalid fuzz run count' >&2; exit 2; }
[[ $container =~ ^[A-Za-z0-9_.-]+$ ]] || { echo 'invalid container name' >&2; exit 2; }
[[ $remote_root =~ ^/tmp/[A-Za-z0-9_./-]+$ && $remote_root != *..* ]] || { echo 'invalid /tmp root' >&2; exit 2; }
[[ -f $repo_dir/fuzz/Cargo.lock ]] || { echo 'fuzz/Cargo.lock must be generated in SSH Linux first' >&2; exit 2; }
staging=$(mktemp -d /tmp/wasm-git-libfuzzer.XXXXXXXX)
trap 'rm -rf "$staging"' EXIT
COPYFILE_DISABLE=1 tar --no-xattrs --exclude='._*' --exclude='fuzz/target' --exclude='fuzz/corpus' \
    --exclude='fuzz/artifacts' --exclude='fuzz/coverage' -cf "$staging/source.tar" \
    -C "$repo_dir" Cargo.toml Cargo.lock src tests tools vendor fuzz
incoming="/tmp/wasm-git-libfuzzer-$(id -u)-$$.tar"
scp -q "$staging/source.tar" "$remote_host:$incoming"
ssh "$remote_host" "docker exec -i '$container' chroot /host /bin/bash -s -- '$remote_root' '$incoming' '$runs'" \
    < "$repo_dir/tools/test-fuzz-linux-remote.sh"
