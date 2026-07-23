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
- Verified cold-file archival to S3-compatible storage

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

# Preview configured cold-file archival
disk-cleaner archive --endpoint https://s3.example.com --bucket archives --profile archive --dry-run

# Upload, verify, and remove configured cold files
disk-cleaner archive --endpoint https://s3.example.com --bucket archives --profile archive
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

## Cold-File Archive

Archive policies are stored in the same config file. Existing config files without an `archive` field remain valid.

```json
{
  "exclude": [
    "~/Codes/important"
  ],
  "archive": [
    {
      "path": "~/Movies/renders",
      "prefix": "cold/renders",
      "older_than_days": 30,
      "partition_by_date": true,
      "exclude": [
        "active-project",
        "~/Movies/renders/keep"
      ]
    }
  ]
}
```

Each regular file older than the policy threshold keeps its path relative to the policy root. With `partition_by_date` omitted or set to `false`, `~/Movies/renders/client/final.mp4` is stored as `s3://archives/cold/renders/client/final.mp4`. When `partition_by_date` is `true`, the file modification time is converted to UTC and the key becomes `s3://archives/cold/renders/YYYY/MM/DD/client/final.mp4`. Relative policy excludes are resolved from the policy root. Absolute and `~/` excludes are also supported.

The archive command requires the AWS CLI and an existing AWS CLI profile. It calculates the local SHA-256 and checks the destination with `head-object` before uploading. A missing key is uploaded with the SHA-256 in the `sha256` object metadata field. An existing object is reused without uploading only when both `ContentLength` and `Metadata.sha256` match; a different existing object is never overwritten. Authentication and network failures are not treated as missing objects. The local file is removed only after remote verification and a second local identity check covering device, inode, change time, size, and modification time. Failed files remain local, later files continue processing, and the command exits with a nonzero status after reporting all failures. Only empty ancestor directories created by successful file removal are removed.

Paths containing a `models` or `checkpoints` component are always excluded. Files with the following extensions are also always excluded:

- `.safetensors`
- `.ckpt`
- `.pt`
- `.pth`
- `.bin`
- `.gguf`
- `.onnx`

Archive prefixes must be non-empty relative paths without leading or trailing slashes, backslashes, empty components, `.` components, or `..` components. Archive policy roots must exist, `older_than_days` must be greater than zero, and policy roots must not overlap.

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
- Archive dry-run does not upload or delete files
- Archive verification failure always preserves the local file
- Model paths and model file extensions are always excluded from archival
