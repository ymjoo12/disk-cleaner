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
- Non-interactive cleanup by target ID or group
- Runtime and saved exclude paths
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

# List clean target groups and IDs
disk-cleaner --list-targets

# Preview cleanup without prompts
disk-cleaner --dry-run --clean all

# Clean everything selectable without prompts
disk-cleaner --clean all

# Clean selected groups without prompts
disk-cleaner --clean caches,projects,docker

# Clean selected target IDs without prompts
disk-cleaner --clean system-caches,npm-cache,node-modules

# Interactive cleanup
disk-cleaner

# Custom large file threshold (default: 100MB)
disk-cleaner --large 500

# Exclude paths for one run
disk-cleaner --scan-only --exclude ~/Codes/important --exclude ~/Downloads/keep
```

## Saved Exclude Paths

```bash
# Save an exclude path
disk-cleaner config add-exclude ~/Codes/important

# Remove an exclude path
disk-cleaner config remove-exclude ~/Codes/important

# List saved exclude paths
disk-cleaner config list

# Print config path
disk-cleaner config path
```

The default config path is `~/.config/disk-cleaner/config.json`.

## Clean Targets

Groups:

- `all`
- `caches`
- `projects`
- `docker`

Target IDs:

- `system-caches`
- `app-logs`
- `trash`
- `xcode-deriveddata`
- `npm-cache`
- `yarn-cache`
- `pnpm-cache`
- `pip-cache`
- `uv-cache`
- `homebrew-cache`
- `gradle-cache`
- `maven-cache`
- `cocoapods-cache`
- `cargo-cache`
- `node-modules`
- `python-venvs`
- `pycache`
- `docker-images`
- `docker-containers`
- `docker-build-cache`
- `docker-volumes`

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
- Interactive cleanup requires user selection
- Interactive final confirmation defaults to Yes
- Non-interactive cleanup runs only when `--clean` is provided
- Use `--dry-run --clean <targets>` to preview the exact non-interactive selection
- Permission errors are reported as warnings and do not stop later cleanup targets
- Docker cleanup only removes unused resources
- Docker cleanup options are hidden when Docker is unavailable or unresponsive
