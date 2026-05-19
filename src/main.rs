use bytesize::ByteSize;
use chrono::{DateTime, Local};
use clap::{Parser, Subcommand};
use colored::*;
use dialoguer::{theme::ColorfulTheme, Confirm, MultiSelect};
use indicatif::{ProgressBar, ProgressStyle};
use jwalk::WalkDir;
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

const DOCKER_INFO_TIMEOUT: Duration = Duration::from_millis(750);
const DOCKER_SCAN_TIMEOUT: Duration = Duration::from_secs(2);
const DOCKER_PRUNE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Parser, Debug)]
#[command(name = "disk-cleaner")]
#[command(about = "macOS Disk Cleaner - Fast parallel disk space analyzer")]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(short, long, global = true, help = "Path to config file")]
    config: Option<PathBuf>,

    #[arg(short, long, help = "Scan only, don't suggest cleanup")]
    scan_only: bool,

    #[arg(short = 'n', long, help = "Dry run - show what would be deleted")]
    dry_run: bool,

    #[arg(
        long,
        value_name = "TARGETS",
        help = "Clean target IDs or groups without prompts, for example: all,caches,projects,docker"
    )]
    clean: Option<String>,

    #[arg(long, help = "List cleanable target IDs and groups")]
    list_targets: bool,

    #[arg(
        long = "exclude",
        value_name = "PATH",
        help = "Exclude a path for this run; can be used multiple times"
    )]
    excludes: Vec<PathBuf>,

    #[arg(
        short,
        long,
        default_value = "100",
        help = "Large file threshold in MB"
    )]
    large: u64,

    #[arg(short, long, help = "Number of scan threads (default: CPU cores)")]
    threads: Option<usize>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    #[command(about = "Manage saved exclude paths")]
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    #[command(about = "Save an exclude path to the config")]
    AddExclude { path: PathBuf },
    #[command(about = "Remove an exclude path from the config")]
    RemoveExclude { path: PathBuf },
    #[command(about = "List saved exclude paths")]
    List,
    #[command(about = "Print the config path")]
    Path,
}

#[derive(Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
struct Config {
    #[serde(default)]
    exclude: Vec<String>,
}

impl Config {
    fn default_path(home: &Path) -> PathBuf {
        home.join(".config/disk-cleaner/config.json")
    }

    fn path(path: Option<&Path>, home: &Path) -> PathBuf {
        path.map(PathBuf::from)
            .unwrap_or_else(|| Self::default_path(home))
    }

