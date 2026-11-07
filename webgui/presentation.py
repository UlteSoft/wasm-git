"""Presentation boundary: no process arguments, credentials or key bytes by default.

Wasm instruction/byte evidence is retained verbatim. Raw reports may contain
arbitrary user data and are only available through an explicit export action.
"""
import re
import os
from pathlib import PurePosixPath

PRIVATE = re.compile(r"(?:api.?key|token|password|passphrase|secret|credential|authorization|private.?key|public.?key|signature|environment|^command$|^argv$|^args$)", re.I)
PEM = re.compile(r"-----BEGIN [^-\r\n]*PRIVATE KEY-----[\s\S]*?(?:-----END [^-\r\n]*PRIVATE KEY-----|$)")
ASSIGNMENT = re.compile(r"(?i)\b((?:[\w.-]*(?:token|password|passphrase|secret|credential|authorization|api[_-]?key))[\"']?\s*[:=]\s*)(?:[\"'][^\"'\r\n]*[\"']|[^\s,;}]+)")
BEARER = re.compile(r"(?i)\b(Bearer|Basic)\s+[A-Za-z0-9+/_=.:-]+")
USERINFO = re.compile(r"([a-zA-Z][a-zA-Z0-9+.-]*://)[^/\s@]+@")


CREDENTIAL_ENV = re.compile(r'(?:TOKEN|PASSWORD|PASSPHRASE|SECRET|PRIVATE_KEY|API_?KEY)', re.I)


def credential_values():
    return tuple(value for key, value in os.environ.items() if len(value) >= 4 and CREDENTIAL_ENV.search(key))


def redact(value, secrets):
    value = str(value)
    for secret in secrets:
        value = value.replace(secret, '[hidden]')
    value = PEM.sub('[private key hidden]', value)
    value = USERINFO.sub(r'\1[credentials hidden]@', value)
    value = BEARER.sub(r'\1 [hidden]', value)
    return ASSIGNMENT.sub(r'\1[hidden]', value)


def text(value):
    return redact(value, credential_values())


def clean(value):
    secrets = credential_values()
    def walk(node):
        if isinstance(node, dict):
            return {key: walk(item) for key, item in node.items() if not PRIVATE.search(key)}
        if isinstance(node, list):
            return [walk(item) for item in node]
        return redact(node, secrets) if isinstance(node, str) else node
    return walk(value)


def choose(value, names):
    return {key: value[key] for key in names.split() if key in value} if isinstance(value, dict) else {}


def basename(value):
    return PurePosixPath(str(value).replace('\\', '/')).name if value else value


def query(operation, value):
    if operation == 'history':
        rows = []
        for item in value['commits']:
            row = choose(item, 'id message parents shallow')
            row['signed'] = bool(item.get('signature'))
            rows.append(clean(row))
        return {**choose(value, 'offset total'), 'commits': rows}
    if operation == 'overview':
        status = choose(value['status'], 'branch head detached staged worktree files untracked legacy_head_unmapped')
        pending = value['status'].get('pending')
        if pending:
            status['pending'] = choose(pending, 'operation status conflicts')
        refs = value.get('refs', {})
        return {'status': status, 'refs': {'refs': [choose(r, 'name id current') for r in refs.get('refs', [])]}}
    if operation == 'inspect':
        audit = value.get('audit', {})
        module = value.get('module', {})
        return {'summary': value.get('summary', {}), 'function_analysis': value.get('function_analysis', {}),
                'audit': {**choose(audit, 'byte_len format validation'),
                          'sections': [choose(s, 'ordinal id kind path span payload_span status') for s in audit.get('sections', [])]},
                'module': {'exports': module.get('exports', []), 'imports': module.get('imports', []),
                           'functions': [choose(f, 'source_index type_id kind export_names symbol') for f in module.get('functions', [])]},
                'debug_status': value.get('debug_info', {}).get('status', 'absent')}
    if operation == 'debug':
        result = clean(choose(value, 'schema_version debug_summary debug_info annotation'))
        # Compiler build directories and embedded source contents are not needed
        # for the normal symbol/line view. Explicit exports retain the full report.
        def trim(node):
            if isinstance(node, dict):
                for key in list(node):
                    if key in ('uri', 'producer', 'metadata_source', 'sources_content', 'source_content'):
                        del node[key]
                    elif key in ('file', 'source') and isinstance(node[key], str) and ('/' in node[key] or '\\' in node[key]):
                        node[key] = basename(node[key])
                    else:
                        trim(node[key])
            elif isinstance(node, list):
                for item in node:
                    trim(item)
        trim(result)
        return result
    if operation == 'diff':
        # Never rewrite native offsets, symbols, instruction text or hex bytes.
        # Input provenance paths and the whole original module dumps are omitted.
        def audit(item):
            return choose(item, 'changed edits semantic_changes custom_changes exact_minimal alignment')
        def diff(item):
            selected = choose(item, 'kind summary changes function_analysis function_matching function_matches binary_changed whole_module_patch sections selection diagnostics selectors unmatched_selectors coverage cross_section_dependencies validation_scope selection_complete selected_content_changed selected_position_changed module_binary_changed')
            if 'audit' in item:
                selected['audit'] = audit(item['audit'])
            if 'edits' in item:
                selected.update(audit(item))
            if 'files' in item:
                selected['files'] = [{**choose(f, 'path status'), 'diff': diff(f['diff']) if f.get('diff') else {'content': choose(f.get('content'), 'side byte_len sha256 span hex'), 'module': choose(f.get('audit'), 'byte_len format validation available reason')}} for f in item['files']]
            return selected
        return diff(value) if isinstance(value, dict) else value
    return clean(value)


def receipt(value):
    output = value.get('output')
    result = {'exit_code': value['exit_code']}
    if isinstance(output, dict):
        result['summary'] = clean(choose(output, 'status outcome branch head commit id files objects packs removed bytes_before bytes_after stored_bytes logical_bytes errors warnings'))
    else:
        result['summary'] = {}
    if value.get('stderr'):
        result['diagnostic'] = text(value['stderr'])[:4000]
    if value.get('repository'):
        result['repository'] = value['repository']
    return result
