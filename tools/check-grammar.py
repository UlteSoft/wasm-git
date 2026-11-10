#!/usr/bin/env python3
"""Review gate for syntax upgrades; never silently accept a dependency's new grammar."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import tomllib

ROOT = Path(__file__).resolve().parents[1]
VENDOR = ROOT / "vendor/wasmparser"
LOCK = ROOT / "syntax/grammar-lock.json"


def inventory():
    source = (VENDOR / "src/lib.rs").read_text()
    source = source.split("macro_rules! _for_each_operator_group {", 1)[1]
    # Stop at the first macro's closing brace, not other generated SIMD lists.
    depth, end = 1, 0
    for end, char in enumerate(source):
        depth += (char == "{") - (char == "}")
        if depth == 0:
            break
    source = source[:end]
    entries = []
    group = None
    token = re.compile(r"@(\w+)\s*\{|\b(\w+)\s*(?:\{([^{}]*)\})?\s*=>\s*(visit_\w+)", re.S)
    for match in token.finditer(source):
        if match[1]:
            group = match[1]
        else:
            entries.append([group, match[2], re.sub(r"\s+", "", match[3] or ""), match[4]])
    if len(entries) < 500 or len({row[1] for row in entries}) != len(entries):
        raise RuntimeError("operator macro layout changed; review the inventory reader")
    version = tomllib.loads((VENDOR / "Cargo.toml").read_text())["package"]["version"]
    revision = json.loads((VENDOR / ".cargo_vcs_info.json").read_text())["git"]["sha1"]
    contract = (ROOT / "src/grammar.rs").read_text()
    if f'"wasmparser {version}"' not in contract or f'"{revision}"' not in contract:
        raise RuntimeError("grammar contract and decoder pin disagree")
    # Operand types and flag readers can change without adding an opcode.
    sources = sorted(str(p.relative_to(VENDOR / "src")) for p in (VENDOR / "src").rglob("*.rs"))
    return {"schema": 1, "decoder": version, "upstream_revision": revision,
            "wire_sources_sha256": {p: hashlib.sha256((VENDOR / "src" / p).read_bytes()).hexdigest()
                                    for p in sources}, "operators": entries}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", type=Path, help="write a review candidate; does not update the lock")
    args = parser.parse_args()
    result = inventory()
    if args.candidate:
        args.candidate.write_text(json.dumps(result, indent=2) + "\n")
        print(f"Review candidate: {args.candidate}; {len(result['operators'])} operators")
    elif result != json.loads(LOCK.read_text()):
        raise SystemExit("Syntax changed: follow documents/SYNTAX_EVOLUTION.eu_US.md before updating the lock")
    else:
        print(f"Syntax inventory verified: {len(result['operators'])} operators, decoder {result['decoder']}")


if __name__ == "__main__":
    main()
