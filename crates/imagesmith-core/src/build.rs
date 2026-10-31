//! One build, end to end: runtime, base import and probe, validation, the
//! remote build.sh run with its event stream, artifact export, and records.

use crate::events::{parse_line, Event, Line};
use crate::hashing::sha256_cached;
use crate::probe::Probe;
use crate::project::{new_build_id, BuildRecord, Project, ProjectIndex};
use crate::recipe::{PasswordMode, Recipe};
use crate::resolve::{self, Lock};
use crate::runner::Runtime;
use crate::size::human;
use crate::tracker::{BuildState, Tracker};
use crate::validate::{self, has_errors, resolve_project_path, Context, Diagnostic, Severity};
use crate::{Error, Result, BUILDER_VERSION};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
pub struct Options {
    /// Run only Prepare, Extract and Snapshot (Files tab "Extract base to browse").
    pub extract_only: bool,
    /// Passwords for users with `password = "prompt"`; sent on stdin, never stored.
    pub passwords: HashMap<String, String>,
    pub cache_cap_bytes: Option<u64>,
}

pub enum Update<'a> {
    State(BuildState),
    /// Host-side preparation message (runtime, hashing, import).
    Note(&'a str),
    Hashing(u64, u64),
    Importing(u64, u64),
    Diagnostics(&'a [Diagnostic]),
    /// A line from build.sh, already folded into `tracker`.
    Line(&'a Line, &'a Tracker),
}

pub struct Outcome {
    pub build_id: String,
    pub state: BuildState,
    /// builds/<id>/ when a record was written.
    pub dir: Option<PathBuf>,
    pub tracker: Tracker,
    pub error: Option<String>,
}

/// Recipe users needing a password at build time.
pub fn prompt_users(r: &Recipe) -> Vec<String> {
    r.users.iter().filter(|u| u.password == PasswordMode::Prompt).map(|u| u.name.clone()).collect()
}

/// Validation with everything known locally: cached hash and cached probe.
pub fn validate_project(project: &Project, recipe: &Recipe) -> Vec<Diagnostic> {
    let base = resolve_project_path(&project.path, &recipe.image.base);
    let sha = crate::hashing::cached_sha256(&base);
    let probe = sha.as_deref().and_then(Probe::load_cached);
    validate::validate(
        &project.path,
        recipe,
        &Context { base_sha256: sha.as_deref(), probe: probe.as_ref(), rosetta: None },
    )
}

fn host_free_bytes(path: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
}

/// Hashes, imports and probes the base. Shared by build and New Project.
pub fn prepare_base(rt: &Runtime, base: &Path, on: &mut dyn FnMut(Update)) -> Result<(String, Probe)> {
    let sha = sha256_cached(base, |d, t| on(Update::Hashing(d, t)))?;
    rt.import_base(&sha, base, &mut |d, t| on(Update::Importing(d, t)))?;
    let probe = match Probe::load_cached(&sha) {
        Some(p) => p,
        None => {
            on(Update::Note("Probing base image …"));
            rt.probe(&sha)?
        }
    };
    Ok((sha, probe))
}

pub fn run(rt: &Runtime, project: &Project, opts: &Options, cancel: &CancelToken, on: &mut dyn FnMut(Update)) -> Result<Outcome> {
    let started_at = chrono::Local::now().to_rfc3339();
    on(Update::State(BuildState::StartingRuntime));
    let store = rt.ensure_store(&mut |l| on(Update::Note(l)))?;

    let rf = project.load_recipe()?;
    let recipe = rf.recipe;
    let base = resolve_project_path(&project.path, &recipe.image.base);

    let pre = validate::validate(&project.path, &recipe, &Context::default());
    if has_errors(&pre) {
        on(Update::Diagnostics(&pre));
        return Err(Error::Invalid(pre.iter().filter(|d| d.severity == Severity::Error).count()));
    }
    let (sha, probe) = prepare_base(rt, &base, on)?;
    let diags = validate::validate(
        &project.path,
        &recipe,
        &Context { base_sha256: Some(&sha), probe: Some(&probe), rosetta: Some(store.rosetta) },
    );
    on(Update::Diagnostics(&diags));
    if has_errors(&diags) {
        return Err(Error::Invalid(diags.iter().filter(|d| d.severity == Severity::Error).count()));
    }
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }

    let mut passwords = HashMap::new();
    for u in prompt_users(&recipe) {
        let pw = opts.passwords.get(&u).ok_or_else(|| Error::Other(format!("password for {u} was not provided")))?;
        passwords.insert(u, pw.clone());
    }

    let build_id = new_build_id();
    let resolved = resolve::resolve(&project.path, &project.id, &build_id, &recipe, &sha, &probe)?;

    // Space: 3× the uncompressed base in the store, the output on the host.
    let usage = crate::store::usage(rt)?;
    let cap = opts.cache_cap_bytes.unwrap_or(20 << 30);
    for e in crate::store::prune(rt, cap, &[("base", &sha), ("rootfs", &resolved.cache_key)])? {
        on(Update::Note(&format!("Evicted {} {} ({})", e.kind, e.key, human(e.bytes))));
    }
    let need = 3 * probe.image_bytes;
    if usage.avail < need {
        return Err(Error::Other(format!(
            "store volume low on space: need {}, have {} (Settings → Caches)",
            human(need),
            human(usage.avail)
        )));
    }
    if let Some(free) = host_free_bytes(&project.path) {
        let need = probe.image_bytes / if recipe.output.compress == crate::recipe::Compress::None { 1 } else { 2 };
        if !opts.extract_only && free < need {
            return Err(Error::Other(format!("project disk low on space: need {}, have {}", human(need), human(free))));
        }
    }

    // Inputs and log files are staged in the app cache; builds/<id>/ is
    // created only once the build has finished.
    let staging = crate::app_dirs::cache_dir().join("builds").join(&build_id);
    std::fs::create_dir_all(&staging)?;
    let tar_path = staging.join("input.tar");
    resolve::write_input_tar(&project.path, &recipe, &resolved, std::io::BufWriter::new(std::fs::File::create(&tar_path)?))?;
    let tar_len = std::fs::metadata(&tar_path)?.len();
    let mut log = std::io::BufWriter::new(std::fs::File::create(staging.join("build.log"))?);
    let mut events = std::io::BufWriter::new(std::fs::File::create(staging.join("events.jsonl"))?);

    let mut args = vec!["build.sh", "--project-id", project.id.as_str(), "--build-id", build_id.as_str()];
    if opts.extract_only {
        args.push("--extract-only");
    }
    let mut child = rt
        .exec(true, &args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    // stdin: framed tar, then one JSON line of passwords.
    let mut stdin = child.stdin.take().unwrap();
    let pw_line = serde_json::to_string(&serde_json::json!({ "passwords": passwords }))?;
    drop(passwords);
    let tar_for_writer = tar_path.clone();
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        writeln!(stdin, "IMAGESMITH-INPUT 1 {tar_len}")?;
        std::io::copy(&mut std::fs::File::open(tar_for_writer)?, &mut stdin)?;
        writeln!(stdin, "{pw_line}")?;
        stdin.flush()
    });

    let (tx, rx) = mpsc::channel::<(bool, String)>();
    let finished = Arc::new(AtomicBool::new(false));
    let mut readers = Vec::new();
    for (is_stdout, stream) in [
        (true, Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>),
        (false, Box::new(child.stderr.take().unwrap())),
    ] {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(stream).lines().map_while(|l| l.ok()) {
                if tx.send((is_stdout, line)).is_err() {
                    break;
                }
            }
        }));
    }
    drop(tx);

    let mut tracker = Tracker::new(Some(&recipe));
    tracker.state = BuildState::Running;
    on(Update::State(BuildState::Running));
    let start = Instant::now();
    let mut kill_thread = None;
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok((is_stdout, raw)) => {
                let line = if is_stdout { parse_line(&raw) } else { Line::Log(raw) };
                let t = start.elapsed().as_millis() as u64;
                match &line {
                    Line::Event(e) => writeln!(events, "{}", serde_json::json!({ "t": t, "event": e }))?,
                    Line::Log(text) => {
                        writeln!(log, "{text}")?;
                        writeln!(events, "{}", serde_json::json!({ "t": t, "log": text }))?;
                    }
                }
                tracker.apply(&line, t);
                on(Update::Line(&line, &tracker));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if cancel.is_cancelled() && kill_thread.is_none() {
            on(Update::Note("Cancelling …"));
            let rt = rt.clone();
            let id = build_id.clone();
            let finished = finished.clone();
            kill_thread = Some(std::thread::spawn(move || {
                let _ = rt.kill_build(&id, &mut || !finished.load(Ordering::SeqCst));
            }));
        }
    }
    finished.store(true, Ordering::SeqCst);
    let status = child.wait()?;
    let _ = writer.join();
    for r in readers {
        let _ = r.join();
    }
    if let Some(k) = kill_thread {
        let _ = k.join();
    }
    let _ = std::fs::remove_file(&tar_path);

    let elapsed = start.elapsed().as_millis() as u64;
    let mut error = None;
    if tracker.duration_ms.is_none() {
        // build.sh died without a done event (OOM, KILL, runtime crash).
        let (msg, hint) = match status.code() {
            Some(137) => ("builder was killed (out of memory?)".to_string(), Some("Increase memory in Settings → Resources".to_string())),
            _ if cancel.is_cancelled() => ("cancelled".to_string(), None),
            code => (format!("build.sh exited with {code:?}"), None),
        };
        if !cancel.is_cancelled() {
            let stage = tracker.current_stage().map(|s| s.id.clone()).unwrap_or_else(|| "prepare".into());
            tracker.apply_event(&Event::Error { stage, message: msg.clone(), hint }, elapsed);
            error = Some(msg);
        }
        tracker.apply_event(&Event::Done { ok: false, duration_ms: elapsed }, elapsed);
    }
    if cancel.is_cancelled() && tracker.state != BuildState::Succeeded {
        tracker.state = BuildState::Cancelled;
    }
    log.flush()?;
    events.flush()?;
    drop(log);
    drop(events);

    if opts.extract_only {
        on(Update::State(tracker.state));
        return Ok(Outcome { build_id, state: tracker.state, dir: None, tracker, error });
    }

    let dir = project.builds_dir().join(&build_id);
    std::fs::create_dir_all(&dir)?;
    let mut export_failed = false;
    if tracker.state == BuildState::Succeeded {
        for a in tracker.artifacts.clone() {
            on(Update::Note(&format!("Exporting {} ({})", a.path, human(a.bytes))));
            if let Err(e) = rt.export(&build_id, &a.path, &dir.join(&a.path), a.bytes) {
                tracker.state = BuildState::Failed;
                export_failed = true;
                error = Some(format!("{e}; the build's temp directory is kept so export can be retried"));
                break;
            }
        }
        let lock = Lock { recipe: &recipe, resolved: &resolved, base_sha256: &sha, builder_version: BUILDER_VERSION };
        std::fs::write(dir.join("recipe.lock.json"), serde_json::to_vec_pretty(&lock)?)?;
    }
    if !export_failed {
        let _ = crate::store::evict(rt, "tmp", &build_id);
    }
    for f in ["build.log", "events.jsonl"] {
        move_file(&staging.join(f), &dir.join(f))?;
    }
    let _ = std::fs::remove_dir(&staging);
    let record = BuildRecord {
        id: build_id.clone(),
        started_at,
        duration_ms: tracker.duration_ms.unwrap_or(elapsed),
        result: tracker.state,
    };
    std::fs::write(dir.join("build.json"), serde_json::to_vec_pretty(&record)?)?;
    let _ = ProjectIndex::load().set_last_build(&project.id, &build_id);
    on(Update::State(tracker.state));
    Ok(Outcome { build_id, state: tracker.state, dir: Some(dir), tracker, error })
}