    fn load(path: Option<&Path>, home: &Path) -> io::Result<Self> {
        let config_path = Self::path(path, home);
        match fs::read_to_string(&config_path) {
            Ok(content) => serde_json::from_str::<Config>(&content).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("failed to parse config {}: {error}", config_path.display()),
                )
            }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
            Err(error) => Err(error),
        }
    }

    fn save(&self, path: Option<&Path>, home: &Path) -> io::Result<()> {
        let config_path = Self::path(path, home);
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)
            .map_err(|error| io::Error::other(format!("failed to encode config: {error}")))?;
        fs::write(config_path, format!("{content}\n"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TargetGroup {
    Caches,
    Projects,
    Docker,
}

impl TargetGroup {
    fn id(self) -> &'static str {
        match self {
            TargetGroup::Caches => "caches",
            TargetGroup::Projects => "projects",
            TargetGroup::Docker => "docker",
        }
    }
}

#[derive(Clone)]
struct Category {
    id: &'static str,
    name: &'static str,
    path: PathBuf,
    safe_to_delete: bool,
}

struct LargeFile {
    path: PathBuf,
    size: u64,
    accessed: Option<SystemTime>,
}

struct DockerInfo {
    images: u64,
    containers: u64,
    volumes: u64,
    build_cache: u64,
    available: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct CleanupReport {
    freed: u64,
    errors: Vec<String>,
}

impl CleanupReport {
    fn add_error(&mut self, path: &Path, error: impl std::fmt::Display) {
        self.errors.push(format!("{}: {error}", path.display()));
    }

    fn merge(&mut self, mut other: CleanupReport) {
        self.freed += other.freed;
        self.errors.append(&mut other.errors);
    }
}

type CleanAction = Box<dyn Fn() -> CleanupReport>;

struct CleanableItem {
    id: &'static str,
    group: TargetGroup,
    label: String,
    size: u64,
    action: CleanAction,
}

#[derive(Default)]
struct ScanReport {
    bytes: u64,
    warnings: u64,
}

struct DirectoryScan {
    entries: Vec<(PathBuf, u64)>,
    warnings: u64,
}

struct LargeFileScan {
    files: Vec<LargeFile>,
    warnings: u64,
}

struct ScanResults {
    category_sizes: Vec<(Category, u64)>,
    node_modules: Vec<(PathBuf, u64)>,
    node_modules_total: u64,
    venvs: Vec<(PathBuf, u64)>,
    venvs_total: u64,
    pycaches: Vec<(PathBuf, u64)>,
    pycache_total: u64,
    large_files: Vec<LargeFile>,
    docker: DockerInfo,
    warnings: u64,
}

impl DockerInfo {
    fn unavailable() -> Self {
        Self {
            images: 0,
            containers: 0,
            volumes: 0,
            build_cache: 0,
            available: false,
        }
    }

    fn total(&self) -> u64 {
        self.images + self.containers + self.volumes + self.build_cache
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{} {error}", "Error:".red().bold());
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let args = Args::parse();
    let home = get_home_dir()?;

    if let Some(Commands::Config { command }) = args.command {
        return run_config_command(command, args.config.as_deref(), &home);
    }

    if args.list_targets {
        print_target_reference();
        return Ok(());
    }

    let threads = args.threads.unwrap_or_else(num_cpus::get).max(1);
    let config = Config::load(args.config.as_deref(), &home)?;
    let excludes = merge_excludes(&config, &args.excludes, &home);

    run_cleaner(&args, &home, threads, &excludes)
}

fn run_config_command(
    command: ConfigCommand,
    config_path: Option<&Path>,
    home: &Path,
) -> io::Result<()> {
    let mut config = Config::load(config_path, home)?;

    match command {
        ConfigCommand::AddExclude { path } => {
            let normalized = display_path(&normalize_path(&path, home), home);
            if !config.exclude.iter().any(|existing| {
                normalize_path(Path::new(existing), home)
                    == normalize_path(Path::new(&normalized), home)
            }) {
                config.exclude.push(normalized.clone());
                config.exclude.sort();
                config.save(config_path, home)?;
            }
            println!("Added exclude: {normalized}");
        }
        ConfigCommand::RemoveExclude { path } => {
            let normalized = normalize_path(&path, home);
            let before = config.exclude.len();
            config
                .exclude
                .retain(|existing| normalize_path(Path::new(existing), home) != normalized);
            config.save(config_path, home)?;

            if config.exclude.len() == before {
                println!("Exclude not found: {}", display_path(&normalized, home));
            } else {
                println!("Removed exclude: {}", display_path(&normalized, home));
            }
        }
        ConfigCommand::List => {
            if config.exclude.is_empty() {
                println!("No saved exclude paths.");
            } else {
                for path in &config.exclude {
                    println!("{path}");
                }
            }
        }
        ConfigCommand::Path => {
            println!("{}", Config::path(config_path, home).display());
        }
    }

    Ok(())
}

fn run_cleaner(args: &Args, home: &Path, threads: usize, excludes: &[PathBuf]) -> io::Result<()> {
    let results = collect_scan_results(args, home, threads, excludes);

    println!("{}", "Scan complete.".green());
    if results.warnings > 0 {
        println!(
            "{} {} scan entries were skipped because they could not be read.",
            "!".yellow(),
            results.warnings
        );
    }
    println!();

    print_usage_table(&results, home);
    print_large_files(&results.large_files, args.large, home);

    if args.scan_only {
        println!();
        println!("{}", "Scan-only mode - no cleanup suggested.".dimmed());
        return Ok(());
    }

    let cleanable = build_cleanable_items(&results, excludes);
    if cleanable.is_empty() {
        println!();
        println!("{}", "No cleanable items found.".dimmed());
        return Ok(());
    }

    if let Some(targets) = args.clean.as_deref() {
        let selected = parse_clean_selection(targets, &cleanable)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        print_selection_summary(&cleanable, &selected);
        finish_cleanup(&cleanable, &selected, args.dry_run);
        return Ok(());
    }

    run_interactive_cleanup(&cleanable, args.dry_run);
    Ok(())
}

fn collect_scan_results(
    args: &Args,
    home: &Path,
    threads: usize,
    excludes: &[PathBuf],
) -> ScanResults {
    let min_large_file_size = args.large * 1024 * 1024;
    let project_dirs = get_project_search_dirs(home);

    print_header();
    if !excludes.is_empty() {
        println!(
            "{} {} excluded path(s) active",
            "i".bright_blue(),
            excludes.len()
        );
        println!();
    }

    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.green} {msg}")
            .unwrap(),
    );

    pb.set_message("Scanning cache locations...");
    let mut scan_warnings = 0u64;
    let mut category_sizes: Vec<(Category, u64)> = get_categories(home)
        .into_iter()
        .map(|category| {
            let report = scan_directory(&category.path, threads, excludes);
            scan_warnings += report.warnings;
            pb.tick();
            (category, report.bytes)
        })
        .filter(|(_, size)| *size > 0)
        .collect();
    category_sizes.sort_by(|a, b| b.1.cmp(&a.1));

    pb.set_message("Scanning node_modules...");
    let node_modules =
        find_directories(&project_dirs, "node_modules", threads, None, None, excludes);
    scan_warnings += node_modules.warnings;
    let node_modules_total = node_modules.entries.iter().map(|(_, size)| size).sum();

    pb.set_message("Scanning Python venvs...");
    let venv_validator: fn(&Path) -> bool =
        |path| path.join("pyvenv.cfg").exists() || path.join("bin/python").exists();
    let mut venvs = find_directories(
        &project_dirs,
        ".venv",
        threads,
        Some(5),
        Some(venv_validator),
        excludes,
    );
    let more_venvs = find_directories(
        &project_dirs,
        "venv",
        threads,
        Some(5),
        Some(venv_validator),
        excludes,
    );
    scan_warnings += venvs.warnings + more_venvs.warnings;
    venvs.entries.extend(more_venvs.entries);
    let venvs_total = venvs.entries.iter().map(|(_, size)| size).sum();

    pb.set_message("Scanning __pycache__...");
    let pycaches = find_directories(&project_dirs, "__pycache__", threads, None, None, excludes);
    scan_warnings += pycaches.warnings;
    let pycache_total = pycaches.entries.iter().map(|(_, size)| size).sum();

    pb.set_message("Finding large files...");
    let large_files = find_large_files(home, min_large_file_size, threads, excludes);
    scan_warnings += large_files.warnings;

    pb.set_message("Checking Docker...");
    let docker = get_docker_info();
    pb.finish_and_clear();

    ScanResults {
        category_sizes,
        node_modules: node_modules.entries,
        node_modules_total,
        venvs: venvs.entries,
        venvs_total,
        pycaches: pycaches.entries,
        pycache_total,
        large_files: large_files.files,
        docker,
        warnings: scan_warnings,
    }
}

fn get_home_dir() -> io::Result<PathBuf> {
    dirs::home_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "cannot determine home directory"))
}

