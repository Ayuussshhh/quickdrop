# QuickDrop Engineering Onboarding Guide

This is a practical onboarding guide for maintaining QuickDrop. It is written like a senior engineer walking you through the system: what matters, why it exists, how execution flows, where state lives, what is dangerous, and how to safely contribute.

One reality check up front: the README still describes the repo as early scaffolding, but the current code already includes discovery, identity, trust, pairing, TLS transport, sender/receiver transfer logic, progress, and Windows context menu support. Treat the source code as the source of truth.

---

# Phase 1 - Big Picture First

## What This Project Does

QuickDrop is a Windows-first, AirDrop-style desktop app for local network file transfer. It discovers nearby QuickDrop devices, lets users pair/trust them, and streams files directly over the LAN without a cloud server.

The user story is simple:

```text
Open app
-> see nearby devices
-> choose files
-> choose a device
-> pair or accept prompt if needed
-> transfer directly over LAN
-> receiver writes files safely to destination folder
```

The engineering story is more interesting:

```text
React UI
-> Tauri IPC command
-> Rust app shell state
-> quickdrop-core domain logic
-> keyring / sled / filesystem / LAN socket
-> Tauri event
-> React state update
-> UI rerender
```

## Main Business Purpose

The product promise is fast, private, local file movement. There are no accounts, no hosted backend, and no upload to a third-party service. The business logic is mostly about safe and trusted transfer rather than normal CRUD.

Important business rules:

- A device identity should be stable across launches.
- A trusted peer should remain trusted until forgotten.
- Untrusted peers need user confirmation.
- A remote peer must never write outside the receive folder.
- A received file must not overwrite an existing file.
- Partial files should not appear as completed files.
- File bytes should be integrity checked.
- Discovery should work on imperfect Windows LANs.

## Architecture Style

This is not a microservice system. It is a modular desktop monolith with an event-driven runtime.

```text
Desktop app monolith
  + thin React renderer
  + Tauri shell / IPC bridge
  + reusable Rust core crate
  + Tokio async tasks
  + direct peer-to-peer LAN protocol
```

The code is modular by responsibility:

- `src`: React UI.
- `src-tauri`: desktop shell, commands, events, tray, plugins, app state.
- `crates/quickdrop-core`: platform-agnostic domain engine.

At runtime it is peer-to-peer:

```text
Device A discovers Device B
-> Device A connects directly to Device B TCP listener
-> TLS protects the byte stream
-> Ed25519 app handshake proves device identity
-> TrustStore or UI prompt decides authorization
-> files stream directly between devices
```

## Main Technologies And Why

Rust is used for networking, crypto, file IO, persistence, and protocol correctness. That is the right place for code that handles hostile paths, partial files, network errors, and trust boundaries.

Tauri 2 wraps the Rust process in a desktop app, gives a webview UI, and provides native plugins for dialogs, tray, notifications, autostart, single-instance behavior, and opener support.

React, TypeScript, and Vite provide the UI. React is not the app brain here; it is a display/control layer over Rust state.

Tokio powers async tasks: receiver listener, per-connection handling, UDP/mDNS discovery, sender streaming, and event pumps.

mDNS plus UDP broadcast handle LAN discovery. mDNS is the standard local service discovery mechanism; UDP broadcast is a pragmatic fallback for Windows networks, VPNs, routers, and corporate Wi-Fi that block or break mDNS.

rustls and tokio-rustls protect the connection. The code intentionally accepts any TLS certificate and authenticates devices at the application layer with Ed25519. That is because this app trusts device keys, not public certificate authorities.

Ed25519 plus keyring creates a stable local device identity. The secret seed lives in the OS credential store, while the public key derives the device UUID and fingerprint.

sled stores trusted peers locally. It is embedded, durable, and does not require a server process.

BLAKE3 hashes file contents quickly for integrity checks.

MessagePack serializes protocol control frames compactly over the TLS stream.

## High-Level Execution Flow

Cold start:

```text
src-tauri/src/main.rs
-> quickdrop_lib::run()
-> install rustls crypto provider
-> resolve app paths
-> initialize logging
-> load settings
-> open sled DB
-> load/create device identity from keyring
-> open TrustStore
-> create TransferManager
-> create AppState
-> build Tauri app
-> register plugins and commands
-> setup tray
-> spawn bootstrap()
-> load React webview
```

Async bootstrap:

```text
bootstrap()
-> start receiver TCP/TLS listener on random port
-> start discovery advertising that port
-> subscribe to peer snapshots
-> copy peer snapshots into AppState.peers_cache
-> emit peers://updated to frontend
```

Normal running state:

```text
Discovery events
-> PeerTable
-> watch channel
-> peers_cache
-> peers://updated
-> React setPeers

Transfer progress
-> TransferManager
-> transfers://updated
-> React setTransfers
```

## How Frontend And Backend Communicate

There is no HTTP API. The API boundary is Tauri IPC.

Frontend to backend uses `invoke()`:

```text
app_info
list_peers
list_transfers
list_trusted_peers
forget_peer
pair_with
send_files
answer_prompt
cancel_transfer
install_context_menu
uninstall_context_menu
```

Backend to frontend uses Tauri events:

```text
peers://updated       -> full peer snapshot
transfers://updated   -> full transfer snapshot
transfers://error     -> send failure message
transfers://received  -> received file paths
prompt://incoming     -> incoming pair/transfer prompt
pairing://sas         -> outgoing pairing SAS code
pairing://done        -> outgoing pairing success
send://files          -> context menu paths forwarded to UI
```

Senior-engineer rule: command names, event names, argument names, and payload shapes are API contracts. Change both sides together and test the full flow.

## How Data Moves Through The System

Device identity:

```text
OS keyring secret seed
-> DeviceIdentity
-> public key
-> SHA-256(public key)
-> fingerprint + UUID
-> discovery advertisements and handshake Hello
```

Peer discovery:

```text
local identity + receiver port
-> mDNS TXT record and UDP beacon
-> remote PeerTable
-> watch snapshot
-> Tauri peers_cache
-> React peers state
```

Trust:

```text
pairing accepted
-> TrustedPeer
-> TrustStore.upsert
-> sled trust/peers/v1
-> sled trust/fp_index/v1
-> list_trusted_peers
-> React trusted state
```

Transfer:

```text
local file paths
-> SendItem list
-> Manifest with rel_path, size, blake3 hash
-> Request::Send over TLS
-> receiver validates paths and authorization
-> sender streams chunks
-> receiver writes .qdpart
-> receiver verifies hashes
-> receiver atomically renames to final file
-> progress/completion events update UI
```

Settings:

```text
settings.json
-> Settings in Arc<RwLock<Settings>>
-> app_info, receiver config, tray destination behavior
```

## Most Important Parts

If you are new, understand these first:

1. `src/App.tsx`: the entire frontend state machine.
2. `src-tauri/src/lib.rs`: app orchestrator, command/event bridge, app state, bootstrap.
3. `crates/quickdrop-core/src/transfer/protocol.rs`: wire contract between devices.
4. `crates/quickdrop-core/src/transfer/handshake.rs`: identity proof after TLS.
5. `crates/quickdrop-core/src/transfer/sender.rs`: outbound transfer and pairing initiator.
6. `crates/quickdrop-core/src/transfer/receiver.rs`: inbound authorization, path safety, file writing.
7. `crates/quickdrop-core/src/discovery.rs`: LAN discovery.
8. `crates/quickdrop-core/src/identity.rs`: stable local device identity.
9. `crates/quickdrop-core/src/pairing.rs`: TrustStore and SAS pairing.
10. `crates/quickdrop-core/src/files.rs`: receiver path safety.

## Brain Files

- `src-tauri/src/lib.rs`: the app brain and integration hub.
- `src/App.tsx`: the UI brain.
- `transfer/protocol.rs`: device-to-device wire contract.
- `transfer/handshake.rs`: authentication protocol.
- `transfer/sender.rs`: send/pair initiator behavior.
- `transfer/receiver.rs`: receive/pair responder behavior.
- `discovery.rs`: local network presence.
- `identity.rs`: root of device identity.
- `pairing.rs`: root of trust persistence.
- `files.rs`: filesystem security boundary.

