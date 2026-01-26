use bytesize::ByteSize;
use chrono::{DateTime, Local};
use clap::Parser;
use colored::*;
use dialoguer::{theme::ColorfulTheme, Confirm, MultiSelect};
use indicatif::{ProgressBar, ProgressStyle};
use jwalk::WalkDir;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

#[derive(Parser)]
#[command(name = "disk-cleaner")]
#[command(about = "macOS Disk Cleaner - Fast parallel disk space analyzer")]
struct Args {
    #[arg(short, long, help = "Scan only, don't suggest cleanup")]
    scan_only: bool,

    #[arg(short = 'n', long, help = "Dry run - show what would be deleted")]
    dry_run: bool,

    #[arg(short, long, default_value = "100", help = "Large file threshold in MB")]
    large: u64,

    #[arg(short, long, help = "Number of scan threads (default: CPU cores)")]
    threads: Option<usize>,
}

#[derive(Clone)]
struct Category {
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

impl DockerInfo {
    fn total(&self) -> u64 {
        self.images + self.containers + self.volumes + self.build_cache
    }
}

fn get_home_dir() -> PathBuf {
    dirs::home_dir().expect("Cannot determine home directory")
}

fn get_project_search_dirs(home: &Path) -> Vec<PathBuf> {
    ["Codes", "Projects", "Documents", "Developer", "workspace", "repos", "src"]
        .iter()
        .map(|d| home.join(d))
        .filter(|p| p.exists())
        .collect()
}

fn get_categories() -> Vec<Category> {
    let home = get_home_dir();
    vec![
        ("System Caches", "Library/Caches", true),
        ("App Logs", "Library/Logs", true),
        ("Trash", ".Trash", true),
        ("Xcode DerivedData", "Library/Developer/Xcode/DerivedData", true),
        ("Xcode Archives", "Library/Developer/Xcode/Archives", false),
        ("iOS Simulators", "Library/Developer/CoreSimulator/Devices", false),
        ("npm Cache", ".npm", true),
        ("Yarn Cache", ".yarn", true),
        ("pnpm Cache", "Library/pnpm", true),
        ("pip Cache", ".cache/pip", true),
        ("uv Cache", ".cache/uv", true),
        ("Homebrew Cache", "Library/Caches/Homebrew", true),
        ("Gradle Cache", ".gradle/caches", true),
        ("Maven Cache", ".m2/repository", true),
        ("CocoaPods Cache", "Library/Caches/CocoaPods", true),
        ("Cargo Cache", ".cargo/registry", true),
    ]
    .into_iter()
    .map(|(name, path, safe)| Category {
        name,
        path: home.join(path),
        safe_to_delete: safe,
    })
    .collect()
}

fn is_docker_available() -> bool {
    Command::new("docker")
        .arg("info")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn get_docker_info() -> DockerInfo {
    if !is_docker_available() {
        return DockerInfo {
            images: 0,
            containers: 0,
            volumes: 0,
            build_cache: 0,
            available: false,
        };
    }

    let output = Command::new("docker")
        .args(["system", "df", "--format", "{{.Type}}\t{{.Reclaimable}}"])
        .output();

    let mut info = DockerInfo {
        images: 0,
        containers: 0,
        volumes: 0,
        build_cache: 0,
        available: true,
    };

    if let Ok(output) = output {
        if output.status.success() {
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
        }
    }
    info
}

fn parse_docker_size(s: &str) -> u64 {
    let s = s.split('(').next().unwrap_or("").trim();
    if s == "0B" || s.is_empty() {
        return 0;
    }

    let (num_str, multiplier) = if s.ends_with("GB") {
        (&s[..s.len() - 2], 1_000_000_000.0)
    } else if s.ends_with("MB") {
        (&s[..s.len() - 2], 1_000_000.0)
    } else if s.ends_with("KB") || s.ends_with("kB") {
        (&s[..s.len() - 2], 1_000.0)
    } else if s.ends_with("B") {
        (&s[..s.len() - 1], 1.0)
    } else {
        return 0;
    };

    num_str.trim().parse::<f64>().unwrap_or(0.0) as u64 * multiplier as u64
}

fn docker_prune(resource: &str, args: &[&str]) -> io::Result<u64> {
    let output = Command::new("docker").args(args).output()?;
    if !output.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("{} prune failed: {}", resource, String::from_utf8_lossy(&output.stderr)),
        ));
    }
    Ok(parse_docker_reclaimed(&String::from_utf8_lossy(&output.stdout)))
}

