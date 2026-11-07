#!/usr/bin/env python3
"""Compile controlled C++ edits, execute native/Wasm oracles, and audit original bytes.

Explicit opt-in; needs Clang/LLD with wasm32, g++, Node and WABT. All artifacts
stay in a new project-local .tmp directory. Launch inside a cgroup with memory.max <= 64 GiB.
No source-intent claim is inferred from names or byte similarity alone.
"""
import argparse
import difflib
import hashlib
import json
from pathlib import Path
import subprocess
import time

SEEDS = [0, 1, 4, 7, 31, 63, 64, 126, 127, 128, 255, 1023, 0xFFFFFFFF]
VARIANTS = {
    "boundary": ("value > 127", "value >= 127"),
    "body_collision": ("x * 7 + 3", "x * 11 + 3"),
    "raii": ("total += 3", "total += 5"),
    "ordering": ("a.price < b.price", "a.price > b.price"),
    "route": ("cost + edge < distance[j]", "cost + edge + 2 < distance[j]"),
    "call_target": ("sum = discount(sum)", "sum = surcharge(sum)"),
    "layout": ("u32 price; // layout insertion point", "u32 extra;\n    u32 price; // layout insertion point"),
    "data": ("{13, 17, 19, 23, 29, 31, 37, 41}", "{13, 17, 19, 23, 43, 31, 37, 41}"),
    "rename": ("domestic_fee", "domestic_fee_renamed"),
    "dead": ("x * 97", "x * 101"),
    "template": ("mix<2654435761u>", "mix<2654435767u>"),
    "atomic": ("u32 step = delta + 1", "u32 step = delta + 2"),
}
MODES = {
    "O0": ["-O0"],
    "O2": ["-O2"],
    "O3-lto": ["-O3", "-flto"],
    "O2-threads": ["-O2", "-matomics", "-mbulk-memory", "-Wl,--shared-memory", "-Wl,--max-memory=16777216"],
}
SINGLE_FUNCTION_EDITS = {
    "boundary": "::classify(",
    "ordering": "::insertion_sort<",
    "raii": "::evaluate(",
    "route": "::route(",
    "atomic": "atomic_export",
}


def sha(data):
    return hashlib.sha256(data).hexdigest()


def command(args):
    p = subprocess.run(list(map(str, args)), stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=180)
    if p.returncode:
        raise RuntimeError(f"{args!r} exited {p.returncode}: {p.stderr.decode(errors='replace')}")
    return p.stdout


def check_cgroup():
    line = next(x for x in Path('/proc/self/cgroup').read_text().splitlines() if x.startswith('0::'))
    here = Path('/sys/fs/cgroup') / line[3:].lstrip('/')
    limits = []
    for parent in [here, *here.parents]:
        file = parent / 'memory.max'
        if file.exists() and file.read_text().strip() != 'max':
            limits.append(int(file.read_text()))
    if not limits or min(limits) > 64 * 1024**3:
        raise RuntimeError('run in a cgroup with an effective memory.max <= 64 GiB')
    return {'path': str(here), 'effective_memory_max': min(limits)}


def uleb(data, at):
    value = shift = 0
    while True:
        byte = data[at]; at += 1
        value |= (byte & 127) << shift
        if not byte & 128:
            return value, at
        shift += 7
        assert shift <= 35


def function_names(data):
    # Independent reader for the standard name subsection of trusted compiler
    # output. No wasm-git parsing or matching is used as the symbol oracle.
    result = {}; at = 8
    while at < len(data):
        section = data[at]; length, start = uleb(data, at + 1); end = start + length
        if section == 0:
            size, cursor = uleb(data, start)
            if data[cursor:cursor+size] == b'name':
                cursor += size
                while cursor < end:
                    kind = data[cursor]; size, payload = uleb(data, cursor + 1)
                    cursor = payload + size
                    if kind == 1:
                        count, pos = uleb(data, payload)
                        for _ in range(count):
                            index, pos = uleb(data, pos); n, pos = uleb(data, pos)
                            result[index] = data[pos:pos+n].decode(); pos += n
                        assert pos == cursor
        at = end
    assert at == len(data)
    return result


