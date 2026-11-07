#!/usr/bin/env python3
"""Opt-in real compiler checks for ObjC/ObjC++, MoonBit, AssemblyScript and Go.

Run with tools/cgroup-run.py. Dependencies must already be installed. All
samples, compiler caches and receipts stay in a new project-local .tmp folder.
The companion test-debug-symbols.py covers C, C++, Rust and TinyGo DWARF.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import struct
import time

spec = importlib.util.spec_from_file_location('debug_oracle', Path(__file__).with_name('test-debug-symbols.py'))
helpers = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helpers)
run = helpers.run


def mapping(args, wasm, sidecar=None):
    return json.loads(run([args.uwgit, 'debug-map', wasm, '--json', *(['--debug-file', sidecar] if sidecar else [])]))


def compare(args, old, new, old_map=None, new_map=None):
    command = ['diff', old, new, '--format', 'json', '--compact']
    if old_map:
        command += ['--old-debug-file', old_map, '--new-debug-file', new_map]
    start = time.monotonic()
    raw = run([args.uwgit, '--threads', '1', *command])
    assert raw == run([args.uwgit, '--threads', '0', *command]), 'parallel output differs'
    result = json.loads(raw)
    helpers.replay(old.read_bytes(), new.read_bytes(), result['audit']['edits'])
    assert result['function_analysis']['available'], result['function_analysis']
    return result, time.monotonic() - start


def annotations(report):
    return [a[side] for change in report['changes'] for a in change.get('function', {}).get('source_annotations', [])
            for side in ['old', 'new'] if a.get(side)]


def go_memory(wasm):
    data = wasm.read_bytes()
    leb = lambda p: helpers.leb(data, p)
    p = 8
    segments = []
    while p < len(data):
        kind = data[p]
        size, start = leb(p + 1)
        p = start + size
        if kind != 11:
            continue
        count, q = leb(start)
        for _ in range(count):
            flags, q = leb(q)
            assert flags == 0 and data[q] == 0x41
            address, q = leb(q + 1)
            assert data[q] == 0x0b
            size, q = leb(q + 1)
            segments.append((address, data[q:q + size]))
            q += size
    base = min(a for a, _ in segments)
    end = max(a + len(b) for a, b in segments)
    assert end - base < 128 * 1024**2
    memory = bytearray(end - base + 64)
    for a, b in segments:
        memory[a - base:a - base + len(b)] = b
    return base, memory


GO_ORACLE = r'''package main
import("debug/gosym";"encoding/json";"fmt";"os")
type Row struct{ PC [2]uint64 `json:"virtual_pc"`; Location struct { File string; Line int } }
type View struct{ Debug struct{ Rows []Row `json:"go_lines"` } `json:"debug_info"` }
func main(){
 data,e:=os.ReadFile(os.Args[1]);if e!=nil{panic(e)}
 raw,e:=os.ReadFile(os.Args[2]);if e!=nil{panic(e)}
 var view View;if e=json.Unmarshal(raw,&view);e!=nil{panic(e)}
 table,e:=gosym.NewTable(nil,gosym.NewLineTable(data,0));if e!=nil{panic(e)}
 for _,row:=range view.Debug.Rows {for _,pc:=range []uint64{row.PC[0],row.PC[1]-1}{
  file,line,fn:=table.PCToLine(pc)
  if file!=row.Location.File || line!=row.Location.Line || fn==nil {panic(fmt.Sprintf("pc=%x got=%s:%d want=%s:%d",pc,file,line,row.Location.File,row.Location.Line))}
 }}
 fmt.Println(len(view.Debug.Rows))
}
'''


def main(args):
    group = helpers.cgroup()
    project = Path(__file__).resolve().parents[1]
    root = Path(args.output).absolute()
    if root.exists() or not root.resolve().is_relative_to(project / '.tmp'):
        raise ValueError('--output must be a fresh project-local .tmp directory')
    root.mkdir(parents=True)
    for name in ['temp', 'go-cache', 'go-path', 'moon-cache']:
        (root / name).mkdir()
    os.environ.update(TMPDIR=str(root / 'temp'), GOCACHE=str(root / 'go-cache'), GOPATH=str(root / 'go-path'),
                      MOON_HOME=args.moon_home, XDG_CACHE_HOME=str(root / 'moon-cache'))
    os.environ['PATH'] = str(Path(args.moon).parent) + ':' + str(Path(args.node).parent) + ':' + os.environ['PATH']
    results = []
    execute = root / 'execute.cjs'
    execute.write_text("const fs=require('node:fs');WebAssembly.instantiate(fs.readFileSync(process.argv[2]),{}).then(({instance:i})=>console.log(i.exports.entry(3)>>>0)).catch(e=>{console.error(e);process.exit(1)});")
    for language, expected_language in [('m', 0x10), ('mm', 0x11)]:
        folder = root / language
        folder.mkdir()
        source = (project / 'tests/fixtures/debug_symbols' / f'module.{language}').read_text()
        path = folder / f'module.{language}'
        pair = []
        for variant in ['old', 'new']:
            path.write_text(source if variant == 'old' else source.replace('+ 11', '+ 12', 1))
            wasm = folder / f'{variant}.wasm'
            run([args.clang, '--target=wasm32', '-O2', '-g', '-gdwarf-5', '-nostdlib', '-Wl,--no-entry',
                 '-Wl,--allow-undefined', '-Wl,--no-gc-sections', path, '-o', wasm])
            view = mapping(args, wasm)
            assert any(u['language_code'] == expected_language for u in view['debug_info']['units'])
            assert any(s['name'] == '-[AuditObject calculate:]' and s['ranges'] for s in view['debug_info']['symbols'])
            pair.append(wasm)
        report, elapsed = compare(args, *pair)
        assert report['summary']['functions_changed'] == 1, report['summary']
        assert any(s['location']['file'].endswith(f'module.{language}') for a in annotations(report) for s in a['sources'])
        results.append({'language': language, 'summary': report['summary'], 'seconds': elapsed, 'runtime': 'method metadata only; Objective-C runtime not linked'})

    moon = root / 'moon'
    moon.mkdir()
    (moon / 'moon.mod').write_text('name = "audit/symbols"\nversion = "0.1.0"\n')
    (moon / 'moon.pkg').write_text('options(link: { "wasm": { "exports": ["entry"] }, "wasm-gc": { "exports": ["entry"] } },)\n')
    source = '///|\nstruct Audit { bias : Int }\n///|\nfn calculate(x : Int) -> Int {\n  let state : Audit = { bias: 11 }\n  x * 7 + state.bias\n}\n///|\npub fn entry(x : Int) -> Int { calculate(x) }\n'
    for target in ['wasm', 'wasm-gc']:
        pair = []
        for variant in ['old', 'new']:
            (moon / 'symbols.mbt').write_text(source if variant == 'old' else source.replace('bias: 11', 'bias: 12'))
            build = root / f'moon-{target}-{variant}'
            run([args.moon, '-C', moon, '--target-dir', build, 'build', '--target', target, '--debug'])
            wasm = next(build.rglob('symbols.wasm'))
            sidecar = Path(str(wasm) + '.map')
            if not sidecar.exists():
                sidecar = None
            assert run([args.node, execute, wasm]).strip() == (b'32' if variant == 'old' else b'33')
            view = mapping(args, wasm, sidecar)
            if target == 'wasm-gc':
                assert view['debug_summary']['source_ranges'] > 0
                assert all(line['origin'] == 'source_map' for line in view['debug_info']['lines'])
            else:
                assert view['debug_summary']['names'] > 0
            pair.append((wasm, sidecar))
        report, elapsed = compare(args, pair[0][0], pair[1][0], pair[0][1], pair[1][1])
        assert report['summary']['functions_changed'] == 1, report['summary']
        if target == 'wasm-gc':
            assert any(s['location']['line'] == 5 and s['location']['file'].endswith('symbols.mbt') for a in annotations(report) for s in a['sources'])
        results.append({'language': 'MoonBit', 'target': target, 'debug_format': 'source_map' if pair[0][1] else 'name', 'summary': report['summary'], 'seconds': elapsed})

    assembly = root / 'assemblyscript'
    assembly.mkdir()
    path = assembly / 'module.ts'
    source = 'export function entry(x: i32): i32 {\n  return x * 7 + 11;\n}\n'
    pair = []
    for variant in ['old', 'new']:
        path.write_text(source if variant == 'old' else source.replace('+ 11', '+ 12'))
        wasm = assembly / f'{variant}.wasm'
        run([args.node, args.asc, path, '--debug', '--sourceMap', '--outFile', wasm])
        assert run([args.node, execute, wasm]).strip() == (b'32' if variant == 'old' else b'33')
        assert mapping(args, wasm, str(wasm) + '.map')['debug_summary']['source_ranges'] > 0
        pair.append(wasm)
    report, elapsed = compare(args, *pair, str(pair[0]) + '.map', str(pair[1]) + '.map')
    assert report['summary']['functions_changed'] == 1
    assert any(s['location']['line'] == 2 and s['location']['file'].endswith('module.ts') for a in annotations(report) for s in a['sources'])
    results.append({'language': 'AssemblyScript', 'summary': report['summary'], 'seconds': elapsed})

    if args.zig:
        folder = root / 'zig'
        folder.mkdir()
        path = folder / 'module.zig'
        source = 'fn calculate(x: u32) u32 { return x * 7 + 11; }\nexport fn entry(x: u32) u32 { return calculate(x); }\n'
        pair = []
        for variant in ['old', 'new']:
            path.write_text(source if variant == 'old' else source.replace('+ 11', '+ 12'))
            wasm = folder / f'{variant}.wasm'
            run([args.zig, 'build-exe', path, '-target', 'wasm32-freestanding', '-O', 'ReleaseSafe',
                 '-fno-entry', '-rdynamic', '--cache-dir', folder / 'cache', '--global-cache-dir', folder / 'global-cache',
                 '-femit-bin=' + str(wasm)])
            assert run([args.node, execute, wasm]).strip() == (b'32' if variant == 'old' else b'33')
            assert mapping(args, wasm)['debug_summary']['source_ranges'] > 0
            pair.append(wasm)
        report, elapsed = compare(args, *pair)
        assert report['summary']['functions_changed'] == 1, report['summary']
        assert any((s.get('declaration') or {}).get('line') == 1 and s['name'] == 'calculate'
                   for a in annotations(report) for s in a['symbols']), 'optimized inline scope lost'
        results.append({'language': 'Zig', 'summary': report['summary'], 'seconds': elapsed})

    go = root / 'go'
    go.mkdir()
    path = go / 'main.go'
    source = 'package main\n//go:noinline\nfunc calculate(x int32) int32 { return x*7 + 11 }\n//go:wasmexport entry\nfunc entry(x int32) int32 { return calculate(x) }\nfunc main() { println(calculate(3)) }\n'
    pair = []
    for variant in ['old', 'new']:
        path.write_text(source if variant == 'old' else source.replace('+ 11', '+ 12'))
        wasm = go / f'{variant}.wasm'
        run(['env', 'GOOS=wasip1', 'GOARCH=wasm', args.go, 'build', '-gcflags=all=-l', '-o', wasm, path])
        view = mapping(args, wasm)
        assert view['debug_summary']['go_virtual_source_ranges'] > 0
        assert not view['debug_info']['lines'], 'Go virtual PCs must not become physical source ranges'
        assert any(s['origin'] == 'go_pclntab' and s['name'] == 'main.calculate' for s in view['debug_info']['symbols'])
        pair.append(wasm)
        if variant == 'old':
            output = go / 'map.json'
            output.write_text(json.dumps(view))
            base, memory = go_memory(wasm)
            table = view['debug_info']['go_tables'][0]
            data = memory[table['memory_address'] - base:]
            count = struct.unpack_from('<Q', data, 8)[0]
            functab = struct.unpack_from('<Q', data, 64)[0]
            if table['stored_pc_encoding'] == 'function_index':
                # debug/gosym assumes native PCs; lift Go's Wasm PC_F fields to
                # virtual entry PCs before asking the independent Go decoder.
                for i in range(count + 1):
                    at = functab + i * 8
                    struct.pack_into('<I', data, at, struct.unpack_from('<I', data, at)[0] << 16)
                    if i < count:
                        at = functab + struct.unpack_from('<I', data, at + 4)[0]
                        struct.pack_into('<I', data, at, struct.unpack_from('<I', data, at)[0] << 16)
            pcln = go / 'pclntab.bin'
            pcln.write_bytes(data)
            oracle = go / 'oracle.go'
            oracle.write_text(GO_ORACLE)
            checked = int(run([args.go, 'run', oracle, pcln, output]))
            assert checked == len(view['debug_info']['go_lines'])
    report, elapsed = compare(args, *pair)
    assert report['summary']['functions_changed'] == 1, report['summary']
    assert any(s['origin'] == 'go_pclntab' and s['name'] == 'main.calculate' for a in annotations(report) for s in a['symbols'])
    results.append({'language': 'Go', 'target': 'wasip1', 'gosym_checked_ranges': checked, 'summary': report['summary'], 'seconds': elapsed})
    stripped = []
    for wasm in pair:
        target = wasm.with_name(wasm.stem + '-without-names.wasm')
        target.write_bytes(helpers.remove_names(wasm.read_bytes()))
        stripped.append(target)
    report, elapsed = compare(args, *stripped)
    assert report['summary']['functions_changed'] == 1
    assert any(s['origin'] == 'go_pclntab' and s['name'] == 'main.calculate' for a in annotations(report) for s in a['symbols'])
    results.append({'language': 'Go', 'target': 'wasip1', 'without_name_section': True, 'summary': report['summary'], 'seconds': elapsed})
    versions = {label: run(command).decode().strip() for label, command in [
        ('clang', [args.clang, '--version']), ('moon', [args.moon, 'version']),
        ('assemblyscript', [args.node, args.asc, '--version']), ('go', [args.go, 'version']),
        ('node', [args.node, '--version'])]}
    if args.zig:
        versions['zig'] = run([args.zig, 'version']).decode().strip()
    (root / 'results.json').write_text(json.dumps({'cgroup': group, 'cases': results, 'compiler_versions': versions,
                                                  'uwgit_sha256': hashlib.sha256(Path(args.uwgit).read_bytes()).hexdigest()}, indent=2))
    print(json.dumps({'cases': len(results), 'output': str(root)}))


if __name__ == '__main__':
    p = argparse.ArgumentParser(description=__doc__)
    for option in ['output', 'uwgit', 'clang', 'moon', 'moon-home', 'asc', 'node']:
        p.add_argument('--' + option, required=True)
    p.add_argument('--go', default='go')
    p.add_argument('--zig', help='Optional installed Zig compiler for a further DWARF producer')
    main(p.parse_args())
