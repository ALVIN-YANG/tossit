# TossIt

TossIt is a lightweight, account-free local-network messenger for iOS, Android,
macOS, and Windows. It is designed for direct text, image, and file sharing,
with group conversations built on persistent device identities rather than cloud
accounts.

The repository is currently at the architecture and cross-platform risk-spike
stage. Product functionality has not been implemented yet.

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
- Android Studio, Android SDK, and NDK for Android targets
- Windows with Microsoft C++ Build Tools for Windows release validation

## Current scaffold commands

```bash
pnpm install
pnpm tauri dev
```
