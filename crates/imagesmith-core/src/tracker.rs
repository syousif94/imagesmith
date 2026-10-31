//! Folds the event stream into stage states and a tagged log buffer. Drives
//! both live builds and replays from events.jsonl.

use crate::events::{Event, Line};
use crate::recipe::{Recipe, StepWhen};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

pub const STAGES: [(&str, &str); 10] = [
    ("prepare", "Prepare"),
    ("extract", "Extract"),
    ("snapshot", "Snapshot"),
    ("first", "First Steps"),
    ("packages", "Packages"),
    ("configure", "Configure"),
    ("files", "Files"),
    ("steps", "Steps"),
    ("assemble", "Assemble"),
    ("package", "Package"),
];

pub const LOG_CAP: usize = 200_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StageStatus {
    Pending,
    Running,
    Done,
    Warn,
    Failed,
    Skipped,
    Cached,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    pub done: u64,
    pub total: Option<u64>,
    pub unit: String,
}

#[derive(Debug, Clone)]
pub struct StageState {
    pub id: String,
    pub title: String,
    pub parent: Option<String>,
    pub status: StageStatus,
    pub progress: Option<Progress>,
    pub started_ms: Option<u64>,
    pub duration_ms: Option<u64>,
    /// Absolute log line range (start, end) for scrolling to this stage.
    pub log_range: (u64, u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuildState {
    Queued,
    StartingRuntime,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone)]
pub struct LogLine {
    pub stage: Option<String>,
    pub text: String,
}

/// Ring of log lines; `first` is the absolute index of the oldest kept line.
#[derive(Debug, Default)]
pub struct LogBuffer {
    pub lines: VecDeque<LogLine>,
    pub first: u64,
}

impl LogBuffer {
    pub fn push(&mut self, l: LogLine) {
        if self.lines.len() == LOG_CAP {
            self.lines.pop_front();
            self.first += 1;
        }
        self.lines.push_back(l);
    }