/// Retries artifact export for a build whose temp directory still exists.
pub fn retry_export(rt: &Runtime, project: &Project, build_id: &str) -> Result<()> {
    let dir = project.builds_dir().join(build_id);
    let tracker = replay(&dir.join("events.jsonl"), None)?;
    for a in &tracker.artifacts {
        let dest = dir.join(&a.path);
        if std::fs::metadata(&dest).map(|m| m.len()).ok() != Some(a.bytes) {
            rt.export(build_id, &a.path, &dest, a.bytes)?;
        }
    }
    crate::store::evict(rt, "tmp", build_id)
}

fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_err() {
        std::fs::copy(from, to)?;
        std::fs::remove_file(from)?;
    }
    Ok(())
}

/// Rebuilds the build view of a past build from its events.jsonl.
pub fn replay(events: &Path, recipe: Option<&Recipe>) -> Result<Tracker> {
    let mut t = Tracker::new(recipe);
    let f = std::fs::File::open(events)?;
    for line in BufReader::new(f).lines() {
        let v: serde_json::Value = serde_json::from_str(&line?)?;
        let at = v["t"].as_u64().unwrap_or(0);
        if let Some(text) = v["log"].as_str() {
            t.apply(&Line::Log(text.to_string()), at);
        } else if let Ok(e) = serde_json::from_value::<Event>(v["event"].clone()) {
            t.apply_event(&e, at);
        }
    }
    Ok(t)
}
