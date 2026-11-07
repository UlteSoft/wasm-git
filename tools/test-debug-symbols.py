#!/usr/bin/env python3
"""Opt-in real compiler oracle. Outputs stay in a fresh project-local .tmp directory.

Requires a <=64 GiB cgroup, Clang/LLD, wasm32 Rust std, TinyGo and Node.
Checks debug mappings against original instruction offsets, source edits,
runtime results, single/multiple worker output, and exact byte-patch replay.
Does not install tools or execute any program named by debug metadata.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import time


def run(args, cwd=None):
    p = subprocess.run(list(map(str, args)), cwd=cwd, stdout=subprocess.PIPE,
                       stderr=subprocess.PIPE, timeout=240)
    if p.returncode:
        raise RuntimeError(f'{args!r}: {p.returncode}\n{p.stderr.decode(errors="replace")}')
    return p.stdout


def cgroup():
    line = next(x for x in Path('/proc/self/cgroup').read_text().splitlines() if x.startswith('0::'))
    here = Path('/sys/fs/cgroup') / line[3:].lstrip('/')
    limits = [int((p/'memory.max').read_text()) for p in [here, *here.parents]
              if (p/'memory.max').exists() and (p/'memory.max').read_text().strip() != 'max']
    assert limits and min(limits) <= 64 * 1024**3, 'run inside a <=64 GiB cgroup'
    return str(here)


def leb(data, pos):
    n = shift = 0
    while True:
        b = data[pos]; pos += 1; n |= (b & 127) << shift
        if b < 128: return n, pos
        shift += 7


def remove_names(data):
    out = bytearray(data[:8]); pos = 8
    while pos < len(data):
        start = pos; kind = data[pos]; size, payload = leb(data, pos+1); pos = payload+size
        if kind == 0:
            n, name = leb(data, payload)
            if data[name:name+n] == b'name': continue
        out += data[start:pos]
    return out


def replay(a, b, edits):
    out = bytearray(); cursor = bc = 0
    for e in edits:
        x, y = e['old_span'], e['new_span']
        assert a[cursor:x['start']] == b[bc:y['start']]
        assert a[x['start']:x['end']].hex() == e['old_hex']
        assert b[y['start']:y['end']].hex() == e['new_hex']
        out += a[cursor:x['start']] + bytes.fromhex(e['new_hex']); cursor=x['end']; bc=y['end']
    out += a[cursor:]; assert out == b


def main(args):
    group = cgroup(); root = Path(args.output).absolute()
    allowed = Path(__file__).resolve().parents[1] / '.tmp'
    if (allowed.resolve() != allowed or '..' in root.parts or root.exists()
            or root.resolve() == allowed or not root.resolve().is_relative_to(allowed)):
        raise ValueError('--output must be a new directory under the project .tmp/')
    root.mkdir(parents=True)
    fixtures = Path(__file__).resolve().parents[1]/'tests/fixtures/debug_symbols'
    execute = root/'execute.cjs'
    execute.write_text('''const fs=require('node:fs');
(async()=>{const {instance}=await WebAssembly.instantiate(fs.readFileSync(process.argv[2]),{});
if(instance.exports._initialize)instance.exports._initialize();
console.log(instance.exports.entry(3)>>>0);})().catch(e=>{console.error(e);process.exit(1)});
''')
    configurations = [('c-dwarf2', 'c', 2), ('c-dwarf3', 'c', 3), ('c-dwarf4', 'c', 4), ('cpp-dwarf5', 'cpp', 5),
                      ('rust-dwarf4', 'rs', 4), ('rust-dwarf5', 'rs', 5), ('tinygo', 'go', 4)]
    results = []
    commands = []
    for name, language, dwarf in configurations:
        source = (fixtures/f'module.{language}').read_text()
        needle = '.wrapping_add(11)' if language == 'rs' else '+ 11'
        changed_line = next(i for i,s in enumerate(source.splitlines(),1) if needle in s)
        modules=[]
        for variant in ['old','new']:
            folder=root/name/variant; folder.mkdir(parents=True)
            path=folder/f'module.{language}'; path.write_text(source if variant=='old' else source.replace(needle,needle.replace('11','12')))
            wasm=folder/'module.wasm'
            if language in ['c','cpp']:
                command=[args.clang,'--target=wasm32','-O2','-g',f'-gdwarf-{dwarf}','-nostdlib','-Wl,--no-entry',f'-fdebug-prefix-map={folder}=/source',path,'-o',wasm]
            elif language=='rs':
                command=[args.rustc,'--target=wasm32-unknown-unknown','--crate-type=cdylib','-C','opt-level=2','-C','debuginfo=2','-C',f'dwarf-version={dwarf}','-C','panic=abort','--remap-path-prefix',f'{folder}=/source',path,'-o',wasm]
            else:
                command=[args.tinygo,'build','-target=wasm-unknown','-gc=leaking','-scheduler=none','-panic=trap','-opt=2','-o',wasm,path]
            run(command); commands.append(list(map(str,command))); modules.append(wasm)
            value=run([args.node,execute,wasm]).strip(); assert value==(b'32' if variant=='old' else b'33'), (name,variant,value)
            mapdata=json.loads(run([args.wasm_git,'debug-map',wasm,'--json']))
            (folder/'map.json').write_text(json.dumps(mapdata,indent=2))
            assert mapdata['debug_summary']['source_ranges']>0, (name,mapdata['debug_summary'])
            assert any('calculate' in (s.get('name') or '') for s in mapdata['debug_info']['symbols']), name
            assert mapdata['debug_summary']['module_sha256']==hashlib.sha256(wasm.read_bytes()).hexdigest()
            for row in mapdata['debug_info']['lines']:
                loc=row['location']
                if loc['file'].endswith(f'/module.{language}') and loc.get('file_md5'):
                    assert loc['file_md5']==hashlib.md5(path.read_bytes()).hexdigest(),(name,loc)
        for strip_names in [False,True]:
            pair=[]
            for module in modules:
                p=module.with_name('without-name.wasm') if strip_names else module
                if strip_names: p.write_bytes(remove_names(module.read_bytes()))
                pair.append(p)
            command=['diff',*pair,'--format','json','--compact']; start=time.monotonic()
            output=run([args.wasm_git,'--threads','1',*command]); elapsed=time.monotonic()-start
            assert output==run([args.wasm_git,'--threads','0',*command]), (name,'parallel mismatch')
            report=json.loads(output); (root/name/f'diff-{strip_names}.json').write_bytes(output)
            replay(pair[0].read_bytes(),pair[1].read_bytes(),report['audit']['edits'])
            assert report['summary']['functions_changed']==1, (name,report['summary'])
            f=next(c['function'] for c in report['changes'] if 'function' in c)
            annotations=f.get('source_annotations',[]); assert annotations,(name,f)
            # Optimization can attribute a folded caller constant to an inlined
            # callee's instruction. Its call site is evidence, not a forced rewrite
            # of the compiler's source row into the known experimental edit line.
            source_lines = [s['location']['line'] for p in annotations for side in ['old','new']
                            for s in (p.get(side) or {}).get('sources',[])]
            inline_calls = [(s.get('call_site') or {}).get('line') for p in annotations for side in ['old','new']
                            for s in (p.get(side) or {}).get('symbols',[])]
            assert changed_line in source_lines + inline_calls,(name,changed_line,annotations)
            results.append({'case':name,'without_name_section':strip_names,'changed_line':changed_line,
                            'mapped_changes':len(annotations),'seconds':elapsed,'summary':report['summary']})
    (root/'results.json').write_text(json.dumps({'cgroup':group,'commands':commands,'cases':results},indent=2))
    print(json.dumps({'cases':len(results),'output':str(root)}))


if __name__=='__main__':
    p=argparse.ArgumentParser();p.add_argument('--output',required=True);p.add_argument('--wasm-git',required=True)
    p.add_argument('--clang',default='clang-21');p.add_argument('--rustc',default='rustc');p.add_argument('--tinygo',default='tinygo');p.add_argument('--node',default='node')
    main(p.parse_args())
