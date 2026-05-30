# QuickDrop

A Windows-first, AirDrop-style LAN file transfer application built with **Tauri 2 + React + Rust**.

This repository is currently at **Step 1 of the build plan** (project scaffolding). All
domain logic (discovery, pairing, transfer engine, file handling) lives in the
`quickdrop-core` crate and is implemented incrementally in subsequent steps.

## Layout

```
quickdrop/
├── Cargo.toml                      # Cargo workspace root
├── package.json                    # Frontend (React + Vite + TS)
├── index.html
├── src/                            # React frontend
├── crates/
│   └── quickdrop-core/             # Platform-agnostic core library
└── src-tauri/                      # Tauri shell (tray, plugins, IPC bridge)
```

## Prerequisites

- **Rust** (stable, MSVC toolchain) — `rustup show` should list `stable-x86_64-pc-windows-msvc`.
- **MSVC C++ Build Tools** with the Windows SDK (required by the Rust MSVC linker).
  Install via the Visual Studio Installer → "Desktop development with C++".
- **Node 20+** and **Yarn 1.x** (Classic).
- **WebView2 Runtime** (preinstalled on Windows 11).

## First-time setup

From the `quickdrop/` directory:

```powershell
yarn install
```

## Run in development

```powershell
yarn tauri dev
```

Logs: `%APPDATA%\QuickDrop\logs\quickdrop.log`. Filter with `QUICKDROP_LOG` env var,
e.g. `QUICKDROP_LOG=debug,sled=warn`.

## Build a release MSI

```powershell
yarn tauri build
```

Produces an MSI under `src-tauri/target/release/bundle/msi/`.

## Runtime data locations

| Purpose | Path |
| --- | --- |
| Settings, DB, logs | `%APPDATA%\QuickDrop\` |
| Default receive folder | `C:\QuickDrop\` |
| Identity keypair | Windows Credential Manager (added in Step 3) |

## Build plan

1. ✅ Scaffold (Tauri 2 + React + TS, workspace, plugins, tray, logging)
2. Identity + TrustStore (Ed25519 in Credential Manager)
3. Discovery service (mDNS + UDP fallback)
4. TLS transport with fingerprint pinning
5. Wire protocol + framed messaging
6. Streaming I/O + backpressure + progress events
7. Resume + atomic writes + duplicate handling
8. Receiver service hardening
9. Pairing UX (SAS dialog, trust persistence)
10. Windows context menu integration (`--send` CLI, registry installer)
11. Packaging + auto-start + uninstall cleanup
12. Test matrix (large files, network drops, multi-device)

## License

MIT OR Apache-2.0