fn get_project_search_dirs(home: &Path) -> Vec<PathBuf> {
    [
        "Codes",
        "Projects",
        "Documents",
        "Developer",
        "workspace",
        "repos",
        "src",
    ]
    .iter()
    .map(|directory| home.join(directory))
    .filter(|path| path.exists())
    .collect()
}

fn get_categories(home: &Path) -> Vec<Category> {
    vec![
        ("system-caches", "System Caches", "Library/Caches", true),
        ("app-logs", "App Logs", "Library/Logs", true),
        ("trash", "Trash", ".Trash", true),
        (
            "xcode-deriveddata",
            "Xcode DerivedData",
            "Library/Developer/Xcode/DerivedData",
            true,
        ),
        (
            "xcode-archives",
            "Xcode Archives",
            "Library/Developer/Xcode/Archives",
            false,
        ),
        (
            "ios-simulators",
            "iOS Simulators",
            "Library/Developer/CoreSimulator/Devices",
            false,
        ),
        ("npm-cache", "npm Cache", ".npm", true),
        ("yarn-cache", "Yarn Cache", ".yarn", true),
        ("pnpm-cache", "pnpm Cache", "Library/pnpm", true),
        ("pip-cache", "pip Cache", ".cache/pip", true),
        ("uv-cache", "uv Cache", ".cache/uv", true),
        (
            "homebrew-cache",
            "Homebrew Cache",
            "Library/Caches/Homebrew",
            true,
        ),
        ("gradle-cache", "Gradle Cache", ".gradle/caches", true),
        ("maven-cache", "Maven Cache", ".m2/repository", true),
        (
            "cocoapods-cache",
            "CocoaPods Cache",
            "Library/Caches/CocoaPods",
            true,
        ),
        ("cargo-cache", "Cargo Cache", ".cargo/registry", true),
    ]
    .into_iter()
    .map(|(id, name, path, safe)| Category {
        id,
        name,
        path: home.join(path),
        safe_to_delete: safe,
    })
    .collect()
}

fn command_output_with_timeout(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> io::Result<Option<Output>> {
    let mut child = Command::new(program)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let start = Instant::now();

    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output().map(Some);
        }

        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }

        thread::sleep(Duration::from_millis(50));
    }
}

fn docker_command_output(args: &[&str], timeout: Duration) -> Option<Output> {
    command_output_with_timeout("docker", args, timeout)
        .ok()
        .flatten()
}

fn docker_socket_path_from_host(docker_host: Option<&str>, home: &Path) -> Option<PathBuf> {
    match docker_host {
        Some(host) if host.starts_with("unix://") => {
            let path = host.trim_start_matches("unix://");
            (!path.is_empty()).then(|| PathBuf::from(path))
        }
        Some(_) => None,
        None => Some(home.join(".docker/run/docker.sock")),
    }
}

fn docker_socket_path() -> Option<PathBuf> {
    let docker_host = env::var("DOCKER_HOST").ok();
    let home = get_home_dir().ok()?;
    docker_socket_path_from_host(docker_host.as_deref(), &home)
}

fn should_attempt_docker_cli() -> bool {
    let Some(socket_path) = docker_socket_path() else {
        return true;
    };

    fs::metadata(socket_path)
        .map(|metadata| metadata.file_type().is_socket())
        .unwrap_or(false)
}