## Safe Utility/Helper Files

Safe means local changes usually have a smaller blast radius if public behavior is preserved.

- `logging.rs`: logging setup.
- `transfer/hash.rs`: BLAKE3 helper.
- `db.rs`: sled open wrapper.
- `src/main.tsx`: React root mount.
- `src/App.css`: styling, though currently duplicated in sections.
- `src-tauri/build.rs`: Tauri build hook.
- `index.html`: Vite HTML shell.
- `vite.config.ts`: safe-ish, but keep the Tauri dev port aligned.

## Dangerous To Modify

- Protocol structs/enums and `PROTOCOL_VERSION` in `transfer/protocol.rs`.
- `AUTH_DOMAIN` and nonce signing in `transfer/handshake.rs`.
- `KEYRING_SERVICE` and `KEYRING_ACCOUNT` in `identity.rs`.
- Trust tree names in `pairing.rs`.
- Path sanitizer and finalization logic in `files.rs`.
- TLS verifier assumptions in `transport.rs`.
- Receiver authorization/resume/write loop in `receiver.rs`.
- Tauri command names and event names in `src-tauri/src/lib.rs` and `src/App.tsx`.
- Discovery service type, UDP port, TXT keys, and peer TTL in `discovery.rs`.
- `src-tauri/capabilities/default.json`, because it is a frontend-native security boundary.

## Likely Bug Hot Spots

- LAN discovery across firewalls, VPNs, adapters, and public/private Windows networks.
- Transfer resume. Existing `.qdpart` prefix bytes are not rehashed before finalization.
- Receive progress. `TauriHost::on_progress` registers receive rows with total bytes/items as zero, so receive progress can display poorly.
- Prompt timeout. Rust removes timed-out prompt senders, but the frontend prompt modal is not automatically removed.
- CSS duplication in `App.css`. New rules are appended at the end of the file; edit the last occurrence.
- Stale trust flags in the Devices tab until discovery emits again.
- Context-menu `--send` timing through single-instance forwarding.

## What To Understand Before Touching Code

Learn in this order:

1. React invokes commands and listens for events.
2. `AppState` is the long-lived runtime owner.
3. `bootstrap()` starts receiver first, then discovery.
4. Sender and receiver only speak through protocol structs.
5. Discovery is only a hint; handshake proves identity.
6. Trust is authorization, not authentication.
7. Receiver path handling is a security boundary.
8. Transfer progress is UI state derived from backend snapshots.

## Senior Mental Model

Think in three layers:

```text
React UI
  Displays snapshots and collects user intent.

Tauri shell
  Owns desktop lifecycle, IPC, app state, tray, prompts, and event bridge.

quickdrop-core
  Owns identity, trust, discovery, protocol, transfer correctness, and file safety.
```

The UI is not the source of truth. The Rust shell owns runtime state. The core crate owns correctness.

## Recent Feature Additions

These three features were added on top of the base architecture. None of them change the wire protocol; they extend the host-decision step, the sender's chunk loop, and the React input surface.

### 1. Receiver-selected destination folders

The receiver now chooses where an incoming transfer lands, instead of always using the global `Settings.destination`.

- `AcceptDecision::Accept { dest: Option<PathBuf> }` carries the chosen folder. `None` means "fall back to settings/default".
- The host (`TauriHost::on_transfer_request`) decides every transfer. Resolution order for the destination root is `chosen_dest -> Settings.destination -> default_dest`, and path sanitization runs against that resolved root.
- `config::dest_options()` exposes Downloads/Desktop/Documents (with home-dir fallbacks). The prompt payload ships these plus the first file's basename so the UI can render destination chips.
- Per-device memory: `TrustedPeer.dest_override` (serde-default, backward compatible). `TrustStore::set_dest(id, dest)` persists it. A trusted + auto-accept peer silently reuses its remembered destination.
- UI: `TransferPrompt` renders Downloads/Desktop/Documents/"Choose Folder…" chips, a "Remember this destination for {peer}" checkbox, and disables Accept until a destination is selected. `answer_prompt` gained `dest` and `remember` params.

### 2. Adaptive chunk sizing (large-file performance)

`sender::chunk_size_for(file_size)` scales the streaming buffer: `<50 MiB -> 512 KiB`, `<1024 MiB -> 4 MiB`, else `16 MiB`, always clamped to `MAX_CONTROL_FRAME` (16 MiB). This replaces the old fixed ~1 MiB chunk and cuts per-chunk framing/syscall overhead on large files while keeping small-file latency low.

Note: this is deliberately a single-stream optimization. True multi-connection parallel chunking was **not** implemented because a single TLS/TCP stream already saturates a typical LAN link, and parallelism would require a protocol/multi-connection change (out of scope). Resume (`.qdpart`) and BLAKE3 integrity are unaffected — chunk size is a sender-side transport detail only.

### 3. Drag-and-drop file sending

The React webview listens via `getCurrentWebview().onDragDropEvent`. On "over" it highlights the device card under the cursor (`peerIdAt` uses `document.elementFromPoint` against `[data-peer-id]`, DPR-corrected); on "drop" it sets the send paths/target and reuses the existing confirm modal. No new Tauri capability is needed (`core:event:allow-listen` already covers drag-drop events; `dragDropEnabled` defaults to true in Tauri v2).

## Recent Feature Additions — Round 2

A second batch of four features. Like the first round, none change the wire protocol. They extend the trust store, add two new sled-backed stores, and add two React tabs.

### 4. Instant Transfer Mode (per-device auto-accept / auto-save)

Each trusted peer can opt into starting transfers immediately, skipping the approval prompt.

- `TrustedPeer` gained `auto_accept: bool` and `auto_save: bool` (both serde-default, backward compatible). `TrustStore::set_instant_prefs(id, auto_accept, auto_save)` persists them.
- Decision logic in `TauriHost::on_transfer_request`: a request is auto-accepted only when the peer is **trusted** *and* (`dev_auto_accept` **or** the existing global `Settings.auto_accept_trusted`). When `auto_save` is set, the peer's remembered `dest_override` is used as the destination; otherwise the normal resolution chain applies.
- **Security model is preserved**: untrusted peers never qualify for auto-accept regardless of these flags — they always go through the manual prompt. The flags are per-device and only meaningful after pairing.
- UI: `TrustedPane` exposes an "Instant Transfer (auto-accept)" checkbox and an "Auto-save" checkbox (the latter disabled unless auto-accept is on, and it surfaces the remembered destination path).

### 5. Device Roles

Trusted devices carry a human-facing role label: `Mobile | Desktop | Laptop | Nas | Workstation | Other` (`DeviceRole` enum, serde `lowercase`, defaults to `Other`).

- Stored as `TrustedPeer.role` (serde-default). `TrustStore::set_role(id, role)` persists it.
- **Discovery is untouched**: the role is local trust-store metadata only — it is *not* advertised over mDNS or exchanged in the hello handshake. This keeps the role private and avoids any protocol change.
- UI: a role badge on each trusted device plus an inline `<select>` to edit it.
- Designed for future role-specific behavior (e.g. NAS defaults, mobile-specific handling) without further schema changes.

### 6. Transfer History

A new `quickdrop_core::history` module records completed transfers.

- `TransferRecord { id, file_name, direction, peer_id, source_device, target_device, timestamp_ms, size, status, paths }`. `HistoryStore` wraps a sled tree (`history/records/v1`); keys are `timestamp_ms.to_be_bytes() ++ id` so `list()` returns newest-first by ordered scan.
- **No duplicate storage**: records are written exactly once, at the existing transfer-completion points — `send_files`' spawned task writes the `Send` record, and `TauriHost::on_transfer_end` writes the `Receive` record. Both reuse data already flowing through those callbacks (no new tracking state on the hot path). Each write emits `history://updated` so the UI refreshes.
- Commands: `list_history`, `delete_history_entry`, `clear_history`, plus `open_path` / `reveal_path` (via `tauri_plugin_opener`) for Open File / Open Folder.
- UI: `HistoryPane` groups rows into Today / Yesterday / Older (`dayBucket`), with Open File, Open Folder, Resend (re-targets an online peer and jumps to the Devices tab), Delete, and Clear-all actions.

