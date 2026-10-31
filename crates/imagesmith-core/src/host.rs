//! Recipe `[[host_scripts]]`: bash scripts run on the Mac in the project folder,
//! `first` before the build and `last` after export. Each script must be approved
//! by the user once per project and content before it runs.

use crate::build::CancelToken;
use crate::project::Project;
use crate::recipe::{Recipe, Step, StepWhen};
use crate::{Error, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

fn approvals_file() -> PathBuf {
    crate::app_dirs::support_dir().join("approved-host-scripts.json")
}

fn digest(project: &Project, s: &Step) -> String {
    let mut h = Sha256::new();
    h.update(project.id.as_bytes());
    h.update([0]);
    h.update(project.path.as_os_str().as_encoded_bytes());
    h.update([0]);
    h.update(s.run.as_bytes());
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn load_approvals() -> BTreeSet<String> {
    std::fs::read(approvals_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn enabled(r: &Recipe, when: StepWhen) -> Vec<&Step> {
    r.host_scripts.iter().filter(|s| s.enabled && s.when == when).collect()
}

/// Enabled scripts the user has not approved in their current form.
pub fn unapproved<'a>(project: &Project, r: &'a Recipe) -> Vec<&'a Step> {
    let ok = load_approvals();
    r.host_scripts.iter().filter(|s| s.enabled && !ok.contains(&digest(project, s))).collect()
}

pub fn approve(project: &Project, scripts: &[Step]) -> Result<()> {
    let mut ok = load_approvals();
    ok.extend(scripts.iter().map(|s| digest(project, s)));
    let path = approvals_file();
    std::fs::create_dir_all(crate::app_dirs::support_dir())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&ok)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Runs `scripts` in order with bash in the project folder, feeding each output line to `out`.
pub fn run(project: &Project, scripts: &[&Step], env: &[(&str, String)], cancel: &CancelToken, out: &mut dyn FnMut(String)) -> Result<()> {
    // Apps started from Finder get a minimal PATH.
    let mut path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin:/usr/sbin:/sbin".into());
    for extra in ["/usr/local/bin", "/opt/homebrew/bin"] {
        if !path.split(':').any(|p| p == extra) {
            path = format!("{extra}:{path}");
        }
    }
    for s in scripts {
        out(format!("── Mac script: {} ──", s.name));
        let mut child = Command::new("/bin/bash")
            .arg("-c")
            .arg(format!("exec 2>&1\nset -ex\n{}", s.run))
            .current_dir(&project.path)
            .env("PATH", &path)
            .envs(env.iter().map(|(k, v)| (*k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()?;
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let (tx, rx) = mpsc::channel::<String>();
        let reader = std::thread::spawn(move || {
            let mut buf = Vec::new();
            while stdout.read_until(b'\n', &mut buf).is_ok_and(|n| n > 0) {
                let line = String::from_utf8_lossy(&buf).trim_end_matches(['\n', '\r']).to_string();
                buf.clear();
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut killed = false;
        loop {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(l) => out(l),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if cancel.is_cancelled() && !killed {
                // SAFETY: signals the script's own process group.
                unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGTERM) };
                killed = true;
            }
        }
        let st = child.wait()?;
        let _ = reader.join();
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if !st.success() {
            let how = st.code().map_or("was killed".to_string(), |c| format!("exited with {c}"));
            return Err(Error::Other(format!("Mac script \"{}\" {how}", s.name)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(name: &str, run: &str) -> Step {
        Step { name: name.into(), run: run.into(), enabled: true, when: StepWhen::First }
    }

    #[test]
    fn runs_in_project_dir_with_env_and_fails_on_error() {
        let d = tempfile::tempdir().unwrap();
        let p = Project { path: d.path().canonicalize().unwrap(), id: "t".into() };
        let ok = step("ok", "echo \"$IMAGESMITH_TEST\" > out.txt\necho done");
        let mut lines = Vec::new();
        run(&p, &[&ok], &[("IMAGESMITH_TEST", "hi".into())], &CancelToken::default(), &mut |l| lines.push(l)).unwrap();
        assert_eq!(std::fs::read_to_string(d.path().join("out.txt")).unwrap(), "hi\n");
        assert!(lines.iter().any(|l| l == "done"));
        let bad = step("bad", "false\necho unreachable");
        let e = run(&p, &[&bad], &[], &CancelToken::default(), &mut |_| {}).unwrap_err();
        assert!(e.to_string().contains("\"bad\" exited with 1"), "{e}");
    }
}