fn is_docker_available() -> bool {
    if !should_attempt_docker_cli() {
        return false;
    }

    docker_command_output(&["info"], DOCKER_INFO_TIMEOUT)
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn get_docker_info() -> DockerInfo {
    if !is_docker_available() {
        return DockerInfo::unavailable();
    }

    let Some(output) = docker_command_output(
        &["system", "df", "--format", "{{.Type}}\t{{.Reclaimable}}"],
        DOCKER_SCAN_TIMEOUT,
    ) else {
        return DockerInfo::unavailable();
    };

    if !output.status.success() {
        return DockerInfo::unavailable();
    }

    let mut info = DockerInfo {
        images: 0,
        containers: 0,
        volumes: 0,
        build_cache: 0,
        available: true,
    };

    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 2 {
            let size = parse_docker_size(parts[1]);
            match parts[0] {
                "Images" => info.images = size,
                "Containers" => info.containers = size,
                "Local Volumes" => info.volumes = size,
                "Build Cache" => info.build_cache = size,
                _ => {}
            }
        }
    }
    info
}

fn parse_docker_size(s: &str) -> u64 {
    let s = s.split('(').next().unwrap_or("").trim();
    if s == "0B" || s.is_empty() {
        return 0;
    }

    let (num_str, multiplier) = if let Some(stripped) = s.strip_suffix("GB") {
        (stripped, 1_000_000_000.0)
    } else if let Some(stripped) = s.strip_suffix("MB") {
        (stripped, 1_000_000.0)
    } else if let Some(stripped) = s.strip_suffix("KB").or_else(|| s.strip_suffix("kB")) {
        (stripped, 1_000.0)
    } else if let Some(stripped) = s.strip_suffix('B') {
        (stripped, 1.0)
    } else {
        return 0;
    };

    (num_str.trim().parse::<f64>().unwrap_or(0.0) * multiplier) as u64
}

fn docker_prune_report(resource: &str, args: &[&str]) -> CleanupReport {
    let mut report = CleanupReport::default();
    if !is_docker_available() {
        return report;
    }

    match command_output_with_timeout("docker", args, DOCKER_PRUNE_TIMEOUT) {
        Ok(Some(output)) if output.status.success() => {
            report.freed = parse_docker_reclaimed(&String::from_utf8_lossy(&output.stdout));
        }
        Ok(Some(output)) => report.errors.push(format!(
            "{resource} prune failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Ok(None) => report.errors.push(format!(
            "{resource} prune timed out because Docker did not respond"
        )),
        Err(error) => report
            .errors
            .push(format!("{resource} prune failed: {error}")),
    }

    report
}

fn parse_docker_reclaimed(output: &str) -> u64 {
    output
        .lines()
        .find(|line| line.contains("reclaimed") || line.contains("freed"))
        .and_then(|line| line.split_whitespace().find(|part| part.ends_with('B')))
        .map(parse_docker_size)
        .unwrap_or(0)
}