### 7. QuickDrop Spaces (foundation only)

A new `quickdrop_core::spaces` module lays the data/storage groundwork for shared spaces. **Collaboration, comments, and chat are intentionally out of scope** — this is membership, shared storage, and activity tracking only.

- Types: `SpaceType { Personal | Project | Family | Team }`, `MemberRole { Owner | Editor | Viewer }`, `ActivityKind { SpaceCreated, MemberAdded, MemberRemoved, FolderAdded, FolderRemoved }`.
- `Space { id, name, space_type, created_at_ms, revision, updated_at_ms, members, shared_folders }`, with `Member` and `SharedFolder` records.
- `SpaceStore` wraps two sled trees: `spaces/meta/v1` (space documents) and `spaces/activity/v1` (append-only activity log; key = `space_id ++ timestamp_ms.to_be_bytes() ++ activity_id`). Every mutation calls `bump()` to increment `revision` and `updated_at_ms` and appends an `Activity` entry.
- **Future-sync design** (documented in the module): each space carries a monotonically increasing `revision`; peers can reconcile by comparing revisions and replaying the activity-log tail. All identifiers are UUIDs so a merge is idempotent (re-applying a known activity/member/folder is a no-op). No sync transport is implemented yet — the shape is chosen so it can be added without a data migration.
- Commands: `list_spaces`, `create_space`, `delete_space`, `add_space_member`, `remove_space_member`, `add_space_folder`, `space_activity`.
- UI: `SpacesPane` (create form + list) and `SpaceCard` (member add/remove, folder add, an activity-feed toggle, and a `rev N` indicator).

## Architecture Diagram

```text
                +-------------------------+
                | React UI                |
                | src/App.tsx             |
                | tabs, modals, state     |
                +-----------+-------------+
                            |
                  invoke()  |  listen()
                            v
                +-------------------------+
                | Tauri Shell             |
                | src-tauri/src/lib.rs    |
                | AppState, commands,     |
                | events, tray, setup     |
                +-----------+-------------+
                            |
                            v
                +-------------------------+
                | quickdrop-core          |
                | identity, trust,        |
                | discovery, transfer     |
                +----+--------------+-----+
                     |              |
                     v              v
              +-------------+  +--------------------+
              | Local state |  | LAN peer           |
              | keyring     |  | mDNS/UDP/TCP/TLS   |
              | sled/json   |  | file stream        |
              +-------------+  +--------------------+
```

## Request Lifecycle

```text
User action
-> React handler
-> invoke("command", payload)
-> Tauri command function
-> AppState read/write
-> quickdrop-core call if needed
-> immediate command result
-> optional background task
-> task emits event later
-> React listener updates state
-> UI rerenders
```

## Data Lifecycle

```text
File path
-> SendItem
-> ManifestItem
-> Request::Send
-> receiver path sanitizer
-> destination + .qdpart
-> chunk writes
-> hash verification
-> atomic rename
-> completion event
```

## Authentication Flow

There is no user login. Authentication is device identity.

```text
First launch
-> generate Ed25519 key
-> store secret seed in OS keyring
-> derive UUID and fingerprint from public key

Connection
-> TLS channel starts
-> both sides send Hello(public key, fingerprint, nonce)
-> both sides sign the peer nonce
-> both sides verify signatures
-> both sides verify fingerprint matches public key

Authorization
-> pairing creates trusted peer records
-> host (Tauri shell) decides every incoming transfer
-> trusted + auto_accept silently accepts using the peer's remembered destination
-> otherwise UI prompt decides accept/reject AND which destination folder to use
```

## Rendering Flow

```text
index.html
-> src/main.tsx
-> React StrictMode
-> App
-> initial invoke snapshots
-> event listeners
-> state updates
-> active tab pane
-> modals from sendTarget/outgoingSas/prompts
-> toast from toast state
```

## API Flow

QuickDrop's API is Tauri IPC:

```text
React invoke/listen
<-> Tauri command/event boundary
<-> Rust AppState
<-> quickdrop-core
```

## DB Flow

```text
Db::open(paths.db_dir)
-> sled database
-> TrustStore::open(&db)
-> trust/peers/v1 tree
-> trust/fp_index/v1 tree
-> transaction upsert/remove
-> list/get/is_trusted/touch operations
```

Only trust is in sled today. Settings are JSON. Identity secret is in OS keyring.

## State Flow

```text
Persistent:
  keyring identity
  sled trusted peers
  settings.json

Rust runtime:
  AppState
  Settings RwLock
  peers_cache RwLock
  pending_prompts Mutex
  TransferManager Mutex
  receiver/discovery handles

React runtime:
  info, peers, trusted, transfers, prompts
  outgoingSas, tab, sendTarget, sendPaths, toast
```

---

# Phase 2 - Folder Structure Analysis

## Root

Purpose: hybrid workspace root. It coordinates Cargo, Yarn, Vite, TypeScript, and Tauri.

What belongs here:

- workspace manifests;
- lockfiles;
- high-level docs;
- TypeScript/Vite config;
- project-level build config.

What does not belong here:

- transfer logic;
- UI components;
- OS integration implementation;
- generated build output.

Pattern: polyglot workspace root.

Classification: core infrastructure.

## `src/`

Purpose: React frontend.

Responsibility: show device/transfer/trust state, collect user intent, call Tauri commands, listen for Tauri events.

What belongs here:

- UI components;
- UI-only state;
- formatting helpers;
- CSS;
- TypeScript payload mirrors.

What does not belong here:

- crypto;
- LAN networking;
- filesystem writes;
- trust decisions;
- protocol validation.

Pattern: thin renderer.

Classification: feature-level UI.

Technical debt: all UI currently lives in one `App.tsx`.

## `src/assets/`

Purpose: frontend static/imported assets.

Classification: reusable UI asset storage.

Current role: minimal.

## `src-tauri/`

Purpose: native desktop shell.

Responsibility: app lifecycle, Tauri plugins, commands, events, tray, context menu, managed state, core adapter code.

What belongs here:

- Tauri command handlers;
- UI-facing DTOs;
- tray/window behavior;
- plugin setup;
- adapters from core callbacks to Tauri events.

What does not belong here:

- reusable protocol logic;
- cryptographic primitives;
- receiver path safety rules;
- React components.

Pattern: application shell / adapter layer.

Classification: core infrastructure and desktop integration.

Danger level: high.

## `src-tauri/capabilities/`

Purpose: Tauri v2 permission grants.

Responsibility: define which native APIs the main window can use.

Pattern: security boundary.

Classification: security infrastructure.

Rule: add least privilege, not broad permissions.

## `src-tauri/icons/`

Purpose: native app icons for bundle/tray.

Classification: packaging infrastructure.

## `crates/`

Purpose: Rust workspace crates.

Current crate: `quickdrop-core`.

Pattern: reusable domain library separated from Tauri.

Classification: business logic and reusable abstractions.

## `crates/quickdrop-core/`

Purpose: platform-agnostic QuickDrop engine.

Responsibility: config, logging, identity, trust, discovery, transport, protocol, sender, receiver, file safety.

What belongs here:

- code that should be testable without a webview;
- device-to-device protocol;
- file safety invariants;
- identity/trust rules;
- discovery and transfer engines.

What does not belong here:

- Tauri command names;
- tray behavior;
- React UI state;
- plugin-specific code.

Pattern: core domain library with host callbacks.

Classification: business logic and core infrastructure.

## `crates/quickdrop-core/src/transfer/`

