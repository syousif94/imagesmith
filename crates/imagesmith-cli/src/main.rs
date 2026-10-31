use clap::{Parser, Subcommand};
use imagesmith_core::build::{self, CancelToken, Options, Update};
use imagesmith_core::events::{Event, Line};
use imagesmith_core::flash;
use imagesmith_core::probe::Probe;
use imagesmith_core::project::Project;
use imagesmith_core::recipe::{Arch, PartitionRef};
use imagesmith_core::runner::{Runtime, RuntimeStatus};
use imagesmith_core::settings::Settings;
use imagesmith_core::size::human;
use imagesmith_core::tracker::{BuildState, StageStatus};
use imagesmith_core::validate::{has_errors, resolve_project_path, Severity};
use imagesmith_core::{store, Error};
use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "imagesmith", version, about = "Customize Linux disk images from a declarative recipe")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a project folder with a starter recipe from a base image
    Init {
        dir: PathBuf,
        #[arg(long)]
        base: PathBuf,
        #[arg(long)]
        name: Option<String>,
        /// Skip importing and probing the base (no container runtime needed)
        #[arg(long)]
        no_probe: bool,
    },
    /// Validate recipe.toml
    Validate {
        #[arg(default_value = ".")]
        project: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Import the base image into the store and show its partitions
    Probe {
        #[arg(default_value = ".")]
        project: PathBuf,
    },
    /// Build the image
    Build {
        #[arg(default_value = ".")]
        project: PathBuf,
        /// Print the raw event stream as JSON lines
        #[arg(long)]
        json: bool,
        /// Only extract the base into the stage (Prepare, Extract, Snapshot)
        #[arg(long)]
        extract_only: bool,
    },
    /// List past builds of a project
    Builds {
        #[arg(default_value = ".")]
        project: PathBuf,
    },
    /// Retry exporting artifacts of a build whose export failed
    RetryExport { project: PathBuf, build_id: String },
    /// Write a built image to an SD card or USB device
    Flash {
        #[arg(default_value = ".")]
        project: PathBuf,
        /// Image to write; defaults to the newest successful build's image
        #[arg(long)]
        image: Option<PathBuf>,
        /// Target device, e.g. /dev/disk4 or /dev/sdb
        #[arg(long)]
        device: Option<String>,
        /// List candidate devices and exit
        #[arg(long)]
        list: bool,
        /// Include internal disks when listing
        #[arg(long)]
        all: bool,
        /// Read the card back and compare checksums
        #[arg(long)]
        verify: bool,
        /// Eject the device when finished
        #[arg(long)]
        eject: bool,
        /// Don't ask for confirmation
        #[arg(long, short = 'y')]
        yes: bool,
        /// Allow flashing an internal, non-removable disk
        #[arg(long)]
        force: bool,
    },
    /// Open a shell chrooted into the project's stage directory
    Shell {
        #[arg(default_value = ".")]
        project: PathBuf,
    },
    /// Container runtime and store container
    Runtime {
        #[command(subcommand)]
        cmd: RuntimeCmd,
    },
    /// Store volume usage and cache management
    Store {
        #[command(subcommand)]
        cmd: StoreCmd,
    },
}

#[derive(Subcommand)]
enum RuntimeCmd {
    /// Show runtime health and store container state
    Status,
    /// Build the builder image and create the store volume and container
    Setup,
    /// Stop the store container (the volume persists)
    Stop,
    /// Delete and recreate the store container (the volume persists)
    Recreate,
}

#[derive(Subcommand)]
enum StoreCmd {
    /// Show store usage per entry
    Df,
    /// Evict least-recently-used bases and rootfs caches above the cap
    Prune {
        #[arg(long)]
        cap_gb: Option<u64>,
    },
    /// Delete one entry (kind: base, rootfs, apt, stage, tmp)
    Evict { kind: String, key: String },
}