fn parse_docker_reclaimed(output: &str) -> u64 {
    output
        .lines()
        .find(|l| l.contains("reclaimed") || l.contains("freed"))
        .and_then(|l| l.split_whitespace().find(|s| s.ends_with('B')))
        .map(parse_docker_size)
        .unwrap_or(0)
}

fn scan_directory(path: &Path, threads: usize) -> u64 {
    if !path.exists() {
        return 0;
    }

    let total = Arc::new(AtomicU64::new(0));
    for entry in WalkDir::new(path)
        .parallelism(jwalk::Parallelism::RayonNewPool(threads))
        .skip_hidden(false)
        .into_iter()
        .flatten()
    {
        if entry.file_type().is_file() {
            if let Ok(meta) = entry.metadata() {
                total.fetch_add(meta.len(), Ordering::Relaxed);
            }
        }
    }
    total.load(Ordering::Relaxed)
}

fn find_directories(
    search_dirs: &[PathBuf],
    target_name: &str,
    threads: usize,
    max_depth: Option<usize>,
    validator: Option<fn(&Path) -> bool>,
) -> Vec<(PathBuf, u64)> {
    let mut results = Vec::new();

    for search_dir in search_dirs {
        let mut walker = WalkDir::new(search_dir)
            .parallelism(jwalk::Parallelism::RayonNewPool(threads))
            .skip_hidden(false);

        if let Some(depth) = max_depth {
            walker = walker.max_depth(depth);
        }

        for entry in walker.into_iter().flatten() {
            if !entry.file_type().is_dir() {
                continue;
            }
            let name = entry.file_name().to_str().unwrap_or("");
            if name != target_name {
                continue;
            }
            let path = entry.path();

            if target_name == "node_modules"
                && path
                    .ancestors()
                    .skip(1)
                    .any(|p| p.file_name() == Some(std::ffi::OsStr::new("node_modules")))
            {
                continue;
            }

            if let Some(validate) = validator {
                if !validate(&path) {
                    continue;
                }
            }

            let size = scan_directory(&path, threads);
            if size > 0 {
                results.push((path, size));
            }
        }
    }
    results
}

fn find_large_files(home: &Path, min_size: u64, threads: usize) -> Vec<LargeFile> {
    let search_dirs: Vec<PathBuf> = ["Downloads", "Desktop", "Documents", "Movies"]
        .iter()
        .map(|d| home.join(d))
        .filter(|p| p.exists())
        .collect();

    let mut results = Vec::new();
    for search_dir in search_dirs {
        for entry in WalkDir::new(&search_dir)
            .parallelism(jwalk::Parallelism::RayonNewPool(threads))
            .skip_hidden(false)
            .into_iter()
            .flatten()
        {
            if entry.file_type().is_file() {
                if let Ok(meta) = entry.metadata() {
                    if meta.len() >= min_size {
                        results.push(LargeFile {
                            path: entry.path(),
                            size: meta.len(),
                            accessed: meta.accessed().ok(),
                        });
                    }
                }
            }
        }
    }

    results.sort_by(|a, b| b.size.cmp(&a.size));
    results.truncate(20);
    results
}

fn format_time_ago(time: Option<SystemTime>) -> String {
    match time {
        Some(t) => {
            let datetime: DateTime<Local> = t.into();
            let days = Local::now().signed_duration_since(datetime).num_days();
            if days > 365 {
                format!("{}년전", days / 365)
            } else if days > 30 {
                format!("{}개월전", days / 30)
            } else if days > 0 {
                format!("{}일전", days)
            } else {
                "최근".to_string()
            }
        }
        None => "-".to_string(),
    }
}

fn shorten_path(path: &Path, home: &Path) -> String {
    path.strip_prefix(home)
        .map(|p| format!("~/{}", p.display()))
        .unwrap_or_else(|_| path.display().to_string())
}