Purpose: transfer engine and protocol.

Responsibility: wire messages, handshake, sender, receiver, hashing, transfer state, cancellation.

Pattern: protocol engine.

Classification: most important business logic.

## `crates/quickdrop-core/src/os/`

Purpose: intended OS abstraction layer.

Current state: mostly placeholder. Real Windows context menu implementation currently lives in `src-tauri/src/context_menu.rs`.

Classification: technical debt / future abstraction.

## `crates/quickdrop-core/tests/`

Purpose: integration tests for the core engine.

Responsibility: validate sender/receiver behavior without Tauri.

Classification: test infrastructure.

## `public/`

Purpose: static files served by Vite.

Classification: frontend static assets.

## `target/`

Purpose: Cargo build output.

Do not hand-edit.

Classification: generated artifact.

## `node_modules/`

Purpose: installed frontend dependencies.

Do not hand-edit.

Classification: generated dependency artifact.

---

# Phase 3 - File-By-File Deep Analysis

## `Cargo.toml`

Why it exists: Rust workspace root and shared dependency versions.

When it executes: Cargo build/test/check and Tauri build.

Who calls it: Cargo, Tauri CLI, rust-analyzer.

Inputs: member crates and dependency declarations.

Outputs: resolved Rust dependency graph and build profiles.

Security implications: this controls versions/features for crypto, TLS, keyring, networking, and serialization. Upgrade carefully.

Performance implications: release profile uses optimized LTO settings and `panic = "abort"` for a smaller production binary.

Common mistakes: adding dependency versions in child crates instead of workspace dependencies, or changing rustls/tokio features without checking downstream code.

Senior insight: central dependency management is a good monolith practice.

## `package.json`

Why it exists: frontend dependencies and scripts.

When it executes: Yarn install/build/dev and Tauri dev/build.

Important scripts:

```text
yarn dev     -> Vite dev server
yarn build   -> TypeScript check + Vite build
yarn tauri   -> Tauri CLI wrapper
```

Security implications: frontend dependencies run in the webview context. Keep the dependency surface small.

Senior insight: the frontend dependency list is intentionally light. Do not add a web framework for work Rust should own.

## `vite.config.ts`

Why it exists: Vite config tuned for Tauri.

Important decisions:

- fixed port `1420` matches `tauri.conf.json`;
- `strictPort` fails fast if the port is occupied;
- `clearScreen: false` keeps Rust errors visible;
- `TAURI_DEV_HOST` supports external testing;
- Vite ignores `src-tauri` file watching.

Danger: changing the dev port requires changing Tauri config.

## `README.md`

Why it exists: setup and roadmap notes.

Important caveat: it is stale. Current source has progressed far beyond the stated scaffold step.

Senior insight: update it before using it as onboarding truth.

## `index.html`

Why it exists: Vite HTML entry.

When it executes: webview load.

Important block:

```text
#root element
-> /src/main.tsx module
```

Common mistake: adding app logic here.

## `src/main.tsx`

Why it exists: React root mount.

When it executes: once when the webview loads.

Inputs: DOM element `root`.

Outputs: renders `<App />` inside React StrictMode.

Common mistake: adding business logic here.

Senior insight: React StrictMode can double-run effects in development, so listener cleanup must be correct.

## `src/App.tsx`

Why it exists: entire frontend application.

When it executes: after React mounts and on each state change.

Who calls it: React root.

Inputs: user actions, Tauri command responses, Tauri event payloads, native dialog paths.

Outputs: commands to Rust, local state updates, UI rendering.

Side effects: registers listeners, opens file dialog, invokes commands, shows toasts.

State it owns:

- `info`: local app/device metadata.
- `peers`: discovered devices.
- `trusted`: persisted trusted peers.
- `transfers`: current transfer rows.
- `prompts`: incoming pair/transfer prompts.
- `outgoingSas`: pairing code for outgoing pairing.
- `tab`: selected tab.
- `sendTarget` and `sendPaths`: pending outbound send state.
- `toast`: transient message.

Command interactions:

```text
app_info
list_peers
list_transfers
list_trusted_peers
forget_peer
pair_with
send_files
answer_prompt
cancel_transfer
```

Event interactions:

```text
peers://updated
transfers://updated
transfers://error
transfers://received
prompt://incoming
pairing://sas
pairing://done
send://files
```

Block-by-block:

```text
Imports
-> React hooks, Tauri APIs, dialog plugin, CSS.

Type definitions
-> manual TypeScript mirrors of Rust payloads.

fmtBytes/fmtRate
-> UI-only formatting.

State declarations
-> display snapshots and local interaction state.

refreshTrusted
-> re-queries trusted peers after trust-affecting flows.

useEffect
-> initial snapshot load and listener registration.

Handlers
-> convert user actions into invoke() calls.

Render tree
-> topbar, tabs, active pane, footer, modals, toast.

Child components
-> presentational panes and modal wrappers.
```

Security implications: this file displays fingerprints/SAS and collects user consent, but it must not make trust decisions itself.

Performance implications: full snapshots are fine at LAN scale. Transfer list sorting uses `useMemo`.

Hidden coupling: TypeScript shapes must match Rust serde output. Event strings must match Rust emissions.

Common mistakes: treating React state as authoritative, forgetting listener cleanup, renaming commands/events on only one side, putting core validation in UI.

Refactor opportunities: split panes/components, create a Tauri API wrapper, centralize toast logic, generate shared types later.

## `src/App.css`

Why it exists: application styling.

Side effects: visual layout only.

Current issue: Step 6 CSS additions are duplicated, including classes like `.send-bar`, `.btn`, `.xfer`, `.modal`, and `.toast`.

Common mistake: changing a class in one duplicated section and wondering why behavior is inconsistent.

Senior insight: CSS bugs can become product bugs when prompts or SAS codes are unclear.

## `src-tauri/src/main.rs`

Why it exists: native process entry point.

When it executes: app launch.

Output: calls `quickdrop_lib::run()`.

Important behavior: release builds use `windows_subsystem = "windows"` to avoid showing a console window.

Common mistake: putting logic here instead of `lib.rs`.

## `src-tauri/src/lib.rs`

Why it exists: the desktop app orchestrator.

When it executes: startup, command invocations, background task callbacks, tray actions.

Who calls it: `main.rs`, React IPC, Tauri runtime, core receiver callbacks.

Inputs: CLI args, React commands, core callbacks, settings, DB, identity, discovery, transfer progress.

Outputs: app window/tray, Tauri events, spawned tasks, command results.

Side effects: creates dirs, opens sled, loads keyring identity, starts receiver/discovery, writes registry for context menu, opens destination folder.

Block-by-block:

```text
AppState
-> long-lived runtime state shared by commands and callbacks.

PromptReply
-> wraps oneshot senders for pair/transfer prompt answers.

AppInfo and TrustedPeerView
-> UI-facing DTOs.

Tauri commands
-> command API used by React.

PromptPayload
-> event payload for prompt://incoming.

TauriHost
-> implements ReceiverHost and turns core callbacks into UI events.

run()
-> process setup, plugin registration, command registration, setup hook.

bootstrap()
-> starts receiver, then discovery, then peer event pump.

handle_send_argv()
-> forwards --send paths to React.

build_tray()
-> tray menu and click behavior.
```

Important command details:

```text
app_info
-> returns version, device name, ID, fingerprint, destination, app_data.

list_peers
-> returns in-memory peer cache.

send_files
-> resolves peer from cache
-> builds SenderConfig
-> converts paths to SendItem
-> builds manifest once (UI totals + wire protocol share it)
-> registers TransferManager row with manifest.transfer_id
-> spawns sender::send_prepared
-> emits transfers://updated.

pair_with
-> resolves peer from cache
-> calls sender::pair_with
-> emits pairing://sas and pairing://done.

answer_prompt
-> removes prompt from pending_prompts
-> sends decision through oneshot channel.
```