fn main() -> ExitCode {
    // The privileged writer re-enters this same binary; it must not go through
    // clap because it is spawned by `flash`, not typed by a user.
    let args: Vec<String> = std::env::args().collect();
    if imagesmith_core::flash::is_helper_invocation(&args) {
        return ExitCode::from(imagesmith_core::flash::helper_main(&args) as u8);
    }
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn runtime() -> (Settings, Runtime) {
    let s = Settings::load();
    let rt = Runtime::from_settings(&s);
    (s, rt)
}

fn note(msg: &str) {
    eprintln!("· {msg}");
}

fn progress_line(label: &str, done: u64, total: u64) {
    if std::io::stderr().is_terminal() {
        let pct = if total > 0 { done * 100 / total } else { 0 };
        eprint!("\r· {label} {pct:>3}% ({} / {})   ", human(done), human(total));
        if done >= total {
            eprintln!();
        }
    }
}

fn prep_updates(u: Update) {
    match u {
        Update::Note(m) => note(m),
        Update::Hashing(d, t) => progress_line("hashing base", d, t),
        Update::Importing(d, t) => progress_line("importing base", d, t),
        Update::Diagnostics(d) => print_diagnostics(d),
        _ => {}
    }
}

fn print_diagnostics(d: &[imagesmith_core::validate::Diagnostic]) {
    for x in d {
        let tag = match x.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        eprintln!("{tag}: {}: {}", x.field, x.message);
        if let Some(h) = &x.hint {
            eprintln!("    hint: {}", h.replace('\n', "\n          "));
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode, Error> {
    match cli.cmd {
        Cmd::Init { dir, base, name, no_probe } => {
            let base_abs = base.canonicalize()?;
            std::fs::create_dir_all(&dir)?;
            let dir = dir.canonicalize()?;
            let name = name.unwrap_or_else(|| dir.file_name().unwrap().to_string_lossy().into_owned());
            let base_str = match base_abs.strip_prefix(&dir) {
                Ok(rel) => rel.to_string_lossy().into_owned(),
                Err(_) => base_abs.to_string_lossy().into_owned(),
            };
            let (mut arch, mut part) = (Arch::Arm64, PartitionRef::Auto);
            if !no_probe {
                let (_, rt) = runtime();
                rt.ensure_store(&mut |l| note(l))?;
                let (_, probe) = build::prepare_base(&rt, &base_abs, &mut prep_updates)?;
                print_probe(&probe);
                if let Some(r) = &probe.rootfs {
                    part = PartitionRef::Number(r.partition);
                    if r.arch.as_deref() == Some("amd64") {
                        arch = Arch::Amd64;
                    }
                }
            }
            let p = Project::create(&dir, &name, &base_str, arch, part)?;
            println!("created {} (project id {})", p.path.display(), p.id);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Validate { project, json } => {
            let p = Project::open(&project)?;
            let rf = p.load_recipe()?;
            let d = build::validate_project(&p, &rf.recipe);
            if json {
                println!("{}", serde_json::to_string_pretty(&d)?);
            } else if d.is_empty() {
                println!("recipe is valid");
            } else {
                print_diagnostics(&d);
            }
            Ok(if has_errors(&d) { ExitCode::FAILURE } else { ExitCode::SUCCESS })
        }
        Cmd::Probe { project } => {
            let p = Project::open(&project)?;
            let rf = p.load_recipe()?;
            let (_, rt) = runtime();
            rt.ensure_store(&mut |l| note(l))?;
            let base = resolve_project_path(&p.path, &rf.recipe.image.base);
            let (_, probe) = build::prepare_base(&rt, &base, &mut prep_updates)?;
            print_probe(&probe);
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Build { project, json, extract_only } => cmd_build(project, json, extract_only),
        Cmd::Builds { project } => {
            let p = Project::open(&project)?;
            for b in p.builds() {
                println!(
                    "{}  {:<10} {:>6}  {}",
                    b.record.id,
                    format!("{:?}", b.record.result),
                    fmt_ms(b.record.duration_ms),
                    b.dir.display()
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::RetryExport { project, build_id } => {
            let p = Project::open(&project)?;
            let (_, rt) = runtime();
            rt.ensure_store(&mut |l| note(l))?;
            build::retry_export(&rt, &p, &build_id)?;
            println!("exported");
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Flash { project, image, device, list, all, verify, eject, yes, force } => {
            cmd_flash(project, image, device, list, all, verify, eject, yes, force)
        }
        Cmd::Shell { project } => {
            let p = Project::open(&project)?;
            let (_, rt) = runtime();
            rt.ensure_store(&mut |l| note(l))?;
            let st = rt
                .cmd()
                .args(["exec", "-it", imagesmith_core::runner::STORE, "/usr/local/lib/imagesmith/shell.sh", &p.id])
                .status()?;
            eprintln!("· uncaptured changes stay in the stage and are overwritten by the next build");
            Ok(if st.success() { ExitCode::SUCCESS } else { ExitCode::FAILURE })
        }
        Cmd::Runtime { cmd } => {
            let (_, rt) = runtime();
            match cmd {
                RuntimeCmd::Status => {
                    match rt.status() {
                        RuntimeStatus::Ready => println!("runtime: ready ({})", rt.bin.display()),
                        RuntimeStatus::Starting => println!("runtime: starting"),
                        RuntimeStatus::Unavailable(m) => {
                            println!("runtime: unavailable: {m}");
                            return Ok(ExitCode::FAILURE);
                        }
                    }
                    println!("builder image: {} ({})", rt.builder_image(), if rt.builder_image_present()? { "present" } else { "missing" });
                    match rt.store_info()? {
                        Some(i) => println!(
                            "store container: {} · builder {} · {} CPUs · {} GB · rosetta {}",
                            if i.running { "running" } else { "stopped" },
                            i.builder_version.as_deref().unwrap_or("?"),
                            i.cpus.map_or("?".into(), |c| c.to_string()),
                            i.memory_gb.map_or("?".into(), |c| c.to_string()),
                            if i.rosetta { "on" } else { "off" },
                        ),
                        None => println!("store container: not created"),
                    }
                }
                RuntimeCmd::Setup => {
                    if rt.status() != RuntimeStatus::Ready {
                        note("starting container system …");
                        rt.start_system()?;
                    }
                    rt.ensure_store(&mut |l| eprintln!("{l}"))?;
                    println!("store ready");
                }
                RuntimeCmd::Stop => rt.stop_store()?,
                RuntimeCmd::Recreate => {
                    rt.delete_store()?;
                    rt.ensure_store(&mut |l| note(l))?;
                    println!("store container recreated");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Cmd::Store { cmd } => {
            let (s, rt) = runtime();
            rt.ensure_store(&mut |l| note(l))?;
            match cmd {
                StoreCmd::Df => {
                    let u = store::usage(&rt)?;
                    println!("volume: {} used of {} ({} free)", human(u.used), human(u.total), human(u.avail));
                    let mut entries = u.entries;
                    entries.sort_by(|a, b| (a.kind.as_str(), b.last_used).cmp(&(b.kind.as_str(), a.last_used)));
                    for e in entries {
                        println!("{:<7} {:<66} {:>10}", e.kind, e.key, human(e.bytes));
                    }
                }
                StoreCmd::Prune { cap_gb } => {
                    let cap = cap_gb.unwrap_or(s.cache_cap_gb) << 30;
                    let v = store::prune(&rt, cap, &[])?;
                    for e in &v {
                        println!("evicted {} {} ({})", e.kind, e.key, human(e.bytes));
                    }
                    if v.is_empty() {
                        println!("under the {} cap; nothing to evict", human(cap));
                    }
                }
                StoreCmd::Evict { kind, key } => {
                    store::evict(&rt, &kind, &key)?;
                    println!("evicted {kind} {key}");
                }
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn print_probe(p: &Probe) {
    println!("{}", p.summary());
    for part in &p.partitions {
        let root = p.rootfs.as_ref().is_some_and(|r| r.partition == part.number);
        println!(
            "  {}  {:<8} {:<10} {:>10}{}",
            part.number,
            part.label.as_deref().unwrap_or("-"),
            part.fstype.as_deref().unwrap_or("-"),
            human(part.size_bytes),
            if root { "  root partition" } else { "" }
        );
    }
    if let Some(r) = &p.rootfs {
        if let Some(s) = &r.resize_service {
            println!("  resize service: {s}");
        }
        for c in &r.caps {
            println!("  file caps: {} {}", c.path, c.caps);
        }
    }
}

fn fmt_ms(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

fn status_icon(s: StageStatus) -> &'static str {
    match s {
        StageStatus::Pending => "○",
        StageStatus::Running => "◌",
        StageStatus::Done => "✓",
        StageStatus::Warn => "▲",
        StageStatus::Failed => "✗",
        StageStatus::Skipped => "–",
        StageStatus::Cached => "⚡",
    }
}

/// Newest successful build's image artifact.
fn latest_image(p: &Project) -> Option<PathBuf> {
    const EXTS: [&str; 4] = [".img", ".img.zst", ".img.xz", ".img.gz"];
    p.builds()
        .into_iter()
        .filter(|b| b.record.result == BuildState::Succeeded)
        .find_map(|b| {
            let mut found: Vec<PathBuf> = std::fs::read_dir(&b.dir)
                .ok()?
                .flatten()
                .map(|e| e.path())
                .filter(|f| {
                    let n = f.to_string_lossy();
                    EXTS.iter().any(|e| n.ends_with(e))
                })
                .collect();
            found.sort();
            found.into_iter().next()
        })
}

fn confirm(prompt: &str) -> bool {
    eprint!("{prompt} [y/N] ");
    let _ = std::io::stderr().flush();
    let mut s = String::new();
    std::io::stdin().read_line(&mut s).is_ok() && matches!(s.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[allow(clippy::too_many_arguments)]
fn cmd_flash(
    project: PathBuf,
    image: Option<PathBuf>,
    device: Option<String>,
    list: bool,
    all: bool,
    verify: bool,
    eject: bool,
    yes: bool,
    force: bool,
) -> Result<ExitCode, Error> {
    let devices = flash::list_devices(all || force)?;
    if list || device.is_none() {
        if devices.is_empty() {
            eprintln!("no removable devices found; insert an SD card or pass --all to see every disk");
        } else {
            println!("{:<10} {:>10}  {}", "DEVICE", "SIZE", "DESCRIPTION");
            for d in &devices {
                let flag = if d.is_protected() { "  (protected)" } else { "" };
                println!("{:<10} {:>10}  {}{}", d.node.display(), human(d.size_bytes), d.label(), flag);
            }
        }
        if list {
            return Ok(ExitCode::SUCCESS);
        }
        return Err(Error::Other("pass --device to choose a target".into()));
    }

    let image = match image {
        Some(i) => i,
        None => {
            let p = Project::open(&project)?;
            latest_image(&p).ok_or_else(|| Error::Other("no built image found; pass --image".into()))?
        }
    };
    let req = flash::Request { image, device: PathBuf::from(device.unwrap()), verify, eject, force };
    let (src, dev) = req.check()?;

    note(&format!("image  {}", req.image.display()));
    note(&format!(
        "size   {} compressed → {}",
        human(src.compressed_bytes),
        src.uncompressed_bytes.map(human).unwrap_or_else(|| "unknown".into())
    ));
    note(&format!("target {}", dev.label()));
    if !dev.mounts.is_empty() {
        note(&format!("mounted at {}", dev.mounts.join(", ")));
    }
    if !yes && !confirm(&format!("Erase {} and write the image?", dev.label())) {
        eprintln!("aborted");
        return Ok(ExitCode::FAILURE);
    }
    let elevation = flash::cli_elevation();
    if elevation == flash::Elevation::Sudo {
        note("flashing needs administrator rights; sudo may ask for your password");
    }

    let cancel = CancelToken::default();
    let started = std::time::Instant::now();
    let bytes = flash::flash(&req, elevation, &cancel, &mut |u| match u {
        flash::Progress::Stage(t) => {
            if std::io::stderr().is_terminal() {
                eprintln!();
            }
            note(t);
        }
        flash::Progress::Note(m) => note(m),
        flash::Progress::Bytes { phase, done, total } => match total {
            Some(t) => progress_line(phase, done, t),
            None if std::io::stderr().is_terminal() => eprint!("\r· {phase} {}   ", human(done)),
            None => {}
        },
    })?;
    if std::io::stderr().is_terminal() {
        eprintln!();
    }
    let secs = started.elapsed().as_secs_f64().max(0.001);
    println!("wrote {} in {} ({}/s)", human(bytes), fmt_ms(started.elapsed().as_millis() as u64), human((bytes as f64 / secs) as u64));
    Ok(ExitCode::SUCCESS)
}

fn cmd_build(project: PathBuf, json: bool, extract_only: bool) -> Result<ExitCode, Error> {
    let p = Project::open(&project)?;
    let rf = p.load_recipe()?;
    let (settings, rt) = runtime();

    let mut passwords = HashMap::new();
    for u in build::prompt_users(&rf.recipe) {
        let a = rpassword::prompt_password(format!("password for {u}: "))?;
        let b = rpassword::prompt_password(format!("confirm password for {u}: "))?;
        if a != b {
            return Err(Error::Other(format!("passwords for {u} do not match")));
        }
        if a.is_empty() {
            return Err(Error::Other(format!("password for {u} is empty")));
        }
        passwords.insert(u, a);
    }

    let pending: Vec<_> = if extract_only { vec![] } else { imagesmith_core::host::unapproved(&p, &rf.recipe).into_iter().cloned().collect() };
    if !pending.is_empty() {
        eprintln!("This recipe runs scripts on this Mac (outside the container):");
        for s in &pending {
            eprintln!("\n── {} ({})\n{}", s.name, if s.when == imagesmith_core::recipe::StepWhen::First { "before the build" } else { "after export" }, s.run.trim_end());
        }
        eprintln!();
        if !std::io::stdin().is_terminal() || !confirm("Run these scripts?") {
            eprintln!("aborted");
            return Ok(ExitCode::FAILURE);
        }
        imagesmith_core::host::approve(&p, &pending)?;
    }

    let cancel = CancelToken::default();
    let c = cancel.clone();
    let mut hits = 0;
    ctrlc::set_handler(move || {
        hits += 1;
        if hits > 1 {
            std::process::exit(130);
        }
        eprintln!("\n· cancelling (Ctrl-C again to abandon)");
        c.cancel();
    })
    .map_err(|e| Error::Other(e.to_string()))?;

    let opts = Options { extract_only, passwords, cache_cap_bytes: Some(settings.cache_cap_gb << 30) };
    let mut last_progress = std::time::Instant::now();
    let outcome = build::run(&rt, &p, &opts, &cancel, &mut |u| match u {
        Update::Line(line, tracker) => {
            if json {
                if let Line::Event(e) = line {
                    println!("{}", serde_json::to_string(e).unwrap_or_default());
                }
                return;
            }
            match line {
                Line::Log(text) => println!("{text}"),
                Line::Event(Event::Stage { index, total, title, parent, .. }) => {
                    if parent.is_some() {
                        println!("\n── Steps · {title}");
                    } else {
                        println!("\n━━ [{index}/{total}] {title}");
                    }
                }
                Line::Event(Event::Progress { done, total: Some(t), unit, .. }) => {
                    if last_progress.elapsed().as_millis() > 1000 || done >= t {
                        last_progress = std::time::Instant::now();
                        let shown = if unit == "bytes" { format!("{} / {}", human(*done), human(*t)) } else { format!("{done} / {t} {unit}") };
                        println!("   … {shown}");
                    }
                }
                Line::Event(Event::Cache { hit: true, key }) => println!("   cached rootfs {key}"),
                Line::Event(Event::Warn { message, hint }) => {
                    println!("warning: {message}");
                    if let Some(h) = hint {
                        println!("    hint: {h}");
                    }
                }
                Line::Event(Event::Error { message, hint, .. }) => {
                    println!("error: {message}");
                    if let Some(h) = hint {
                        println!("    hint: {h}");
                    }
                }
                Line::Event(Event::Done { .. }) => {
                    println!();
                    for s in &tracker.stages {
                        let indent = if s.parent.is_some() { "    " } else { "" };
                        let dur = match (s.status, s.duration_ms) {
                            (StageStatus::Cached, _) => "cached".to_string(),
                            (_, Some(d)) => fmt_ms(d),
                            _ => String::new(),
                        };
                        println!("{indent}{} {:<28} {dur:>8}", status_icon(s.status), s.title);
                    }
                }
                _ => {}
            }
        }
        Update::State(s) => {
            if !json && s == BuildState::StartingRuntime {
                note("starting runtime");
            }
        }
        other => prep_updates(other),
    });
    let _ = std::io::stdout().flush();

    let outcome = match outcome {
        Ok(o) => o,
        Err(Error::Invalid(n)) => {
            eprintln!("error: recipe has {n} blocking error(s)");
            return Ok(ExitCode::FAILURE);
        }
        Err(e) => return Err(e),
    };
    if let Some(e) = &outcome.error {
        eprintln!("error: {e}");
    }
    match outcome.state {
        BuildState::Succeeded => {
            if let Some(dir) = &outcome.dir {
                eprintln!("✓ build {} succeeded → {}", outcome.build_id, dir.display());
                for a in &outcome.tracker.artifacts {
                    eprintln!("    {:<10} {:>10}  {}", a.kind, human(a.bytes), a.path);
                }
            } else {
                eprintln!("✓ stage extracted for project {}", p.id);
            }
            Ok(ExitCode::SUCCESS)
        }
        BuildState::Cancelled => {
            eprintln!("build {} cancelled", outcome.build_id);
            Ok(ExitCode::from(130))
        }
        _ => {
            if let Some(f) = &outcome.tracker.failure {
                eprintln!("✗ build {} failed in {}: {}", outcome.build_id, f.stage, f.message);
                if let Some(h) = &f.hint {
                    eprintln!("    hint: {h}");
                }
            }
            Ok(ExitCode::FAILURE)
        }
    }
}
