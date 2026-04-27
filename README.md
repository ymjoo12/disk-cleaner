# macOS Disk Cleaner

Fast parallel disk space analyzer and cleaner for macOS.

## Features

- Parallel directory scanning using all CPU cores
- Cache/log cleanup (npm, pip, Homebrew, Xcode, etc.)
- Project cleanup (node_modules, .venv, __pycache__)
- Docker cleanup (images, containers, volumes, build cache)
- Docker scan is skipped automatically when Docker is unavailable or unresponsive
- Large file detection
- Interactive selection with confirmation
- Dry-run mode

## Installation

```bash
cargo install --path .
```

Or build manually:

```bash
cargo build --release
./target/release/disk-cleaner
```

## Usage

```bash
# Scan only (no cleanup)
disk-cleaner --scan-only

# Preview cleanup (dry run)
disk-cleaner --dry-run

# Interactive cleanup
disk-cleaner

# Custom large file threshold (default: 100MB)
disk-cleaner --large 500
```

## Scan Categories

| Category | Location | Safe to Delete |
|----------|----------|----------------|
| System Caches | ~/Library/Caches | Yes |
| App Logs | ~/Library/Logs | Yes |
| Trash | ~/.Trash | Yes |
| Xcode DerivedData | ~/Library/Developer/Xcode/DerivedData | Yes |
| Xcode Archives | ~/Library/Developer/Xcode/Archives | No |
| iOS Simulators | ~/Library/Developer/CoreSimulator/Devices | No |
| npm/Yarn/pnpm Cache | ~/.npm, ~/.yarn, ~/Library/pnpm | Yes |
| pip/uv Cache | ~/.cache/pip, ~/.cache/uv | Yes |
| Homebrew Cache | ~/Library/Caches/Homebrew | Yes |
| Gradle/Maven Cache | ~/.gradle/caches, ~/.m2/repository | Yes |
| CocoaPods Cache | ~/Library/Caches/CocoaPods | Yes |
| Cargo Cache | ~/.cargo/registry | Yes |
| node_modules | Project directories | Yes |
| Python .venv | Project directories | Yes |
| __pycache__ | Project directories | Yes |
| Docker | Images, containers, volumes | Yes (unused only) |

## Safety

- Categories marked "No" are shown but not selectable for cleanup
- All cleanup requires explicit user selection
- Final confirmation prompt before any deletion (default: No)
- Docker cleanup only removes unused resources
- Docker cleanup options are hidden when Docker is unavailable or unresponsive