Security implications: this is the command/event boundary. Do not expose broad native capabilities or bypass core validation here.

Performance implications: `send_files` now builds the manifest once and passes it to `sender::send_prepared`, so each file is hashed a single time and the UI transfer ID matches the wire transfer ID.

Common mistakes: changing event names, forgetting to focus/show window for prompts, holding sync locks across awaits, treating `peers_cache` as always fresh.

Refactor opportunities: split into `commands`, `tray`, `host`, and `bootstrap` modules; emit prompt timeout cleanup; wire settings commands.

## `src-tauri/src/context_menu.rs`

Why it exists: install/uninstall Windows Explorer context menu entries.

When it executes: through `install_context_menu` or `uninstall_context_menu` commands.

Side effects: writes/removes HKCU registry keys for file and directory shell menu entries.

Security implications: per-user HKCU avoids admin rights. Command strings must quote executable and selected path.

Common mistake: using HKLM, forgetting directories, or failing to quote paths.

## `src-tauri/Cargo.toml`

Why it exists: Tauri shell crate manifest.

Responsibility: depends on `quickdrop-core`, Tauri plugins, and Windows-only `winreg`.

Common mistake: adding core-domain dependencies here instead of to `quickdrop-core`.

## `src-tauri/tauri.conf.json`

Why it exists: Tauri app and packaging config.

Important fields: product name, identifier, dev URL, frontend dist, window sizes, tray icon, bundle icons, MSI settings.

Security implication: `csp` is `null`. That is acceptable only while the app does not render untrusted remote content.

Common mistake: changing dev URL without changing Vite config.

## `src-tauri/capabilities/default.json`

Why it exists: Tauri permission model for the main window.

Responsibility: grants dialog, opener, notification, autostart, window, and event permissions.

Danger: over-granting increases damage if UI content is compromised.

## `crates/quickdrop-core/src/lib.rs`

Why it exists: core crate module map and policy.

Important declarations: `forbid(unsafe_code)`, warnings for Rust idioms and missing debug implementations, module exports, `VERSION`.

Common mistake: adding implementation logic here instead of modules.

## `config.rs`

Why it exists: runtime paths and user settings.

When it executes: startup.

Inputs: OS directory APIs, env vars for device name, settings JSON.

Outputs: `Paths` and `Settings`.

Side effects: creates app data, DB, log, and default destination directories.

Security implications: destination path decides where received files land.

Common mistakes: hardcoding `%APPDATA%`, storing large state in JSON, assuming settings changes apply live everywhere.

Senior insight: this centralizes filesystem locations so other modules do not scatter path assumptions.

## `db.rs`

Why it exists: thin sled database wrapper.

When it executes: startup.

Current user: TrustStore.

Senior insight: small now, useful migration boundary later.

## `error.rs`

Why it exists: unified core error type.

Categories: IO, serde, DB, discovery, transport, protocol, integrity, peer rejected, not trusted, cancelled, not found, config, internal.

Security implications: protocol/integrity errors should fail closed. Avoid leaking secrets in error strings that reach UI.

Common mistake: overusing `Internal` for expected network/user errors.

## `identity.rs`

Why it exists: stable local device identity.

When it executes: startup and handshake/discovery/pairing use.

Inputs: OS keyring or RNG.

Outputs: device UUID, fingerprint, public identity, Ed25519 signatures.

Side effects: creates/reads/deletes OS credential.

Security implications: secret key stays out of normal disk files. Changing keyring constants rotates identity and breaks old trust.

Block-by-block:

```text
KeyStore trait
-> production and test storage abstraction.

KeyringStore
-> OS credential manager adapter.

MemoryKeyStore
-> test implementation.

Fingerprint
-> 16-byte public-key-derived visual identifier.

PublicIdentity
-> network-safe identity representation.

DeviceIdentity
-> secret signing key plus public identity helpers.

load_or_create_with
-> stable key load or first-run generation.

verify
-> peer signature verification helper.
```

Common mistakes: logging secret material, regenerating identity silently on keyring failures, trusting discovery fingerprint before handshake.

## `pairing.rs`

Why it exists: trusted peer persistence and SAS pairing code.

When it executes: pairing, trust lookup, trusted peer listing, discovery trusted flag checks.

DB interactions: sled trees `trust/peers/v1` and `trust/fp_index/v1`.

Security implications: upsert/remove use transactions to keep primary peer records and fingerprint index in sync.

Block-by-block:

```text
TrustedPeer
-> persisted trust record.

TrustStore
-> get/upsert/remove/list/touch/is_trusted.

compute_sas
-> symmetric 6-digit code from both public keys and nonce.
```

Common mistakes: changing tree names without migration, updating one sled tree without the other, trusting a peer before user-confirmed pairing.

## `discovery.rs`

Why it exists: peer discovery and liveness.

When it executes: after receiver listener starts.

Inputs: identity, receiver port, device metadata, trust lookup closure.

Outputs: watch channel snapshots of `Peer` lists.

Side effects: mDNS register/browse, UDP broadcast/listen, stale peer sweeper tasks.

Important constants:

```text
SERVICE_TYPE      = _quickdrop._tcp.local.
UDP_BEACON_PORT   = 54545
UDP_BEACON_INTERVAL = 3 seconds
PEER_TTL_MS       = 10 seconds
```

Security implications: discovery is unauthenticated. It is a hint, not proof. The transfer handshake proves identity.

Block-by-block:

```text
OsKind / DeviceType
-> metadata enums.

Peer
-> discovered peer snapshot.

UdpBeacon
-> fallback JSON announcement.

PeerTable
-> map of peers plus watch sender.

DiscoveryService::start
-> mDNS register/browser, UDP sender/listener, sweeper.

peer_from_mdns
-> parse mDNS TXT into Peer.

bind_udp_broadcast
-> socket options for broadcast/reuse.
```

Common mistakes: making security decisions from discovery, forgetting to ignore local ID, assuming mDNS works everywhere.

## `files.rs`

Why it exists: receiver-side file safety.

When it executes: every incoming transfer before prompting and during finalization.

Inputs: untrusted remote `rel_path`, destination root, sort setting, existing filesystem.

Outputs: safe relative paths, destination dirs, unique final file paths, finalized files.

Security implications: this is a hard boundary. It blocks path traversal, absolute paths, drive prefixes, reserved Windows names, illegal chars, and overwrite behavior.

Block-by-block:

```text
category_for_ext
-> Images/Videos/Documents/Archives mapping.

sanitize_rel_path
-> rejects dangerous remote paths.

sanitize_segment
-> cleans or rejects individual path components.

resolve_dest
-> category sorting or preserving subfolders.

unique_dest
-> no overwrites; appends (1), (2), etc.

finalize_part
-> rename .qdpart to final path.
```

Common mistakes: validating after prompting, allowing `..`, overwriting existing files, finalizing before hash checks.

## `logging.rs`

Why it exists: tracing setup.

When it executes: startup.

Outputs: rolling file logs and debug stderr logs.

Important: `LogGuard` must live for process lifetime, so it is stored in `AppState`.

Common mistake: logging secrets or noisy per-chunk data.

## `transport.rs`

Why it exists: TLS setup and MessagePack framing.

When it executes: receiver startup, sender connection, every protocol message.

Security model:

```text
TLS -> confidentiality/integrity for channel
handshake.rs -> device authentication
TrustStore/UI prompt -> authorization
```

Important design: TLS accepts any certificate because app-level Ed25519 authentication is the real identity check.

Framing:

```text
4-byte big-endian length
-> MessagePack body
```

Common mistakes: using transport without handshake, raising frame limits carelessly, trying to enforce public CA validation for LAN peers.

## `os/mod.rs` and `os/windows.rs`

Why they exist: intended core OS abstraction.

Current state: placeholder. Real context menu logic is in Tauri shell.

Senior insight: treat as future abstraction or cleanup candidate.

## `transfer/mod.rs`

Why it exists: transfer module map and UI progress types.

Key types: `TransferProgress`, `Direction`, `TransferState`.