fn scan_directory(path: &Path, threads: usize, excludes: &[PathBuf]) -> ScanReport {
    if !path.exists() || is_excluded(path, excludes) {
        return ScanReport::default();
    }

    let total = Arc::new(AtomicU64::new(0));
    let warnings = Arc::new(AtomicU64::new(0));
    for entry in WalkDir::new(path)
        .parallelism(jwalk::Parallelism::RayonNewPool(threads))
        .skip_hidden(false)
        .into_iter()
    {
        let Ok(entry) = entry else {
            warnings.fetch_add(1, Ordering::Relaxed);
            continue;
        };

        let entry_path = entry.path();
        if is_excluded(&entry_path, excludes) || !entry.file_type().is_file() {
            continue;
        }

        match entry.metadata() {
            Ok(metadata) => {
                total.fetch_add(metadata.len(), Ordering::Relaxed);
            }
            Err(_) => {
                warnings.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    ScanReport {
        bytes: total.load(Ordering::Relaxed),
        warnings: warnings.load(Ordering::Relaxed),
    }
}

fn find_directories(
    search_dirs: &[PathBuf],
    target_name: &str,
    threads: usize,
    max_depth: Option<usize>,
    validator: Option<fn(&Path) -> bool>,
    excludes: &[PathBuf],
) -> DirectoryScan {
    let mut entries = Vec::new();
    let mut warnings = 0u64;

    for search_dir in search_dirs {
        if is_excluded(search_dir, excludes) {
            continue;
        }

        let mut walker = WalkDir::new(search_dir)
            .parallelism(jwalk::Parallelism::RayonNewPool(threads))
            .skip_hidden(false);

        if let Some(depth) = max_depth {
            walker = walker.max_depth(depth);
        }

        for entry in walker.into_iter() {
            let Ok(entry) = entry else {
                warnings += 1;
                continue;
            };

            if !entry.file_type().is_dir() {
                continue;
            }

            let path = entry.path();
            if is_excluded(&path, excludes) || entry.file_name().to_str() != Some(target_name) {
                continue;
            }

            if target_name == "node_modules"
                && path.ancestors().skip(1).any(|ancestor| {
                    ancestor.file_name() == Some(std::ffi::OsStr::new("node_modules"))
                })
            {
                continue;
            }

            if validator.is_some_and(|validate| !validate(&path)) {
                continue;
            }

            let report = scan_directory(&path, threads, excludes);
            warnings += report.warnings;
            if report.bytes > 0 {
                entries.push((path, report.bytes));
            }
        }
    }

    DirectoryScan { entries, warnings }
}

fn find_large_files(
    home: &Path,
    min_size: u64,
    threads: usize,
    excludes: &[PathBuf],
) -> LargeFileScan {
    let search_dirs: Vec<PathBuf> = ["Downloads", "Desktop", "Documents", "Movies"]
        .iter()
        .map(|directory| home.join(directory))
        .filter(|path| path.exists() && !is_excluded(path, excludes))
        .collect();

    let mut files = Vec::new();
    let mut warnings = 0u64;
    for search_dir in search_dirs {
        for entry in WalkDir::new(&search_dir)
            .parallelism(jwalk::Parallelism::RayonNewPool(threads))
            .skip_hidden(false)
            .into_iter()
        {
            let Ok(entry) = entry else {
                warnings += 1;
                continue;
            };

            let path = entry.path();
            if is_excluded(&path, excludes) || !entry.file_type().is_file() {
                continue;
            }

            match entry.metadata() {
                Ok(metadata) if metadata.len() >= min_size => files.push(LargeFile {
                    path,
                    size: metadata.len(),
                    accessed: metadata.accessed().ok(),
                }),
                Ok(_) => {}
                Err(_) => warnings += 1,
            }
        }
    }

    files.sort_by(|a, b| b.size.cmp(&a.size));
    files.truncate(20);
    LargeFileScan { files, warnings }
}

fn build_cleanable_items(results: &ScanResults, excludes: &[PathBuf]) -> Vec<CleanableItem> {
    let mut cleanable = Vec::new();

    for (category, size) in &results.category_sizes {
        if category.safe_to_delete {
            let path = category.path.clone();
            let excludes = excludes.to_vec();
            cleanable.push(CleanableItem {
                id: category.id,
                group: TargetGroup::Caches,
                label: format!("{} ({})", category.name, ByteSize(*size)),
                size: *size,
                action: Box::new(move || delete_directory_contents(&path, &excludes)),
            });
        }
    }

    push_path_collection(
        &mut cleanable,
        "node-modules",
        TargetGroup::Projects,
        "node_modules",
        &results.node_modules,
        results.node_modules_total,
        excludes,
    );
    push_path_collection(
        &mut cleanable,
        "python-venvs",
        TargetGroup::Projects,
        "Python venvs",
        &results.venvs,
        results.venvs_total,
        excludes,
    );
    push_path_collection(
        &mut cleanable,
        "pycache",
        TargetGroup::Projects,
        "__pycache__",
        &results.pycaches,
        results.pycache_total,
        excludes,
    );

    if results.docker.available {
        push_docker_item(
            &mut cleanable,
            "docker-images",
            "Docker images",
            results.docker.images,
            || docker_prune_report("image", &["image", "prune", "-af"]),
        );
        push_docker_item(
            &mut cleanable,
            "docker-containers",
            "Docker containers",
            results.docker.containers,
            || docker_prune_report("container", &["container", "prune", "-f"]),
        );
        push_docker_item(
            &mut cleanable,
            "docker-build-cache",
            "Docker build cache",
            results.docker.build_cache,
            || docker_prune_report("builder", &["builder", "prune", "-af"]),
        );
        push_docker_item(
            &mut cleanable,
            "docker-volumes",
            "Docker volumes",
            results.docker.volumes,
            || docker_prune_report("volume", &["volume", "prune", "-af"]),
        );
    }

    cleanable
}

fn push_path_collection(
    cleanable: &mut Vec<CleanableItem>,
    id: &'static str,
    group: TargetGroup,
    label: &str,
    paths: &[(PathBuf, u64)],
    total: u64,
    excludes: &[PathBuf],
) {
    if total == 0 {
        return;
    }

    let paths: Vec<PathBuf> = paths.iter().map(|(path, _)| path.clone()).collect();
    let excludes = excludes.to_vec();
    cleanable.push(CleanableItem {
        id,
        group,
        label: format!("{label} - {} dirs ({})", paths.len(), ByteSize(total)),
        size: total,
        action: Box::new(move || {
            let mut report = CleanupReport::default();
            for path in &paths {
                report.merge(delete_path(path, &excludes, true));
            }
            report
        }),
    });
}

fn push_docker_item<F>(
    cleanable: &mut Vec<CleanableItem>,
    id: &'static str,
    label: &str,
    size: u64,
    action: F,
) where
    F: Fn() -> CleanupReport + 'static,
{
    if size == 0 {
        return;
    }

    cleanable.push(CleanableItem {
        id,
        group: TargetGroup::Docker,
        label: format!("{label} ({})", ByteSize(size)),
        size,
        action: Box::new(action),
    });
}

fn parse_clean_selection(input: &str, items: &[CleanableItem]) -> Result<Vec<usize>, String> {
    let mut selected = Vec::new();

    for token in input
        .split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
    {
        let matches: Vec<usize> = items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| target_matches(token, item).then_some(index))
            .collect();

        if matches.is_empty() {
            if !is_known_target(token) {
                return Err(format!("unknown clean target: {token}"));
            }
            continue;
        }

        for index in matches {
            if !selected.contains(&index) {
                selected.push(index);
            }
        }
    }

    if selected.is_empty() {
        return Err("no clean targets matched the current scan".to_string());
    }

    Ok(selected)
}

fn is_known_target(token: &str) -> bool {
    matches!(token, "all" | "caches" | "projects" | "docker") || target_ids().contains(&token)
}

fn target_matches(token: &str, item: &CleanableItem) -> bool {
    token == "all" || token == item.group.id() || token == item.id
}

fn run_interactive_cleanup(cleanable: &[CleanableItem], dry_run: bool) {
    let names: Vec<&str> = cleanable.iter().map(|item| item.label.as_str()).collect();
    let selections = MultiSelect::with_theme(&ColorfulTheme::default())
        .with_prompt("Select categories to clean")
        .items(&names)
        .interact();

    match selections {
        Ok(selected) if !selected.is_empty() => {
            print_selection_summary(cleanable, &selected);
            if dry_run {
                print_dry_run(cleanable, &selected);
                return;
            }

            match Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt("Proceed with cleanup?")
                .default(true)
                .interact()
            {
                Ok(true) => finish_cleanup(cleanable, &selected, false),
                Ok(false) => println!("{}", "Cancelled.".dimmed()),
                Err(_) => println!("{}", "Interrupted.".red()),
            }
        }
        Ok(_) => println!("{}", "No categories selected.".dimmed()),
        Err(_) => println!("{}", "Interrupted.".red()),
    }
}

fn print_selection_summary(cleanable: &[CleanableItem], selected: &[usize]) {
    let total_to_free: u64 = selected.iter().map(|&index| cleanable[index].size).sum();
    println!();
    println!(
        "Will free approximately {}",
        ByteSize(total_to_free).to_string().green().bold()
    );
    for &index in selected {
        println!("  - {} [{}]", cleanable[index].label, cleanable[index].id);
    }
}

fn finish_cleanup(cleanable: &[CleanableItem], selected: &[usize], dry_run: bool) {
    if dry_run {
        print_dry_run(cleanable, selected);
        return;
    }

    println!();
    let report = run_cleanup(cleanable, selected, false);
    println!(
        "{} Freed {}",
        "✓".green().bold(),
        ByteSize(report.freed).to_string().green().bold()
    );
    print_cleanup_errors(&report);
}

fn print_dry_run(cleanable: &[CleanableItem], selected: &[usize]) {
    println!();
    println!(
        "{}",
        "Dry run mode - no files will be deleted.".yellow().bold()
    );
    for &index in selected {
        println!("  - {} [{}]", cleanable[index].label, cleanable[index].id);
    }
}

fn run_cleanup(cleanable: &[CleanableItem], selected: &[usize], dry_run: bool) -> CleanupReport {
    let mut report = CleanupReport::default();
    if dry_run {
        return report;
    }

    let pb = ProgressBar::new(selected.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .unwrap()
            .progress_chars("#>-"),
    );

    for &index in selected {
        let item = &cleanable[index];
        pb.set_message(format!("Cleaning {}...", item.id));
        let item_report = (item.action)();
        report.freed += item_report.freed;
        for error in item_report.errors {
            report.errors.push(format!("{}: {error}", item.id));
        }
        pb.inc(1);
    }

    pb.finish_and_clear();
    report
}

fn print_cleanup_errors(report: &CleanupReport) {
    if report.errors.is_empty() {
        return;
    }

    println!();
    println!("{}", "Warnings:".yellow());
    for error in &report.errors {
        println!("  {} {error}", "!".yellow());
    }
}

fn delete_directory_contents(path: &Path, excludes: &[PathBuf]) -> CleanupReport {
    let mut report = CleanupReport::default();
    if !path.exists() || is_excluded(path, excludes) {
        return report;
    }

    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) => {
            report.add_error(path, error);
            return report;
        }
    };

    for entry in entries {
        match entry {
            Ok(entry) => report.merge(delete_path(&entry.path(), excludes, true)),
            Err(error) => report.errors.push(format!("{}: {error}", path.display())),
        }
    }

    report
}

