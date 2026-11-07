#!/usr/bin/env python3
"""Run the bounded language-metadata mutation test on 16 pinned E-core workers.

Build tests/debug_language_formats.rs first (optionally with LLVM coverage).
Invoke through tools/cgroup-run.py; no toolchains or profilers are installed.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import time


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def source_digest(repo):
    paths = sorted(subprocess.check_output(['rg', '--files', 'src', 'tests', 'tools'], cwd=repo, text=True).splitlines())
    h = hashlib.sha256()
    for name in paths + ['Cargo.toml', 'Cargo.lock']:
        h.update(name.encode() + b'\0' + (repo / name).read_bytes() + b'\0')
    return h.hexdigest()


def main(args):
    repo = Path(__file__).resolve().parents[1]
    output = Path(args.output).absolute()
    if output.exists() or not output.resolve().is_relative_to(repo / '.tmp'):
        raise ValueError('--output must be a fresh directory under project .tmp/')
    relative = next(line[3:] for line in Path('/proc/self/cgroup').read_text().splitlines() if line.startswith('0::'))
    group = Path('/sys/fs/cgroup') / relative.lstrip('/')
    assert (group / 'memory.max').read_text().strip() == '68719476736'
    assert (group / 'memory.swap.max').read_text().strip() == '0'
    assert set(range(16, 32)) <= os.sched_getaffinity(0)
    assert 1 <= args.seconds <= 600
    binary = Path(args.test_binary).resolve(strict=True)
    output.mkdir(parents=True)
    start = int(time.time() * 1000) + 3000
    metadata = {'seconds': args.seconds, 'cpus': list(range(16, 32)), 'start_ms': start, 'cgroup': relative,
                'memory_max': 68719476736, 'swap_max': 0, 'source_sha256': source_digest(repo),
                'binary_sha256': digest(binary), 'seed': '6c616e677761736d xor worker ordinal',
                'families': ['source_map', 'mutated_source_map', 'inline_source_map', 'dwarf_language',
                             'external_dwarf', 'symbol_map', 'go_pclntab', 'name_spaces', 'code_relocation', 'opaque_references']}
    (output / 'metadata.json').write_text(json.dumps(metadata, indent=2))
    workers = []
    try:
        for worker, cpu in enumerate(range(16, 32)):
            log = (output / f'worker-{worker}.log').open('w')
            env = dict(os.environ, UWGIT_DEBUG_WORKER=str(worker), UWGIT_DEBUG_SECONDS=str(args.seconds),
                       UWGIT_DEBUG_START_MS=str(start), UWGIT_DEBUG_OUTPUT=str(output),
                       LLVM_PROFILE_FILE=str(output / f'worker-{worker}-%p.profraw'))
            child = subprocess.Popen(['taskset', '-c', str(cpu), str(binary), '--ignored', '--exact',
                                      'timed_language_debug_fuzz', '--nocapture', '--test-threads=1'],
                                     cwd=repo, env=env, stdout=log, stderr=subprocess.STDOUT)
            workers.append((child, log))
        last = 0
        while any(child.poll() is None for child, _ in workers):
            failures = [(i, child.returncode) for i, (child, _) in enumerate(workers) if child.poll() not in (None, 0)]
            if failures:
                raise RuntimeError(f'worker failure: {failures}; see preserved worker logs and deterministic seeds')
            elapsed = max(0, int(time.time() * 1000) - start)
            if elapsed > (args.seconds + 30) * 1000:
                raise TimeoutError('worker deadline exceeded')
            if elapsed - last >= 30000:
                records = []
                for path in output.glob('progress-*.json'):
                    try:
                        records.append(json.loads(path.read_text()))
                    except json.JSONDecodeError:
                        pass
                print(json.dumps({'elapsed_ms': elapsed, 'cases': sum(r['cases'] for r in records),
                                  'active_workers': sum(c.poll() is None for c, _ in workers)}), flush=True)
                last = elapsed
            time.sleep(1)
        assert all(child.returncode == 0 for child, _ in workers)
        results = [json.loads((output / f'result-{i}.json').read_text()) for i in range(16)]
        assert all(r['status'] == 'PASS' and r['elapsed_ms'] >= args.seconds * 1000 and all(r['families']) for r in results)
        if args.seconds >= 10:
            assert all(r['cli_rounds'] >= 10 for r in results), 'each worker must exercise all CLI mutation families'
        assert source_digest(repo) == metadata['source_sha256'], 'source changed during the campaign'
        summary = {'status': 'PASS', 'seconds': args.seconds, 'workers': 16, 'cases': sum(r['cases'] for r in results),
                   'failures': 0, 'cli_rounds': sum(r['cli_rounds'] for r in results), 'families': [sum(r['families'][i] for r in results) for i in range(10)],
                   'source_sha256': metadata['source_sha256'], 'memory_peak': int((group / 'memory.peak').read_text()),
                   'memory_events': (group / 'memory.events').read_text()}
        (output / 'summary.json').write_text(json.dumps(summary, indent=2))
        print(json.dumps(summary), flush=True)
    finally:
        for child, log in workers:
            if child.poll() is None:
                child.terminate()
        for child, log in workers:
            try:
                child.wait(timeout=10)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()
            log.close()


if __name__ == '__main__':
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--test-binary', required=True)
    p.add_argument('--output', required=True)
    p.add_argument('--seconds', type=int, default=600)
    main(p.parse_args())