Common mistake: renaming enum variants without checking frontend string comparisons.

## `transfer/protocol.rs`

Why it exists: wire protocol schema.

When it executes: serialization/deserialization over TLS.

Important types:

```text
Hello
Auth
Request::Send / Request::Pair
Response::Accept / PairingAccepted / Reject
Manifest / ManifestItem
FileStart / FileEnd / TransferEnd
TransferStatus
```

Security implications: `Hello` carries public identity, `ManifestItem.rel_path` is untrusted, `blake3_hex` is integrity metadata.

Common mistakes: renaming/removing fields without a protocol version plan, changing enum variants without updating both sender and receiver.

Senior insight: protocol files are like database schemas. Treat changes as migrations.

## `transfer/handshake.rs`

Why it exists: application-level peer authentication.

When it executes: immediately after TLS connect/accept.

Flow:

```text
generate nonce
-> send Hello
-> read peer Hello
-> check protocol version
-> verify fingerprint matches peer public key
-> sign peer nonce with AUTH_DOMAIN prefix
-> send Auth
-> read peer Auth
-> verify peer signature over our nonce
-> return PeerHandshake
```

Security implications: this is what makes accepting arbitrary TLS certificates safe.

Common mistakes: changing message order and causing deadlock, removing domain separation, trusting discovery identity instead of handshake identity.

## `transfer/hash.rs`

Why it exists: streaming BLAKE3 file hashing.

When it executes: manifest building.

Performance implication: large files are read before transfer starts.

Common mistake: reading whole files into memory.

## `transfer/manager.rs`

Why it exists: active transfer registry and cancellation handles.

When it executes: transfer register, progress, cancel, finish.

State: `HashMap<Uuid, Entry>` behind a sync `Mutex`.

Outputs: full transfer snapshots and cancel `AtomicBool`s.

Performance implications: sync API avoids async overhead in hot callbacks. Full snapshots are fine at small transfer counts.

Common mistakes: expecting cancellation to stop instantly, never cleaning old completed transfers, registering receive transfers without totals.

Refactor opportunity: use the watch receiver for Tauri event emission and periodically call cleanup.

## `transfer/sender.rs`

Why it exists: outbound transfer and pairing initiator.

When it executes: `send_files` and `pair_with` commands.

Inputs: peer address, config, identity, paths, progress callback, cancel flag.

Outputs: network frames, progress callbacks, trusted peer upsert after pairing.

Block-by-block:

```text
SendItem
-> file or directory path.

SenderConfig
-> device metadata and TrustStore.

build_manifest
-> walk inputs, hash every file, build Manifest and local path list.

send_prepared
-> TCP connect
-> TLS connect
-> app handshake
-> send Request::Send (caller-supplied manifest)
-> read Accept offsets
-> FileStart/chunks/FileEnd per file
-> TransferEnd Completed.

send_to
-> build_manifest then delegate to send_prepared (convenience wrapper for tests).

pair_with
-> connect/handshake
-> compute SAS
-> send Request::Pair
-> persist trusted peer on PairingAccepted.
```

Security implications: checks receiver resume offsets are not beyond file size.

Performance implications: each file is hashed once. The Tauri flow builds the manifest a single time and streams it via `send_prepared`.

Note: the UI transfer registration and the wire protocol now share one manifest (one `transfer_id`), so there is no ID mismatch or duplicated hashing.

## `transfer/receiver.rs`

Why it exists: inbound transfer server and receive-side security.

When it executes: receiver listener starts during bootstrap; connection handlers run per peer.

Inputs: TCP streams, TLS, handshake data, Request frames, settings, trust store, host callbacks.

Outputs: files, progress callbacks, prompt callbacks, trust updates, transfer-end callbacks.

Block-by-block:

```text
AcceptDecision / PairDecision
-> host authorization results.

ReceiverHost trait
-> core asks host/UI for prompts without depending on Tauri.

ReceiverConfig
-> device metadata, trust, settings, destination.

start
-> bind random TCP port, create TLS acceptor, spawn accept loop.

handle_connection
-> TLS accept, handshake, route Request.

handle_pair
-> compute SAS, ask host, persist trust, respond.

handle_send
-> validate manifest
-> snapshot settings
-> sanitize all paths before prompt
-> authorize trusted/prompt
-> compute resume offsets
-> receive chunks into .qdpart
-> verify hash/size
-> finalize files
-> notify host.
```

Security implications: this is the most sensitive file. It handles untrusted network input and writes to disk.

Protections:

- handshake before request;
- manifest sanity checks;
- path sanitization before prompt;
- trusted auto-accept only after TrustStore check;
- `.qdpart` partial files;
- stream/full hash checks;
- out-of-order frame checks.

Dangerous current issue: resumed `.qdpart` prefixes are not rehashed before finalization. A corrupt existing prefix could survive if only the resumed suffix is checked.

Refactor opportunities: verify partial prefixes, add receive cancellation, register receive totals, add more integration tests.

## `crates/quickdrop-core/tests/transfer_roundtrip.rs`

Why it exists: end-to-end core transfer test without Tauri.

What it tests: localhost receiver, sender transfer, file bytes, BLAKE3 hash, TransferManager snapshot publishing.

Important patterns: in-memory key store, temp dirs, random listener port, AcceptAllHost.

Needed future tests: pairing, rejection, malicious paths, resume integrity, cancellation, directory transfer.

---

# Phase 4 - Execution Flow Walkthroughs

## App Launch

```text
OS launches app
-> main.rs
-> quickdrop_lib::run()
-> Paths::resolve
-> logging::init
-> Settings::load_or_default
-> Db::open
-> DeviceIdentity::load_or_create
-> TrustStore::open
-> TransferManager::new
-> AppState created
-> Tauri plugins and commands registered
-> setup builds tray and spawns bootstrap
-> React webview loads
```

## Page Load

```text
React App mounts
-> initial invoke app_info/list_peers/list_transfers/list_trusted_peers
-> register event listeners
-> snapshots arrive
-> state updates
-> active tab renders
```

## Discovery Update

```text
Remote advertisement received by mDNS or UDP
-> parse into Peer
-> PeerTable.upsert
-> watch channel publishes sorted alive list
-> bootstrap peer pump updates peers_cache
-> emit peers://updated
-> React setPeers
-> Devices tab rerenders
```

## Send Files

```text
User chooses files
-> sendPaths set
-> user picks peer
-> send confirmation modal
-> invoke send_files
-> resolve peer from peers_cache
-> build SenderConfig
-> build manifest once
-> TransferManager.register
-> spawn sender::send_prepared
-> emit transfers://updated
-> React switches to Transfers tab
```

Sender task:

```text
sender::send_prepared
-> TCP connect to peer address
-> TLS connect
-> handshake::perform
-> Request::Send (pre-built manifest)
-> Response::Accept offsets
-> FileStart/chunks/FileEnd for each file
-> TransferEnd Completed
-> Tauri marks transfer Completed
```

Receiver side:

```text
listener accept
-> TLS accept
-> handshake::perform
-> Request::Send
-> validate manifest
-> sanitize paths
-> trust or prompt decision
-> Response::Accept offsets
-> receive chunks into .qdpart
-> verify hash/size
-> rename to final path
-> TransferEnd
-> emit transfers://received and transfers://updated
```

## Incoming Transfer Prompt

```text
receiver needs user decision
-> TauriHost::on_transfer_request
-> create prompt_id and oneshot channel
-> store in pending_prompts
-> emit prompt://incoming
-> focus main window
-> wait up to 120 seconds
-> React PromptModal answer
-> invoke answer_prompt
-> send AcceptDecision/Reject through oneshot
-> receiver writes Accept or Reject response
```

## Pairing Initiated Locally