fn delete_path(path: &Path, excludes: &[PathBuf], remove_root: bool) -> CleanupReport {
    let mut report = CleanupReport::default();
    if is_excluded(path, excludes) {
        return report;
    }

    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return report,
        Err(error) => {
            report.add_error(path, error);
            return report;
        }
    };

    if metadata.is_dir() {
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) => {
                report.add_error(path, error);
                return report;
            }
        };

        for entry in entries {
            match entry {
                Ok(entry) => report.merge(delete_path(&entry.path(), excludes, true)),
                Err(error) => report.errors.push(format!("{}: {error}", path.display())),
            }
        }

        if remove_root {
            match fs::remove_dir(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => report.add_error(path, error),
            }
        }
        return report;
    }

    let size = metadata.len();
    match fs::remove_file(path) {
        Ok(()) => report.freed += size,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => report.add_error(path, error),
    }
    report
}

fn merge_excludes(config: &Config, cli_excludes: &[PathBuf], home: &Path) -> Vec<PathBuf> {
    let mut excludes = Vec::new();
    for path in &config.exclude {
        push_unique_path(&mut excludes, normalize_path(Path::new(path), home));
    }
    for path in cli_excludes {
        push_unique_path(&mut excludes, normalize_path(path, home));
    }
    excludes
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.iter().any(|existing| existing == &path) {
        paths.push(path);
    }
}

