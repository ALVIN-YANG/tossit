# TossIt

TossIt is an account-free local-network messenger for iPhone and Mac. Devices on
the same Wi-Fi discover each other and exchange text, images, and files directly,
without a cloud relay.

Version 0.2.0 includes Wi-Fi-scoped conversations, cellular/offline history,
mDNS discovery with manual endpoint fallback, mutual verification codes, TLS
1.3 transport, persistent Ed25519 device identity in Apple Keychain, offline
send queues, automatic and manual retry, transfer cancellation, free-space
preflight, SHA-256 verification, bounded image previews, paged history, unread
state, and local received-file cleanup. A Wi-Fi is saved only after its first
successful send or receive, so merely joining a network does not create an empty
conversation space.

The automated verification suite covers the shared Rust core and Svelte UI.
The latest build still needs a final iPhone-to-Mac physical-device exchange.
Public macOS distribution also needs a Developer ID certificate and Apple
notarization credentials. Android, Windows, groups, and the one-off Bluetooth
transfer tool are deferred.

## Architecture and plan

See [Architecture and Implementation Plan](docs/architecture-and-implementation-plan.md).

## Planned stack

- Tauri 2 application shell
- Thin Svelte/TypeScript UI compiled to static assets
- Framework-independent Rust protocol and networking core
- Swift/Kotlin adapters only where mobile operating systems require native APIs

## Development prerequisites

- Rust stable toolchain
- Node.js and pnpm
- Xcode for macOS/iOS targets

## Development commands

```bash
pnpm install
pnpm tauri dev
```

Run the complete local verification suite with:

```bash
pnpm verify
```

This checks the Svelte application, creates a production frontend build, runs
Rust formatting and Clippy with warnings denied, and executes all Rust tests.

## Rust workspace

- `crates/tossit-protocol`: versioned wire envelopes, payloads, and validation.
- `crates/tossit-core`: framework-independent application state.
- `crates/tossit-identity`: Apple Keychain-backed Ed25519 identity, Peer ID, and signing.
- `crates/tossit-network`: mDNS discovery, TLS transport, signed handshakes, text delivery, and streamed attachments.
- `crates/tossit-storage`: SQLite migrations and durable peer/message history.
- `src-tauri`: the thin Tauri adapter exposed to the frontend.
- `src-tauri/gen/apple`: generated iOS project and AppIcon assets.