fn delete_directory_contents(path: &Path) -> io::Result<u64> {
    if !path.exists() {
        return Ok(0);
    }

    let mut freed = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let entry_path = entry.path();
        if entry_path.is_dir() {
            freed += scan_directory(&entry_path, 4);
            fs::remove_dir_all(&entry_path)?;
        } else {
            freed += entry.metadata().map(|m| m.len()).unwrap_or(0);
            fs::remove_file(&entry_path)?;
        }
    }
    Ok(freed)
}

fn delete_directory(path: &Path) -> io::Result<u64> {
    if !path.exists() {
        return Ok(0);
    }
    let size = scan_directory(path, 4);
    fs::remove_dir_all(path)?;
    Ok(size)
}

fn print_header() {
    println!();
    println!("{}", "╭─────────────────────────────────────────────────────────────╮".bright_blue());
    println!("{}", "│                    macOS Disk Cleaner                       │".bright_blue());
    println!("{}", "╰─────────────────────────────────────────────────────────────╯".bright_blue());
    println!();
}

fn print_table_header() {
    println!("{}", "╭──────────────────────────────────────────────────────────────────────────────╮".bright_blue());
    println!("{}", "│                         Disk Usage by Category                               │".bright_blue());
    println!("{}", "├────────────────────────┬──────────────────────────────┬──────────────────────┤".bright_blue());
    println!(
        "{} {:<22} {} {:<28} {} {:>20} {}",
        "│".bright_blue(), "Category".bold(), "│".bright_blue(),
        "Location".bold(), "│".bright_blue(), "Size".bold(), "│".bright_blue()
    );
    println!("{}", "├────────────────────────┼──────────────────────────────┼──────────────────────┤".bright_blue());
}

fn print_table_row(name: &str, location: &str, size: u64) {
    let size_str = ByteSize(size).to_string();
    let size_colored = if size > 1_000_000_000 {
        size_str.red().bold()
    } else if size > 100_000_000 {
        size_str.yellow()
    } else {
        size_str.normal()
    };

    let loc = if location.len() > 28 {
        format!("{}...", &location[..25])
    } else {
        location.to_string()
    };

    println!(
        "{} {:<22} {} {:<28} {} {:>20} {}",
        "│".bright_blue(), name, "│".bright_blue(), loc, "│".bright_blue(), size_colored, "│".bright_blue()
    );
}

fn print_table_footer(total: u64) {
    println!("{}", "├────────────────────────┴──────────────────────────────┼──────────────────────┤".bright_blue());
    println!(
        "{} {:<52} {} {:>20} {}",
        "│".bright_blue(), "TOTAL".bold(), "│".bright_blue(),
        ByteSize(total).to_string().green().bold(), "│".bright_blue()
    );
    println!("{}", "╰───────────────────────────────────────────────────────┴──────────────────────╯".bright_blue());
}