```text
User clicks Pair
-> invoke pair_with
-> resolve peer
-> sender::pair_with
-> TCP/TLS/handshake
-> generate SAS nonce
-> compute SAS
-> emit pairing://sas
-> Request::Pair
-> receiver computes same SAS and prompts user
-> receiver accepts and stores trust
-> Response::PairingAccepted
-> sender stores trust
-> emit pairing://done
-> React refreshes trusted list
```

## Context Menu Send

```text
Explorer launches quickdrop.exe --send "path"
-> single-instance plugin forwards argv if app is running
-> handle_send_argv
-> emit send://files
-> React stores sendPaths and switches to Devices
-> user selects target
-> normal send flow
```

## Cancel Transfer

```text
User clicks Cancel
-> invoke cancel_transfer
-> TransferManager.cancel sets AtomicBool
-> sender loop checks flag
-> writes TransferEnd Cancelled if possible
-> returns Error::Cancelled
-> Tauri marks transfer Cancelled
-> emits transfers://updated
```

Cancellation is cooperative, so it stops between chunks/files, not necessarily in the middle of an OS write.

## Forget Peer

```text
Trusted tab Forget
-> invoke forget_peer
-> parse UUID
-> TrustStore.remove transaction
-> refreshTrusted
-> UI updates
```

## Error Flow

```text
core returns Err(e)
-> Tauri task logs warning
-> map Cancelled vs Failed
-> manager.finish
-> emit transfers://updated
-> emit transfers://error
-> React toast
```

---

# Phase 5 - System Design And Engineering Thinking

## Scaling Strategy

This app scales as a local desktop process, not as cloud infrastructure.

Expected scale:

- small peer count on a LAN;
- small number of simultaneous transfers;
- potentially huge files;
- potentially large directories;
- LAN and disk throughput as bottlenecks.

Good choices:

- direct peer-to-peer transfer avoids server bandwidth;
- file bytes stream instead of loading into memory;
- BLAKE3 is fast;
- full UI snapshots are simple and fine at current scale.

Scaling risks:

- huge manifests built fully before transfer;
- all files hashed before sending;
- current double hashing in send flow;
- no incoming connection concurrency limit;
- receiver allocates per chunk;
- completed transfers are not actively cleaned up.

## Caching Strategy

There is no heavy cache layer. Caches are small snapshots:

- `peers_cache` for command-time peer lookup;
- TransferManager snapshot for UI;
- React copies for rendering;
- TrustStore fingerprint index for persistent lookup.

This is appropriate because state is local and small.

## State Management Philosophy

Rust owns authoritative state. React owns display state.

```text
Rust = source of truth
React = latest visible snapshot + local interaction state
```

This prevents the UI from becoming responsible for network, trust, or filesystem correctness.

## Security Model

Layers:

```text
OS keyring protects identity secret
-> TLS encrypts transport bytes
-> Ed25519 handshake authenticates peer identity
-> TrustStore/SAS/UI prompt authorizes peer actions
-> path sanitizer protects filesystem
-> BLAKE3 verifies file integrity
-> Tauri capabilities restrict frontend native access
```

Current security concerns:

- resume prefix is not fully verified;
- CSP is disabled;
- no rate limiting for incoming connections;
- prompt timeout does not clear frontend modal;
- no explicit firewall/network diagnostics.

## Performance Bottlenecks

- manifest hashing before transfer;
- duplicate manifest hashing;
- large directory traversal;
- disk read/write speed;
- network throughput;
- per-chunk allocation;
- frequent progress events.

## Modularity Strategy

Original developer intent is clear:

```text
quickdrop-core = what QuickDrop means
src-tauri      = how QuickDrop lives as a desktop app
src            = how users see and control it
```

That is a strong architecture. Preserve it.

## Dependency Strategy

Dependencies are purpose-specific and fairly minimal. Keep it that way. A desktop app pays for every dependency in package size, build time, and attack surface.

## Error Handling Philosophy

Core returns typed `Error`. Tauri converts errors to strings for the UI. Background tasks log and emit failure events.

Good: protocol/integrity failures fail closed.

Needs improvement: frontend error states are mostly toasts and console logs.

## Logging Strategy

Logs go to `%APPDATA%\QuickDrop\logs\quickdrop.log` on Windows through rolling file appenders. Debug builds also log to stderr.

Useful dev command:

```powershell
$env:QUICKDROP_LOG = "debug,sled=warn"
yarn tauri dev
```

## Monitoring Strategy

There is no remote telemetry. That matches the privacy/local-first nature of the app. Future local diagnostics would be valuable: discovery source, receiver port, last peer error, export logs.

## Deployment Assumptions

- Windows-first.
- WebView2 available.
- Rust stable MSVC toolchain.
- Node 20+ and Yarn classic.
- Tauri MSI packaging.
- Runtime data in app data.
- Default receive folder is `C:\QuickDrop` on Windows.

## Technical Debt

- README is stale.
- `src-tauri/src/lib.rs` is doing many jobs.
- `src/App.tsx` is monolithic.
- CSS has duplicated sections.
- send flow double-builds manifests.
- UI transfer ID can differ from protocol transfer ID.
- resume integrity is incomplete.
- receive progress totals are not registered.
- settings fields exist without settings UI.
- prompt timeout does not notify frontend.
- no incoming transfer rate/concurrency limits.
- `os/windows.rs` is placeholder while real context menu lives in Tauri shell.

## What Is Elegant

- Core crate is independent from Tauri.
- ReceiverHost trait keeps UI out of core.
- mDNS plus UDP fallback is practical.
- TrustStore has a fingerprint index.
- Path validation happens before prompting.
- Secret identity lives in OS keyring.
- `.qdpart` plus atomic rename avoids exposing partial files.

## What Is Dangerous

- Weakening path sanitization.
- Using transport without handshake.
- Changing protocol structs casually.
- Changing identity derivation/keyring constants.
- Leaving resume prefix unverified.
- Keeping duplicate manifest build as transfer history grows.

## What Senior Engineers Would Improve First

1. Build manifest once and use one transfer ID.
2. Verify resumed partial prefixes before finalizing.
3. Register receive progress with real totals.
4. Add integration tests for resume, cancellation, pairing, prompt rejection, and malicious paths.
5. Split `src-tauri/src/lib.rs` into focused modules.
6. Split `App.tsx` and clean duplicated CSS.
7. Add local diagnostics for discovery and receiver state.

---

# Phase 6 - Learning Mode

## Stage 1 - Easy Concepts

Study:

- `src/main.tsx`
- `src/App.tsx`
- `src/App.css`

Learn:

- tabs and panes;
- `invoke()` calls;
- `listen()` event handlers;
- prompt modals;
- transfer rendering.

Ignore for now:

- crypto;
- TLS;
- sled transactions;
- socket options.

## Stage 2 - Intermediate Concepts

Study:

- `src-tauri/src/lib.rs`
- `config.rs`
- `logging.rs`
- `db.rs`
- `transfer/manager.rs`

Learn:

- `AppState`;
- startup order;
- receiver before discovery;
- peer cache;
- pending prompts;
- transfer manager;
- tray and context menu flow.

Patterns:

- `Arc` for shared ownership;
- `RwLock` for settings/cache;
- `Mutex` for maps;
- `tokio::spawn` for background tasks.

## Stage 3 - Advanced Concepts

Study:

- `identity.rs`
- `pairing.rs`
- `transport.rs`
- `transfer/protocol.rs`
- `transfer/handshake.rs`
- `files.rs`

Learn:

- identity lifecycle;
- fingerprint derivation;
- SAS pairing;
- trust persistence;
- TLS vs app authentication;
- path traversal defense;
- MessagePack framing.

## Stage 4 - Architecture Mastery

Study:

- `transfer/sender.rs`
- `transfer/receiver.rs`
- `discovery.rs`
- `transfer_roundtrip.rs`
- full `src-tauri/src/lib.rs`

Practice tracing:

- app launch;
- peer discovered;
- pair initiated;
- incoming pair accepted;
- send file;
- receive file;
- cancel transfer;
- forget peer;
- context menu send.

Ask these senior questions:

