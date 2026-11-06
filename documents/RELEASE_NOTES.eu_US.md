# uwgit 0.1.1 prerelease

Lossless WebAssembly audit and binary version history, with shared native client
and server sources. This prerelease uses uwgit repository/network formats; it is
not a drop-in Git protocol implementation.

- Windows release binaries statically link the C/C++ runtime. Their actual PE
  imports are checked before packaging. This fixes startup on clean Windows
  installations without the VC redistributable; use this version instead of
  the 0.1.0 Windows packages.
- Eight native platform packages: Linux x86_64/AArch64/ARMv7/RV64GC, Windows
  x86_64/ARM64, macOS x86_64/ARM64. Each includes client and standalone server.
- Two WASIp1 client packages: independently validated MVP and Core 2.0 modules,
  including rebuilt standard library, libc and Zstd. Both retain full audit
  interpretation independently of their own executable instruction profile.
- Binary history, compression/GC, lossless patch restoration, section/function/
  block/instruction and custom-section audits, and available debug-symbol maps.
- Native SSH/OpenPGP commit signatures, HTTP/SSH server workflows, shared forks,
  persistent pull requests, reviews/checks, branch protection and merge recovery.
- Publication requires the regression/build matrix, QEMU Linux execution,
  native Windows/macOS execution, WASI runtime smoke, and artifact allowlisting.

Verify downloaded archives against `SHA256SUMS`. See the included bilingual
release guide for commands, tested architectures, WASI capability limits,
directory-sync semantics and the distinction between native CI and local QEMU
guest testing. WASIp1 runs one worker and does not provide external process,
network-server or advisory-lock-dependent workflows. Pre-release formats may
change; existing superseded pre-release repositories are not migrated.