def replay(a, b, report):
    output = bytearray(); ac = bc = 0
    for edit in report['audit']['edits']:
        x, y = edit['old_span'], edit['new_span']
        assert ac <= x['start'] <= x['end'] <= len(a)
        assert bc <= y['start'] <= y['end'] <= len(b)
        assert a[x['start']:x['end']].hex() == edit['old_hex']
        assert b[y['start']:y['end']].hex() == edit['new_hex']
        assert a[ac:x['start']] == b[bc:y['start']]
        output += a[ac:x['start']] + bytes.fromhex(edit['new_hex'])
        ac, bc = x['end'], y['end']
    output += a[ac:]
    assert output == b


def run(args):
    resource = check_cgroup()
    root = Path(args.output).absolute()
    allowed = Path(__file__).resolve().parents[1] / '.tmp'
    if (allowed.resolve() != allowed or '..' in root.parts or root.exists()
            or root.resolve() == allowed or not root.resolve().is_relative_to(allowed)):
        raise ValueError('--output must be a new directory under the project .tmp/')
    root.mkdir(parents=True)
    source = Path(__file__).resolve().parents[1] / 'tests/fixtures/cpp_intent/engine.cpp'
    base = source.read_text()
    sources = {'base': base}
    for name, (old, new) in VARIANTS.items():
        assert old in base
        sources[name] = base.replace(old, new)
    sources['layout'] = sources['layout'].replace('Record record{seed, sum, events}', 'Record record{seed, 0, sum, events}')
    a = next(line for line in base.splitlines(True) if 'KEEP u32 domestic_fee' in line)
    b = next(line for line in base.splitlines(True) if 'KEEP u32 foreign_fee' in line)
    assert a + b in base
    sources['reorder'] = base.replace(a + b, b + a)
    driver = root / 'driver.cpp'
    driver.write_text('''#include <cstdio>
using u32=__UINT32_TYPE__;
extern "C" u32 evaluate_export(u32,u32),fees_export(u32,u32),boundary_export(u32),table_export(u32),atomic_export(u32);
int main(){const u32 seeds[]={''' + ','.join(str(x)+'u' for x in SEEDS) + '''};
for(u32 s:seeds)for(u32 m=0;m<4;++m)std::printf("%u,%u,%u,%u,%u,%u,%u\\n",s,m,evaluate_export(s,m),fees_export(s,m),boundary_export(s),table_export(s),atomic_export(s));}
''')
    js = root / 'execute.cjs'
    js.write_text('''const fs=require('node:fs');
(async()=>{const {instance}=await WebAssembly.instantiate(fs.readFileSync(process.argv[2]));
const e=instance.exports;
for(const s of ''' + json.dumps(SEEDS) + ''')for(let m=0;m<4;m++)console.log([s,m,e.evaluate(s,m)>>>0,e.fees(s,m)>>>0,e.boundary(s)>>>0,e.table_value(s)>>>0,e.atomic_step(s)>>>0].join(','));
})().catch(e=>{console.error(e);process.exit(1)});
''')
    commands = []; native = {}; modules = {}; names = {}
    for name, text in sources.items():
        d = root / 'sources' / name; d.mkdir(parents=True)
        cpp = d / 'engine.cpp'; cpp.write_text(text)
        (d/'source.diff').write_text(''.join(difflib.unified_diff(base.splitlines(True), text.splitlines(True), fromfile='base/engine.cpp', tofile=f'{name}/engine.cpp')))
        exe = d / 'native'
        cmd = [args.cxx, '-std=c++20', '-O2', '-fno-builtin', '-fno-tree-loop-distribute-patterns', '-Wno-attributes', cpp, driver, '-o', exe]
        command(cmd); commands.append(list(map(str, cmd))); native[name] = command([exe])
        (d/'native.csv').write_bytes(native[name])
        for mode, flags in MODES.items():
            d = root / mode / name; d.mkdir(parents=True)
            # Identical basename prevents an output-name change from masquerading
            # as a source edit in the Wasm module-name custom subsection.
            wasm = d / 'module.wasm'
            cmd = [args.clang, '--target=wasm32-unknown-unknown', '-std=c++20', *flags,
                   '-ffreestanding', '-fno-builtin', '-fno-exceptions', '-fno-rtti', '-nostdlib', '-Wl,--no-entry', cpp, '-o', wasm]
            command(cmd); commands.append(list(map(str, cmd)))
            command([args.validate, '--enable-all', wasm])
            (d/'module.wat').write_bytes(command([args.wasm2wat, '--enable-all', wasm]))
            output = command([args.node, js, wasm]); assert output == native[name], (mode, name, 'native/Wasm mismatch')
            (d/'wasm.csv').write_bytes(output)
            raw = wasm.read_bytes(); names[mode,name] = function_names(raw)
            modules[mode,name,False] = wasm
            stripped = d/'stripped.wasm'; stripped.write_bytes(raw)
            command([args.strip, stripped]); modules[mode,name,True] = stripped
            assert command([args.node, js, stripped]) == output
    rows = []
    for mode in MODES:
        for name in sources:
            if name == 'base': continue
            for stripped in [False,True]:
                a = modules[mode,'base',stripped]; b = modules[mode,name,stripped]
                run_args = ['diff',a,b,'--format','json','--compact']
                start = time.perf_counter()
                raw = command([args.wasm_git,'--threads','1',*run_args]); elapsed = time.perf_counter()-start
                assert raw == command([args.wasm_git,'--threads','0',*run_args]), (mode,name,'parallel output mismatch')
                d = json.loads(raw); replay(a.read_bytes(),b.read_bytes(),d)
                assert d['function_analysis']['available'] and d['audit']['old']['standard']['validation']=='valid' and d['audit']['new']['standard']['validation']=='valid'
                target = b.parent / ('stripped.diff.json' if stripped else 'named.diff.json'); target.write_bytes(raw)
                old_names, new_names = names[mode,'base'], names[mode,name]
                changed=[]
                for c in d['changes']:
                    if 'function' in c:
                        f=c['function']; changed.append({'old':old_names.get(f['old_source_index']), 'new':new_names.get(f['new_source_index']),
                            'kind':f['change_kind'],'operators':len(f['operators']), 'alignment_complete':f['instruction_alignment']['complete']})
                        if not stripped:
                            assert f.get('old_symbol')==old_names.get(f['old_source_index']) and f.get('new_symbol')==new_names.get(f['new_source_index'])
                cross=[{'old':old_names.get(m['old_source_index']), 'new':new_names.get(m['new_source_index']), 'ambiguous':m['ambiguous']}
                       for m in d['function_matches'] if old_names.get(m['old_source_index']) != new_names.get(m['new_source_index'])]
                if name in ['data','dead','rename']:
                    assert d['summary']['functions_changed']==0, (mode,name,'spurious instruction modification')
                if name in ['boundary','body_collision','ordering','raii','route','atomic']:
                    assert d['summary']['functions_changed']==1, (mode,name,d['summary'])
                if name in SINGLE_FUNCTION_EDITS:
                    expected = SINGLE_FUNCTION_EDITS[name]
                    if mode == 'O0':
                        expected = {'raii': '::Counter::~Counter(', 'ordering': '::Priority::operator()'}.get(name, expected)
                    assert expected in changed[0]['old'] and changed[0]['old'] == changed[0]['new'], (mode, name, changed)
                    assert changed[0]['alignment_complete'], (mode, name, 'incomplete instruction alignment')
                if name=='body_collision':
                    if not stripped:
                        assert not cross and 'domestic_fee' in changed[0]['old'] and changed[0]['old']==changed[0]['new'], (mode,name,'wrong changed function')
                    else:
                        assert any(m['ambiguous'] for m in d['function_matches']), (mode,name,'erased ambiguity')
                if name=='call_target':
                    caller = next(f for f in changed if '::evaluate(' in f['old'])
                    assert caller['kind'] != 'confirmed_reference_relocation', (mode,name,'retarget hidden as relocation')
                    assert all(f['kind'] != 'body_or_interface_change' or '::evaluate(' in f['old'] for f in changed), (mode,name,changed)
                    bindings = [binding for c in d['changes'] if 'function' in c for binding in c['function']['call_bindings']]
                    retargets = [binding for binding in bindings if binding['old'] and binding['new']
                                and '::discount(' in old_names.get(binding['old']['function_index'],'')
                                and '::surcharge(' in new_names.get(binding['new']['function_index'],'')]
                    assert retargets and all(b['target_changed'] is not False for b in retargets), (mode,name,'lost changed binding')
                if name=='dead' or (name=='rename' and stripped):
                    assert a.read_bytes()==b.read_bytes() and not d['binary_changed'], (mode,name,'compiler removed source change')
                if name == 'layout' and not stripped and cross:
                    # Exact body reuse can disagree with same-source symbols.
                    # Do not pass off the implementation view as a source rename.
                    text_report = command([args.wasm_git, 'diff', a, b]).decode()
                    assert 'names differ; body correspondence is not a source rename proof' in text_report
                if name in ['dead','rename','layout','reorder']:
                    assert native[name]==native['base'], (mode,name,'unexpected behavior change')
                else:
                    assert native[name]!=native['base'], (mode,name,'test never observes source edit')
                if mode=='O2-threads':
                    inspected=json.loads(command([args.wasm_git,'inspect',b,'--json']))
                    assert any(e['feature']=='threads' for e in inspected['audit']['standard']['evidence'])
                # Serialized native patches independently reproduce every byte.
                patch_file=b.parent/('stripped.wgp' if stripped else 'named.wgp')
                command([args.wasm_git,'patch',a,b,'--output',patch_file])
                restored=b.parent/('stripped.restored.wasm' if stripped else 'named.restored.wasm')
                command([args.wasm_git,'apply',a,patch_file,'--output',restored]); assert restored.read_bytes()==b.read_bytes()
                row={'mode':mode,'case':name,'stripped':stripped,'summary':d['summary'],'changed_functions':changed,
                     'cross_symbol_pairs':cross,'runtime_changed':native[name]!=native['base'],'binary_changed':d['binary_changed'],
                     'old_sha256':sha(a.read_bytes()),'new_sha256':sha(b.read_bytes()),'serial_diff_seconds':elapsed}
                if args.baseline_wasm_git:
                    previous=json.loads(command([args.baseline_wasm_git,'--threads','1',*run_args]))
                    (b.parent/('stripped.before.json' if stripped else 'named.before.json')).write_text(json.dumps(previous))
                    row['previous_cross_symbol_pairs']=sum(old_names.get(m['old_source_index'])!=new_names.get(m['new_source_index']) for m in previous['function_matches'])
                rows.append(row);print(mode,name,'stripped' if stripped else 'named','ok',flush=True)
    receipt={'format':1,'resources':resource,'source_sha256':sha(source.read_bytes()),
             'harness_sha256':sha(Path(__file__).read_bytes()),
             'compiler_version':command([args.clang,'--version']).decode(),'native_compiler_version':command([args.cxx,'--version']).decode(),
             'wasm_git_sha256':sha(Path(args.wasm_git).read_bytes()),'wasm_modules':len(modules),'comparisons':len(rows),
             'runtime_vectors_per_module':len(SEEDS)*4,'thread_scheduling_tested':False,'commands':commands,'cases':rows}
    (root/'receipt.json').write_text(json.dumps(receipt,indent=2)+'\n')
    print(f"passed {len(rows)} compiled comparisons; receipt: {root/'receipt.json'}")


if __name__ == '__main__':
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--output',required=True); p.add_argument('--wasm-git',required=True);p.add_argument('--baseline-wasm-git')
    p.add_argument('--clang',default='clang++');p.add_argument('--cxx',default='g++');p.add_argument('--node',default='node')
    p.add_argument('--validate',default='wasm-validate');p.add_argument('--wasm2wat',default='wasm2wat');p.add_argument('--strip',default='wasm-strip')
    run(p.parse_args())