fn normalize_path(path: &Path, home: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    let expanded = if raw == "~" {
        home.to_path_buf()
    } else if let Some(stripped) = raw.strip_prefix("~/") {
        home.join(stripped)
    } else {
        path.to_path_buf()
    };

    if expanded.is_absolute() {
        expanded
    } else {
        env::current_dir()
            .map(|current_dir| current_dir.join(&expanded))
            .unwrap_or(expanded)
    }
}

fn is_excluded(path: &Path, excludes: &[PathBuf]) -> bool {
    excludes.iter().any(|exclude| path.starts_with(exclude))
}

fn display_path(path: &Path, home: &Path) -> String {
    path.strip_prefix(home)
        .map(|path| format!("~/{}", path.display()))
        .unwrap_or_else(|_| path.display().to_string())
}

fn format_time_ago(time: Option<SystemTime>) -> String {
    match time {
        Some(time) => {
            let datetime: DateTime<Local> = time.into();
            let days = Local::now().signed_duration_since(datetime).num_days();
            if days > 365 {
                let years = days / 365;
                format!("{years} years ago")
            } else if days > 30 {
                let months = days / 30;
                format!("{months} months ago")
            } else if days > 0 {
                format!("{days} days ago")
            } else {
                "recent".to_string()
            }
        }
        None => "-".to_string(),
    }
}

fn print_target_reference() {
    println!("Groups:");
    println!("  all");
    println!("  caches");
    println!("  projects");
    println!("  docker");
    println!();
    println!("Target IDs:");
    for id in target_ids() {
        println!("  {id}");
    }
}

fn target_ids() -> &'static [&'static str] {
    &[
        "system-caches",
        "app-logs",
        "trash",
        "xcode-deriveddata",
        "npm-cache",
        "yarn-cache",
        "pnpm-cache",
        "pip-cache",
        "uv-cache",
        "homebrew-cache",
        "gradle-cache",
        "maven-cache",
        "cocoapods-cache",
        "cargo-cache",
        "node-modules",
        "python-venvs",
        "pycache",
        "docker-images",
        "docker-containers",
        "docker-build-cache",
        "docker-volumes",
    ]
}

fn print_header() {
    println!();
    println!("{}", "macOS Disk Cleaner".bright_blue().bold());
    println!(
        "{}",
        "Fast parallel disk space analyzer and cleaner".dimmed()
    );
    println!();
}

fn print_usage_table(results: &ScanResults, home: &Path) {
    println!(
        "{:<24} {:<32} {:>14}",
        "Category".bold(),
        "Location".bold(),
        "Size".bold()
    );
    println!("{:-<74}", "");

    let mut total = 0u64;
    for (category, size) in &results.category_sizes {
        print_table_row(category.name, &display_path(&category.path, home), *size);
        total += size;
    }

    if results.node_modules_total > 0 {
        print_table_row(
            "node_modules",
            &format!("{} directories", results.node_modules.len()),
            results.node_modules_total,
        );
        total += results.node_modules_total;
    }
    if results.venvs_total > 0 {
        print_table_row(
            "Python venvs",
            &format!("{} directories", results.venvs.len()),
            results.venvs_total,
        );
        total += results.venvs_total;
    }
    if results.pycache_total > 0 {
        print_table_row(
            "__pycache__",
            &format!("{} directories", results.pycaches.len()),
            results.pycache_total,
        );
        total += results.pycache_total;
    }
    if results.docker.available && results.docker.total() > 0 {
        print_table_row(
            "Docker",
            "images, containers, volumes",
            results.docker.total(),
        );
        total += results.docker.total();
    }

    println!("{:-<74}", "");
    print_table_row("TOTAL", "", total);
}

fn print_table_row(name: &str, location: &str, size: u64) {
    let location = if location.len() > 32 {
        format!("{}...", &location[..29])
    } else {
        location.to_string()
    };
    let size = ByteSize(size).to_string();
    println!("{name:<24} {location:<32} {size:>14}");
}

