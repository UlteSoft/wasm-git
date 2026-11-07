"""Read-only local server inventory and native receive-audit presentation."""
from pathlib import Path
import socket


def root_path(value):
    root = Path(value).expanduser().resolve()
    if not root.is_dir():
        raise ValueError("Choose an existing server root directory.")
    return root


def overview(root, services):
    root = root_path(root)
    repos = []
    skipped = 0
    unscanned = 0
    # Nested namespaces are allowed. Never enter object stores, hidden paths,
    # symlinks or an unbounded directory tree just to draw a dashboard.
    pending = [(root, 0)]
    visited = 0
    while pending and visited < 10000:
        directory, depth = pending.pop()
        visited += 1
        if (directory / ".wasm-git").is_dir():
            if len(repos) < 1000:
                repos.append({"name": str(directory.relative_to(root)), "path": str(directory)})
            else:
                skipped += 1
            continue
        if depth >= 8:
            unscanned += 1
            continue
        for path in directory.iterdir():
            if not path.name.startswith(".") and not path.is_symlink() and path.is_dir():
                if len(pending) < 10000:
                    pending.append((path, depth + 1))
                else:
                    unscanned += 1
    unscanned += len(pending)
    logs = [str(path) for path in root.glob("*.bin") if path.name.endswith("audit.bin") and path.is_file()]
    return {"root": str(root), "repositories": sorted(repos, key=lambda r: r["name"]),
            "omitted_repositories": skipped, "unscanned_directories": unscanned, "audit_logs": logs[:100],
            "services": services,
            "process_scope": "Only service processes started by this GUI are managed. Existing services remain independently managed."}


def probe(address):
    host, separator, port = address.rpartition(":")
    host = host.strip("[]")
    if not separator or host not in ("127.0.0.1", "localhost", "::1"):
        raise ValueError("TCP probe accepts a loopback host:port only.")
    port = int(port)
    if not 1 <= port <= 65535:
        raise ValueError("Invalid port.")
    try:
        with socket.create_connection((host, port), timeout=2):
            return {"address": address, "reachable": True, "check": "TCP connection only; not protocol authentication or repository health"}
    except OSError as error:
        return {"address": address, "reachable": False, "error": str(error)}
