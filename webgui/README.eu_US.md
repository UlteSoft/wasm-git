# wasm-git Web GUI

[English manual](../documents/WEBGUI.eu_US.md) · [简体中文手册](../documents/WEBGUI.zh_CN.md)

A local browser interface for the native client and server. Python 3.10+ and the
standard library are sufficient; no pip, npm, frontend build or GUI binary is
required. Missing native executables are built from this checkout using Cargo.

Run from the project root:

```sh
python3 webgui/client.py start --repo /path/to/repository --open
python3 webgui/client.py stop

python3 webgui/server.py start --root /path/to/hosted-repositories --open
python3 webgui/server.py stop
```

The client GUI uses port **8737**; the independent supervision GUI uses **8738**.
Omit `--repo` to create, clone or choose a repository in the browser. Both launchers
also accept `serve` (foreground) and `status`. Use `--client /path/to/wasm-git` and
`--server /path/to/wasm-git-server` to choose existing builds; `--no-build` disables
automatic compilation and `--build` explicitly rebuilds.

Choose **English** or **简体中文** at the top right. Switching keeps current form
input and displayed results intact. The preference is remembered per browser
origin. Commit messages, paths, symbols, instruction text, native diagnostics and
downloaded audit bytes remain original evidence.

Common repository workflows have dedicated forms. The capability catalog and
command desk expose the installed client's full argument grammar, binary standard
input/output, signing, PRs, storage and plumbing. The server entry point adds root
inventory, managed HTTP/stdio services, policy/protection compilation and receive
audit inspection. Detailed boundaries and examples are in the bilingual manual.

Open the complete authenticated URL printed at startup. The GUI binds only to
`127.0.0.1`; it is a local administrative tool, not a public WasmHub frontend.
Stopping the GUI also stops commands and services it launched. State and temporary
binary output are stored beneath `.tmp/webgui/` by default and excluded from Git.