fn main() {
    let args = Args::parse();
    let threads = args.threads.unwrap_or_else(num_cpus::get);
    let home = get_home_dir();
    let min_large_file_size = args.large * 1024 * 1024;
    let project_dirs = get_project_search_dirs(&home);

    print_header();

    let pb = ProgressBar::new_spinner();
    pb.set_style(ProgressStyle::default_spinner().template("{spinner:.green} {msg}").unwrap());

    pb.set_message("Scanning cache locations...");
    let categories = get_categories();
    let mut category_sizes: Vec<(Category, u64)> = categories
        .into_iter()
        .map(|c| {
            let size = scan_directory(&c.path, threads);
            pb.tick();
            (c, size)
        })
        .filter(|(_, size)| *size > 0)
        .collect();
    category_sizes.sort_by(|a, b| b.1.cmp(&a.1));

    pb.set_message("Scanning node_modules...");
    let node_modules = find_directories(&project_dirs, "node_modules", threads, None, None);
    let node_modules_total: u64 = node_modules.iter().map(|(_, s)| s).sum();

    pb.set_message("Scanning Python venvs...");
    let venv_validator: fn(&Path) -> bool = |p| p.join("pyvenv.cfg").exists() || p.join("bin/python").exists();
    let mut venvs = find_directories(&project_dirs, ".venv", threads, Some(5), Some(venv_validator));
    venvs.extend(find_directories(&project_dirs, "venv", threads, Some(5), Some(venv_validator)));
    let venvs_total: u64 = venvs.iter().map(|(_, s)| s).sum();

    pb.set_message("Scanning __pycache__...");
    let pycaches = find_directories(&project_dirs, "__pycache__", threads, None, None);
    let pycache_total: u64 = pycaches.iter().map(|(_, s)| s).sum();

    pb.set_message("Finding large files...");
    let large_files = find_large_files(&home, min_large_file_size, threads);

    pb.set_message("Checking Docker...");
    let docker = get_docker_info();

    pb.finish_and_clear();
    println!("{}", "Scan complete!".green());
    println!();

    print_table_header();

    let mut total: u64 = 0;
    for (cat, size) in &category_sizes {
        print_table_row(cat.name, &shorten_path(&cat.path, &home), *size);
        total += size;
    }

    if node_modules_total > 0 {
        print_table_row("node_modules", &format!("{} directories", node_modules.len()), node_modules_total);
        total += node_modules_total;
    }
    if venvs_total > 0 {
        print_table_row("Python .venv", &format!("{} directories", venvs.len()), venvs_total);
        total += venvs_total;
    }
    if pycache_total > 0 {
        print_table_row("__pycache__", &format!("{} directories", pycaches.len()), pycache_total);
        total += pycache_total;
    }
    if docker.available && docker.total() > 0 {
        print_table_row("Docker", "images, containers, volumes", docker.total());
        total += docker.total();
    }

    print_table_footer(total);

    if !large_files.is_empty() {
        println!();
        println!("{}", "╭──────────────────────────────────────────────────────────────────────────────╮".bright_blue());
        println!("{}", format!("│                    Large Files (>{}MB)                                       │", args.large).bright_blue());
        println!("{}", "├──────────────────────────────────────────────────┬─────────────┬─────────────┤".bright_blue());
        println!(
            "{} {:<48} {} {:>11} {} {:>11} {}",
            "│".bright_blue(), "File".bold(), "│".bright_blue(),
            "Size".bold(), "│".bright_blue(), "Access".bold(), "│".bright_blue()
        );
        println!("{}", "├──────────────────────────────────────────────────┼─────────────┼─────────────┤".bright_blue());

        for file in &large_files {
            let short = shorten_path(&file.path, &home);
            let display = if short.len() > 48 {
                format!("...{}", &short[short.len() - 45..])
            } else {
                short
            };
            println!(
                "{} {:<48} {} {:>11} {} {:>11} {}",
                "│".bright_blue(), display, "│".bright_blue(),
                ByteSize(file.size).to_string().yellow(), "│".bright_blue(),
                format_time_ago(file.accessed), "│".bright_blue()
            );
        }
        println!("{}", "╰──────────────────────────────────────────────────┴─────────────┴─────────────╯".bright_blue());
    }

    if args.scan_only {
        println!();
        println!("{}", "Scan-only mode - no cleanup suggested.".dimmed());
        return;
    }

    println!();

    let mut cleanable: Vec<(String, u64, Box<dyn Fn() -> io::Result<u64>>)> = Vec::new();

    for (cat, size) in &category_sizes {
        if cat.safe_to_delete {
            let path = cat.path.clone();
            let name = format!("{} ({})", cat.name, ByteSize(*size));
            cleanable.push((name, *size, Box::new(move || delete_directory_contents(&path))));
        }
    }

    if node_modules_total > 0 {
        let paths: Vec<PathBuf> = node_modules.iter().map(|(p, _)| p.clone()).collect();
        cleanable.push((
            format!("node_modules - {} dirs ({})", paths.len(), ByteSize(node_modules_total)),
            node_modules_total,
            Box::new(move || {
                let mut freed = 0u64;
                for p in &paths {
                    freed += delete_directory(p).unwrap_or(0);
                }
                Ok(freed)
            }),
        ));
    }

    if venvs_total > 0 {
        let paths: Vec<PathBuf> = venvs.iter().map(|(p, _)| p.clone()).collect();
        cleanable.push((
            format!("Python .venv - {} dirs ({})", paths.len(), ByteSize(venvs_total)),
            venvs_total,
            Box::new(move || {
                let mut freed = 0u64;
                for p in &paths {
                    freed += delete_directory(p).unwrap_or(0);
                }
                Ok(freed)
            }),
        ));
    }

    if pycache_total > 0 {
        let paths: Vec<PathBuf> = pycaches.iter().map(|(p, _)| p.clone()).collect();
        cleanable.push((
            format!("__pycache__ - {} dirs ({})", paths.len(), ByteSize(pycache_total)),
            pycache_total,
            Box::new(move || {
                let mut freed = 0u64;
                for p in &paths {
                    freed += delete_directory(p).unwrap_or(0);
                }
                Ok(freed)
            }),
        ));
    }

    if docker.available {
        if docker.images > 0 {
            let size = docker.images;
            cleanable.push((
                format!("Docker Images ({})", ByteSize(size)),
                size,
                Box::new(|| docker_prune("image", &["image", "prune", "-af"])),
            ));
        }
        if docker.containers > 0 {
            let size = docker.containers;
            cleanable.push((
                format!("Docker Containers ({})", ByteSize(size)),
                size,
                Box::new(|| docker_prune("container", &["container", "prune", "-f"])),
            ));
        }
        if docker.build_cache > 0 {
            let size = docker.build_cache;
            cleanable.push((
                format!("Docker Build Cache ({})", ByteSize(size)),
                size,
                Box::new(|| docker_prune("builder", &["builder", "prune", "-af"])),
            ));
        }
        if docker.volumes > 0 {
            let size = docker.volumes;
            cleanable.push((
                format!("Docker Volumes ({})", ByteSize(size)),
                size,
                Box::new(|| docker_prune("volume", &["volume", "prune", "-af"])),
            ));
        }
    }

    if cleanable.is_empty() {
        println!("{}", "No cleanable items found.".dimmed());
        return;
    }

    let names: Vec<&str> = cleanable.iter().map(|(n, _, _)| n.as_str()).collect();
    let selections = MultiSelect::with_theme(&ColorfulTheme::default())
        .with_prompt("Select categories to clean")
        .items(&names)
        .interact();

    match selections {
        Ok(selected) if !selected.is_empty() => {
            let total_to_free: u64 = selected.iter().map(|&i| cleanable[i].1).sum();
            println!();
            println!("Will free approximately {}", ByteSize(total_to_free).to_string().green().bold());

            if args.dry_run {
                println!();
                println!("{}", "Dry run mode - no files will be deleted.".yellow().bold());
                for &i in &selected {
                    println!("  - {}", cleanable[i].0);
                }
                return;
            }

            match Confirm::with_theme(&ColorfulTheme::default())
                .with_prompt("Proceed with cleanup?")
                .default(false)
                .interact()
            {
                Ok(true) => {
                    println!();
                    let pb = ProgressBar::new(selected.len() as u64);
                    pb.set_style(
                        ProgressStyle::default_bar()
                            .template("{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} {msg}")
                            .unwrap()
                            .progress_chars("#>-"),
                    );

                    let mut freed = 0u64;
                    let mut errors = Vec::new();

                    for &i in &selected {
                        pb.set_message(format!("Cleaning {}...", cleanable[i].0));
                        match (cleanable[i].2)() {
                            Ok(f) => freed += f,
                            Err(e) => errors.push((cleanable[i].0.clone(), e)),
                        }
                        pb.inc(1);
                    }

                    pb.finish_and_clear();
                    println!();
                    println!("{} Freed {}", "✓".green().bold(), ByteSize(freed).to_string().green().bold());

                    if !errors.is_empty() {
                        println!();
                        println!("{}", "Errors:".red());
                        for (name, err) in errors {
                            println!("  {} {}: {}", "✗".red(), name, err);
                        }
                    }
                }
                Ok(false) => println!("{}", "Cancelled.".dimmed()),
                Err(_) => println!("{}", "Interrupted.".red()),
            }
        }
        Ok(_) => println!("{}", "No categories selected.".dimmed()),
        Err(_) => println!("{}", "Interrupted.".red()),
    }
}