    pub fn next_index(&self) -> u64 {
        self.first + self.lines.len() as u64
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactInfo {
    pub kind: String,
    pub path: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub stage: String,
    pub message: String,
    pub hint: Option<String>,
    pub log_line: u64,
}

pub struct Tracker {
    pub state: BuildState,
    pub stages: Vec<StageState>,
    pub log: LogBuffer,
    pub artifacts: Vec<ArtifactInfo>,
    pub warnings: Vec<(String, Option<String>)>,
    pub failure: Option<Failure>,
    pub duration_ms: Option<u64>,
    current: Option<usize>,
}

impl Tracker {
    /// Pre-populates the stages plus one row per enabled recipe step.
    pub fn new(recipe: Option<&Recipe>) -> Tracker {
        let mut stages = Vec::new();
        for (id, title) in STAGES {
            // Without first steps the stage would only ever show as skipped.
            if id == "first" && !recipe.is_some_and(|r| r.steps.iter().any(|s| s.enabled && s.when == StepWhen::First)) {
                continue;
            }
            stages.push(StageState::new(id, title, None));
            let (prefix, when) = match id {
                "first" => ("first", StepWhen::First),
                "steps" => ("step", StepWhen::Last),
                _ => continue,
            };
            if let Some(r) = recipe {
                for (i, s) in r.steps.iter().filter(|s| s.enabled && s.when == when).enumerate() {
                    stages.push(StageState::new(&format!("{prefix}-{}", i + 1), &s.name, Some(id)));
                }
            }
        }
        Tracker {
            state: BuildState::Queued,
            stages,
            log: LogBuffer::default(),
            artifacts: Vec::new(),
            warnings: Vec::new(),
            failure: None,
            duration_ms: None,
            current: None,
        }
    }

    fn index_of(&self, id: &str) -> Option<usize> {
        self.stages.iter().position(|s| s.id == id)
    }

    pub fn current_stage(&self) -> Option<&StageState> {
        self.current.map(|i| &self.stages[i])
    }

    fn finish_current(&mut self, now_ms: u64) {
        if let Some(i) = self.current {
            let at = self.log.next_index();
            let s = &mut self.stages[i];
            if s.status == StageStatus::Running {
                s.status = StageStatus::Done;
            }
            s.duration_ms = s.started_ms.map(|t| now_ms.saturating_sub(t));
            s.log_range.1 = at;
        }
    }

    pub fn apply(&mut self, line: &Line, now_ms: u64) {
        match line {
            Line::Log(text) => {
                let stage = self.current.map(|i| self.stages[i].id.clone());
                self.log.push(LogLine { stage, text: text.clone() });
            }
            Line::Event(e) => self.apply_event(e, now_ms),
        }
    }

    pub fn apply_event(&mut self, e: &Event, now_ms: u64) {
        match e {
            Event::Stage { id, title, parent, .. } => {
                self.state = BuildState::Running;
                let entering_child = parent.is_some();
                let cur_parent = self.current.and_then(|i| self.stages[i].parent.clone());
                self.finish_current(now_ms);
                // Leaving the steps group: close the parent too.
                if !entering_child {
                    if let Some(p) = cur_parent.and_then(|p| self.index_of(&p)) {
                        let at = self.log.next_index();
                        let s = &mut self.stages[p];
                        if s.status == StageStatus::Running {
                            s.status = StageStatus::Done;
                        }
                        s.duration_ms = s.started_ms.map(|t| now_ms.saturating_sub(t));
                        s.log_range.1 = at;
                    }
                }
                let i = match self.index_of(id) {
                    Some(i) => i,
                    None => {
                        let pos = parent
                            .as_ref()
                            .and_then(|p| {
                                let pi = self.index_of(p)?;
                                Some(pi + 1 + self.stages[pi + 1..].iter().take_while(|s| s.parent.as_deref() == Some(p)).count())
                            })
                            .unwrap_or(self.stages.len());
                        self.stages.insert(pos, StageState::new(id, title, parent.as_deref()));
                        pos
                    }
                };
                // Mark stages before this one (that never reported) as skipped.
                for s in &mut self.stages[..i] {
                    if s.status == StageStatus::Pending && s.parent.is_none() && Some(&s.id) != parent.as_ref() {
                        s.status = StageStatus::Skipped;
                    }
                }
                let at = self.log.next_index();
                let s = &mut self.stages[i];
                s.title = title.clone();
                if s.status != StageStatus::Cached {
                    s.status = StageStatus::Running;
                }
                s.started_ms = Some(now_ms);
                s.log_range = (at, at);
                if let Some(p) = parent.as_ref().and_then(|p| self.index_of(p)) {
                    let ps = &mut self.stages[p];
                    if ps.status == StageStatus::Pending {
                        ps.status = StageStatus::Running;
                        ps.started_ms = Some(now_ms);
                        ps.log_range = (at, at);
                    }
                }
                self.current = Some(i);
                self.log.push(LogLine { stage: Some(id.clone()), text: format!("── {title} ──") });
            }
            Event::Progress { stage, done, total, unit } => {
                if let Some(i) = self.index_of(stage) {
                    self.stages[i].progress = Some(Progress { done: *done, total: *total, unit: unit.clone() });
                }
            }
            Event::Warn { message, hint } => {
                if let Some(i) = self.current {
                    self.stages[i].status = StageStatus::Warn;
                }
                self.warnings.push((message.clone(), hint.clone()));
                let stage = self.current.map(|i| self.stages[i].id.clone());
                self.log.push(LogLine { stage, text: format!("warning: {message}") });
            }
            Event::Artifact { kind, path, bytes } => {
                self.artifacts.push(ArtifactInfo { kind: kind.clone(), path: path.clone(), bytes: *bytes });
            }
            Event::Cache { hit, .. } => {
                if *hit {
                    if let Some(i) = self.index_of("extract") {
                        self.stages[i].status = StageStatus::Cached;
                    }
                }
            }
            Event::Error { stage, message, hint } => {
                let at = self.log.next_index();
                if let Some(i) = self.index_of(stage).or(self.current) {
                    self.stages[i].status = StageStatus::Failed;
                    if let Some(p) = self.stages[i].parent.clone().and_then(|p| self.index_of(&p)) {
                        self.stages[p].status = StageStatus::Failed;
                    }
                }
                let first_error = self
                    .log
                    .lines
                    .iter()
                    .enumerate()
                    .rev()
                    .take_while(|(_, l)| l.stage.as_deref() == Some(stage.as_str()))
                    .filter(|(_, l)| looks_like_error(&l.text))
                    .last()
                    .map(|(i, _)| self.log.first + i as u64)
                    .unwrap_or(at);
                self.failure = Some(Failure { stage: stage.clone(), message: message.clone(), hint: hint.clone(), log_line: first_error });
                self.log.push(LogLine { stage: Some(stage.clone()), text: format!("error: {message}") });
            }
            Event::Done { ok, duration_ms } => {
                if !ok {
                    // Interrupted mid-stage (cancel, crash): the running stage and its parent didn't finish.
                    for s in &mut self.stages {
                        if s.status == StageStatus::Running {
                            s.status = StageStatus::Failed;
                        }
                    }
                }
                self.finish_current(now_ms);
                for s in &mut self.stages {
                    if s.status == StageStatus::Running {
                        s.status = if *ok { StageStatus::Done } else { StageStatus::Failed };
                    }
                }
                self.duration_ms = Some(*duration_ms);
                self.state = match (ok, &self.failure) {
                    (true, _) => BuildState::Succeeded,
                    (false, Some(_)) => BuildState::Failed,
                    (false, None) => BuildState::Cancelled,
                };
                self.current = None;
            }
        }
    }

    /// Overall fraction: stages done plus the current stage's byte progress.
    pub fn overall_progress(&self) -> f64 {
        let top: Vec<&StageState> = self.stages.iter().filter(|s| s.parent.is_none()).collect();
        let finished = top
            .iter()
            .filter(|s| matches!(s.status, StageStatus::Done | StageStatus::Warn | StageStatus::Cached | StageStatus::Skipped))
            .count() as f64;
        let partial = top
            .iter()
            .find(|s| s.status == StageStatus::Running)
            .and_then(|s| s.progress.as_ref())
            .and_then(|p| p.total.filter(|t| *t > 0).map(|t| p.done as f64 / t as f64))
            .unwrap_or(0.0);
        ((finished + partial.min(1.0)) / top.len() as f64).min(1.0)
    }
}

impl StageState {
    fn new(id: &str, title: &str, parent: Option<&str>) -> StageState {
        StageState {
            id: id.into(),
            title: title.into(),
            parent: parent.map(String::from),
            status: StageStatus::Pending,
            progress: None,
            started_ms: None,
            duration_ms: None,
            log_range: (0, 0),
        }
    }
}

pub fn looks_like_error(l: &str) -> bool {
    let l = l.to_ascii_lowercase();
    l.starts_with("e:") || l.contains("error") || l.contains("failed") || l.contains("fatal")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::parse_line;

    #[test]
    fn folds_a_build() {
        let (_, r) = crate::recipe::parse(
            "[image]\nname='x'\nbase='b.img'\n[[steps]]\nname='A'\nrun='true'\n[[steps]]\nname='B'\nrun='false'\n",
        )
        .unwrap();
        let mut t = Tracker::new(Some(&r));
        assert_eq!(t.stages.len(), 11);
        let lines = [
            r#"@@{"type":"stage","id":"prepare","title":"Prepare","index":1,"total":9}"#,
            "hello",
            r#"@@{"type":"stage","id":"extract","title":"Extract","index":2,"total":9}"#,
            r#"@@{"type":"cache","hit":true,"key":"k"}"#,
            r#"@@{"type":"stage","id":"steps","title":"Steps","index":7,"total":9}"#,
            r#"@@{"type":"stage","id":"step-1","title":"A","index":7,"total":9,"parent":"steps"}"#,
            r#"@@{"type":"stage","id":"step-2","title":"B","index":7,"total":9,"parent":"steps"}"#,
            "boom: error here",
            r#"@@{"type":"error","stage":"step-2","message":"step B exited 1","hint":null}"#,
            r#"@@{"type":"done","ok":false,"duration_ms":10}"#,
        ];
        for (i, l) in lines.iter().enumerate() {
            t.apply(&parse_line(l), i as u64 * 100);
        }
        let st = |id: &str| t.stages.iter().find(|s| s.id == id).unwrap().status;
        assert_eq!(st("prepare"), StageStatus::Done);
        assert_eq!(st("extract"), StageStatus::Cached);
        assert_eq!(st("snapshot"), StageStatus::Skipped);
        assert_eq!(st("step-1"), StageStatus::Done);
        assert_eq!(st("step-2"), StageStatus::Failed);
        assert_eq!(st("steps"), StageStatus::Failed);
        assert_eq!(st("assemble"), StageStatus::Pending);
        assert_eq!(t.state, BuildState::Failed);
        let f = t.failure.as_ref().unwrap();
        assert_eq!(t.log.lines[(f.log_line - t.log.first) as usize].text, "boom: error here");
    }

    #[test]
    fn first_steps_get_their_own_stage() {
        let (_, r) = crate::recipe::parse(
            "[image]\nname='x'\nbase='b.img'\n[[steps]]\nname='Late'\nrun='true'\n[[steps]]\nname='Early'\nrun='true'\nwhen='first'\n",
        )
        .unwrap();
        let t = Tracker::new(Some(&r));
        let ids: Vec<&str> = t.stages.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(&ids[2..6], ["snapshot", "first", "first-1", "packages"]);
        assert!(ids.contains(&"step-1") && !ids.contains(&"step-2"));
    }
}