- What input is untrusted?
- Who owns the source of truth?
- What state is persistent?
- What happens if the app crashes here?
- What happens if the network drops here?
- What happens if two app versions talk?
- What happens if two transfers race?

---

# Phase 7 - Contributor Mode

## How To Approach A Task

Start from the user-visible behavior and trace inward.

```text
UI bug
-> App.tsx
-> command/event contract
-> src-tauri/src/lib.rs
-> core module if needed

Transfer bug
-> sender.rs / receiver.rs
-> protocol.rs
-> transport.rs / handshake.rs
-> Tauri event bridge

Discovery bug
-> discovery.rs
-> bootstrap peer pump
-> peers_cache
-> App.tsx devices pane

Trust/pairing bug
-> identity.rs
-> pairing.rs
-> handshake.rs
-> sender::pair_with / receiver::handle_pair
-> prompt UI

File safety bug
-> files.rs
-> receiver::handle_send
-> tests
```

## Where To Look First

- UI action not working: `App.tsx` handler and Tauri command.
- UI not updating: Rust event emission and frontend listener.
- Peer missing: `discovery.rs`, bootstrap peer pump, firewall/VPN.
- Send fails before progress: peer cache, manifest build, TCP/TLS/handshake.
- Send fails after progress: sender stream or receiver validation.
- Receive destination issue: `files.rs` and `receiver.rs`.
- Pairing issue: `sender::pair_with`, `receiver::handle_pair`, `pairing.rs`.
- Trust list issue: `TrustStore` and `list_trusted_peers`.

## Debugging Workflow

1. Reproduce with logs enabled.
2. Identify the failing boundary: UI, IPC, app state, discovery, handshake, protocol, filesystem.
3. Find the first expected event/state transition that did not happen.
4. Add temporary structured logs if needed.
5. Add or extend a core test for risky behavior.
6. Make the smallest root-cause fix.

Useful commands:

```powershell
yarn tauri dev
yarn build
cargo test -p quickdrop-core
cargo test
$env:QUICKDROP_LOG = "debug,sled=warn"
```

For LAN debugging, check firewall, VPN, network profile, same subnet, and logs on both devices.

## Tracing Workflow Examples

Send failure:

```text
confirmSend called?
-> invoke send_files reached Rust?
-> peer found in peers_cache?
-> peer has address?
-> manifest built?
-> transfer registered?
-> TCP connect?
-> TLS connect?
-> handshake?
-> Request::Send accepted?
-> chunks streamed?
-> receiver finalized?
```

Device missing:

```text
receiver started?
-> discovery started?
-> mDNS registered?
-> UDP bind succeeded?
-> remote advertisement received?
-> PeerTable.upsert?
-> watch changed?
-> peers_cache updated?
-> peers://updated emitted?
-> React setPeers?
```

## Feature Implementation Workflow

1. Decide ownership: UI, Tauri shell, or core.
2. If it affects trust/protocol/file safety, start in core.
3. Add tests for risky behavior.
4. Expose minimal Tauri command/event payloads.
5. Update React state/UI.
6. Test in Tauri dev.
7. Review security, persistence, and compatibility.

Example settings feature:

```text
config.rs already has Settings
-> add get/update settings commands
-> validate destination path in Rust
-> call Settings::save
-> decide whether receiver sees changes immediately
-> add React settings UI
```

## Refactor Workflow

Safe refactors:

- split `App.tsx` into components;
- split `src-tauri/src/lib.rs` into modules;
- remove CSS duplication;
- centralize frontend command/event names.

Risky refactors:

- protocol structs;
- identity derivation;
- trust schema;
- path sanitizer;
- receiver streaming loop;
- transport verifier behavior.

Before risky refactors, add tests and think about compatibility.

## Production-Safe Checklist

Ask:

- Does this affect trusted peers?
- Does this affect where files are written?
- Does this affect protocol compatibility?
- Does this expose new Tauri permissions?
- Does this handle network failure and cancellation?
- Does this preserve partial-file safety?
- Does this avoid logging secrets or file contents?
- Does this preserve Windows behavior?

Minimum verification for core transfer changes:

```powershell
cargo test -p quickdrop-core
yarn build
yarn tauri dev
```

Manual checks:

- launch app;
- discover peer;
- pair;
- send small file;
- send large file;
- cancel transfer;
- receive file;
- forget peer;
- inspect logs.

---

# Phase 8 - Visualization Mode

## Layered Architecture

```text
+--------------------------------------------------------------+
| React UI                                                     |
| tabs, lists, prompts, file picker, toasts                    |
+--------------------------+-----------------------------------+
                           |
                           | Tauri invoke / events
                           v
+--------------------------------------------------------------+
| Tauri Shell                                                  |
| AppState, commands, tray, plugins, core callback bridge      |
+--------------------------+-----------------------------------+
                           |
                           | Rust API calls / callback traits
                           v
+--------------------------------------------------------------+
| quickdrop-core                                               |
| identity, trust, discovery, transport, sender, receiver      |
+--------------------------+-----------------------------------+
                           |
          +----------------+------------------+
          |                                   |
          v                                   v
+--------------------+             +---------------------------+
| Local persistence  |             | LAN peer                  |
| keyring/sled/json  |             | mDNS/UDP/TCP/TLS/files    |
+--------------------+             +---------------------------+
```

## Direct Transfer

```text
Sender Device                              Receiver Device
-------------                              ---------------
choose files
-> send_files
-> sender::send_prepared
-> TCP connect --------------------------> listener accept
-> TLS handshake <-----------------------> TLS accept
-> Hello/Auth <--------------------------> Hello/Auth
-> Request::Send(manifest) -------------> validate, sanitize, authorize
<- Response::Accept(offsets) ------------ accept
-> FileStart ----------------------------> open .qdpart
-> chunks -------------------------------> write bytes
-> FileEnd ------------------------------> verify hash
                                           rename .qdpart
-> TransferEnd --------------------------> complete
<- events update UI ---------------------- progress/completion
```

## Trust And Auth

```text
First run
-> Ed25519 key
-> secret in OS keyring
-> public key
-> fingerprint + UUID

Pairing
-> both devices exchange public keys in handshake
-> SAS = hash(sorted public keys + nonce)
-> users compare 6 digits
-> both accept
-> both persist TrustedPeer

Transfer
-> TLS encrypts
-> Hello/Auth proves device key
-> TrustStore or prompt authorizes receive
```

## File Receive Safety

```text
remote rel_path
-> reject empty/too long/control chars
-> normalize slashes
-> reject absolute path
-> reject ..
-> reject drive/root prefix
-> reject reserved Windows names
-> clean illegal characters
-> resolve destination
-> choose unique filename
-> write .qdpart
-> verify
-> rename final
```

## State Ownership

```text
Persistent authority:
  keyring, sled, settings.json

Runtime authority:
  AppState, PeerTable, TransferManager, pending_prompts

Display state:
  React hooks in App.tsx
```

## Common Web-App Concepts That Are Not Present

```text
HTTP API routes   -> Tauri commands are the API boundary.
Server actions    -> Native Rust commands fill that role.
Middleware        -> Handshake and receiver validation are closest equivalents.
Login auth        -> Device identity and trust are the auth model.
ORM               -> sled key-value trees are used directly.
WebSockets        -> Tauri events handle local app events.
Queues            -> Tokio tasks/channels handle async work.
Cron jobs         -> Discovery sweeper interval is the closest equivalent.
React providers   -> no providers; Tauri AppState is the backend provider.
Custom hooks      -> none yet; built-in hooks only.
Caching layer     -> lightweight peer/progress snapshots only.
```

## Final Mental Model

Think in boundaries:

```text
UI intent vs backend authority
discovery hint vs authenticated identity
authenticated identity vs trusted authorization
remote path string vs safe local path
partial file vs completed file
runtime state vs persistent state
protocol contract vs implementation detail
```

If a change respects these boundaries, it probably fits the architecture. If it blurs them, slow down and design it carefully.