fn print_large_files(files: &[LargeFile], threshold_mb: u64, home: &Path) {
    if files.is_empty() {
        return;
    }

    println!();
    println!("Large Files (>{threshold_mb}MB)");
    println!(
        "{:<52} {:>14} {:>14}",
        "File".bold(),
        "Size".bold(),
        "Access".bold()
    );
    println!("{:-<82}", "");

    for file in files {
        let short = display_path(&file.path, home);
        let display = if short.len() > 52 {
            format!("...{}", &short[short.len() - 49..])
        } else {
            short
        };
        println!(
            "{display:<52} {:>14} {:>14}",
            ByteSize(file.size).to_string(),
            format_time_ago(file.accessed)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_timeout_returns_none_for_unresponsive_command() {
        let output =
            command_output_with_timeout("sh", &["-c", "sleep 2"], Duration::from_millis(20))
                .expect("command should spawn");

        assert!(output.is_none());
    }

    #[test]
    fn command_timeout_captures_responsive_command_output() {
        let output =
            command_output_with_timeout("sh", &["-c", "printf ok"], Duration::from_secs(1))
                .expect("command should spawn")
                .expect("command should finish before timeout");

        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "ok");
    }

    #[test]
    fn parse_docker_size_keeps_decimal_units() {
        assert_eq!(parse_docker_size("1.5GB (50%)"), 1_500_000_000);
        assert_eq!(parse_docker_size("42.25MB"), 42_250_000);
        assert_eq!(parse_docker_size("512kB"), 512_000);
    }

    #[test]
    fn docker_socket_path_uses_default_desktop_socket() {
        let home = Path::new("/Users/example");

        assert_eq!(
            docker_socket_path_from_host(None, home),
            Some(PathBuf::from("/Users/example/.docker/run/docker.sock"))
        );
    }

    #[test]
    fn docker_socket_path_parses_unix_docker_host() {
        let home = Path::new("/Users/example");

        assert_eq!(
            docker_socket_path_from_host(Some("unix:///tmp/docker.sock"), home),
            Some(PathBuf::from("/tmp/docker.sock"))
        );
    }

    #[test]
    fn docker_socket_path_skips_non_unix_docker_host() {
        let home = Path::new("/Users/example");

        assert_eq!(
            docker_socket_path_from_host(Some("tcp://127.0.0.1:2375"), home),
            None
        );
    }

    #[test]
    fn clean_selection_accepts_groups_and_ids() {
        let items = vec![
            test_item("system-caches", TargetGroup::Caches, 10),
            test_item("node-modules", TargetGroup::Projects, 20),
            test_item("docker-images", TargetGroup::Docker, 30),
        ];

        assert_eq!(parse_clean_selection("all", &items).unwrap(), vec![0, 1, 2]);
        assert_eq!(
            parse_clean_selection("caches,node-modules", &items).unwrap(),
            vec![0, 1]
        );
        assert_eq!(
            parse_clean_selection("docker-images", &items).unwrap(),
            vec![2]
        );
    }

    #[test]
    fn clean_selection_ignores_known_empty_group() {
        let items = vec![test_item("system-caches", TargetGroup::Caches, 10)];

        assert_eq!(
            parse_clean_selection("caches,docker", &items).unwrap(),
            vec![0]
        );
    }

    #[test]
    fn clean_selection_rejects_only_empty_known_group() {
        let items = vec![test_item("system-caches", TargetGroup::Caches, 10)];

        assert_eq!(
            parse_clean_selection("docker", &items).unwrap_err(),
            "no clean targets matched the current scan"
        );
    }

    #[test]
    fn clean_selection_rejects_unknown_targets() {
        let items = vec![test_item("system-caches", TargetGroup::Caches, 10)];

        assert!(parse_clean_selection("system-caches,unknown", &items).is_err());
    }

    #[test]
    fn config_and_cli_excludes_are_merged_and_expanded() {
        let home = Path::new("/Users/example");
        let config = Config {
            exclude: vec!["~/Codes/keep".to_string()],
        };
        let cli = vec![PathBuf::from("~/Downloads/keep")];

        assert_eq!(
            merge_excludes(&config, &cli, home),
            vec![
                PathBuf::from("/Users/example/Codes/keep"),
                PathBuf::from("/Users/example/Downloads/keep"),
            ]
        );
    }

    #[test]
    fn cleanup_runner_continues_after_item_error() {
        let items = vec![
            test_item_with_action(
                "first",
                TargetGroup::Caches,
                10,
                Box::new(|| CleanupReport {
                    freed: 0,
                    errors: vec!["permission denied".to_string()],
                }),
            ),
            test_item_with_action(
                "second",
                TargetGroup::Caches,
                20,
                Box::new(|| CleanupReport {
                    freed: 20,
                    errors: Vec::new(),
                }),
            ),
        ];

        let report = run_cleanup(&items, &[0, 1], false);

        assert_eq!(report.freed, 20);
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].contains("first"));
    }

    fn test_item(id: &'static str, group: TargetGroup, size: u64) -> CleanableItem {
        test_item_with_action(id, group, size, Box::new(CleanupReport::default))
    }

    fn test_item_with_action(
        id: &'static str,
        group: TargetGroup,
        size: u64,
        action: CleanAction,
    ) -> CleanableItem {
        CleanableItem {
            id,
            group,
            label: id.to_string(),
            size,
            action,
        }
    }
}
