// SPDX-License-Identifier: GPL-3.0-only
// Project Zomboid RAM Configurator - native GUI front end.
// Copyright (C) 2023-2026 Minidoracat
// This program comes with ABSOLUTELY NO WARRANTY. See the LICENSE file for details.
#![windows_subsystem = "windows"]

mod backend;
mod i18n;

slint::include_modules!();

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use slint::{ComponentHandle, ModelRc, SharedString, VecModel, Weak};

use backend::{AppError, MemoryInfo, Mode, RestorePlan, Snapshot};
use i18n::I18n;

/// The only file this tool ever rewrites.
const CONFIG_NAME: &str = "ProjectZomboid64.json";
/// Where the corresponding source can be obtained, as required by the GPL.
const SOURCE_URL: &str = "https://github.com/Minidoracat/project-zomboid-ram-configurator";
/// The licence text is embedded so the running program can always display it.
/// Redistribution additionally requires offering the corresponding source, for
/// which `SOURCE_URL` is published in the about dialog.
const LICENSE_TEXT: &str = concat!(
    include_str!("../../LICENSE"),
    "\n\n--- Noto Sans TC / SC — SIL Open Font License 1.1 ---\n\n",
    include_str!("../assets/fonts/OFL.txt"),
    "\n\n--- Lucide icons — ISC / MIT notices ---\n\n",
    include_str!("../assets/icons/LICENSE.txt"),
);
const BYTES_PER_GIB: u64 = 1024 * 1024 * 1024;

// ---------------------------------------------------------------------------
// messages
// ---------------------------------------------------------------------------

/// A piece of user-visible text that is resolved at render time, so switching
/// the language never loses state and never leaves a stale sentence behind.
#[derive(Clone)]
enum Msg {
    /// Plain key.
    Key(&'static str),
    /// Key with `{name}` parameters. Values are language neutral: numbers,
    /// formatted sizes and raw user paths.
    Fill(&'static str, Vec<(String, String)>),
    /// Text that must not be translated, such as a file system path.
    Raw(String),
    /// Title of a backend error code.
    ErrTitle(&'static str),
    /// Recovery hint for a backend error code.
    ErrFix(&'static str),
}

fn fill(key: &'static str, params: &[(&str, String)]) -> Msg {
    Msg::Fill(
        key,
        params
            .iter()
            .map(|(name, value)| ((*name).to_string(), value.clone()))
            .collect(),
    )
}

/// A bordered block of text: `kind` is 1 info, 2 done, 3 warning, 4 error.
#[derive(Clone)]
struct Note {
    kind: i32,
    title: Msg,
    lines: Vec<Msg>,
}

// ---------------------------------------------------------------------------
// state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModeSel {
    Heavy,
    Balanced,
    Custom,
}

impl ModeSel {
    fn index(self) -> i32 {
        match self {
            ModeSel::Heavy => 0,
            ModeSel::Balanced => 1,
            ModeSel::Custom => 2,
        }
    }

    fn from_index(index: i32) -> Option<Self> {
        match index {
            0 => Some(ModeSel::Heavy),
            1 => Some(ModeSel::Balanced),
            2 => Some(ModeSel::Custom),
            _ => None,
        }
    }
}

/// What we know about the `.bak` next to the current config.
enum BackupState {
    /// Nothing checked yet (before the first read).
    Unknown,
    /// No backup on disk; the first apply will create one.
    Missing,
    Usable {
        path: PathBuf,
        limit_bytes: u64,
        age_secs: Option<u64>,
    },
    /// A backup exists but cannot be used, e.g. its JSON is damaged.
    Unusable { path: PathBuf, err: AppError },
}

/// The modal currently on screen. Each variant owns the data the confirmation
/// will act on, so a confirmation can never pick up newer, unreviewed content.
enum Dialog {
    Apply {
        gb: u64,
        snapshot: Snapshot,
    },
    Restore {
        plan: RestorePlan,
        age_secs: Option<u64>,
    },
    Install {
        options: Vec<PathBuf>,
    },
    About,
}

impl Dialog {
    fn kind(&self) -> i32 {
        match self {
            Dialog::Apply { .. } => 0,
            Dialog::Restore { .. } => 1,
            Dialog::Install { .. } => 2,
            Dialog::About => 3,
        }
    }
}

struct State {
    i18n: I18n,
    /// True once the first detection pass finished.
    detected: bool,
    memory: Option<MemoryInfo>,
    path: Option<PathBuf>,
    snapshot: Option<Snapshot>,
    config_error: Option<AppError>,
    backup: BackupState,
    mode: ModeSel,
    custom_text: String,
    custom_error: Option<Msg>,
    dialog: Option<Dialog>,
    busy: bool,
    /// Set when a close request was refused because work is in flight, so the
    /// busy line can say why the window stayed open.
    close_blocked: bool,
    status: Option<Note>,
    alert: Option<Note>,
}

impl State {
    fn new(i18n: I18n) -> Self {
        Self {
            i18n,
            detected: false,
            memory: None,
            path: None,
            snapshot: None,
            config_error: None,
            backup: BackupState::Unknown,
            mode: ModeSel::Heavy,
            custom_text: String::new(),
            custom_error: None,
            dialog: None,
            busy: false,
            close_blocked: false,
            status: Some(Note {
                kind: 1,
                title: Msg::Key("status.detecting"),
                lines: Vec::new(),
            }),
            alert: None,
        }
    }

    // ---- text helpers ----------------------------------------------------

    fn resolve(&self, msg: &Msg) -> String {
        match msg {
            Msg::Key(key) => self.i18n.t(key).to_string(),
            Msg::Fill(key, params) => {
                let borrowed: Vec<(&str, &str)> = params
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.as_str()))
                    .collect();
                self.i18n.tp(key, &borrowed)
            }
            Msg::Raw(text) => text.clone(),
            Msg::ErrTitle(code) => self.error_text(code, "title"),
            Msg::ErrFix(code) => self.error_text(code, "fix"),
        }
    }

    fn error_text(&self, code: &str, part: &str) -> String {
        let key = format!("err.{code}.{part}");
        if self.i18n.has(&key) {
            self.i18n.t(&key).to_string()
        } else {
            self.i18n.t(&format!("err.unknown.{part}")).to_string()
        }
    }

    /// A size in GB, using the locale's unit pattern.
    fn gb_text(&self, gb: u64) -> String {
        self.i18n.tp("value.gb", &[("value", &gb.to_string())])
    }

    /// A byte count rendered in GB: exact when whole, two decimals otherwise.
    fn bytes_text(&self, bytes: u64) -> String {
        let gb = bytes as f64 / BYTES_PER_GIB as f64;
        let rounded = gb.round();
        let number = if (gb - rounded).abs() < 0.005 {
            format!("{}", rounded as u64)
        } else {
            format!("{gb:.2}")
        };
        self.i18n.tp("value.gb", &[("value", &number)])
    }

    fn age_text(&self, age_secs: Option<u64>) -> String {
        match age_secs {
            None => self.i18n.t("age.unknown").to_string(),
            Some(secs) if secs < 60 => self.i18n.t("age.justNow").to_string(),
            Some(secs) if secs < 3600 => self
                .i18n
                .tp("age.minutes", &[("value", &(secs / 60).to_string())]),
            Some(secs) if secs < 86_400 => self
                .i18n
                .tp("age.hours", &[("value", &(secs / 3600).to_string())]),
            Some(secs) => self
                .i18n
                .tp("age.days", &[("value", &(secs / 86_400).to_string())]),
        }
    }

    fn current_limit_text(&self) -> String {
        match &self.snapshot {
            Some(snapshot) => self.bytes_text(snapshot.heap_bytes),
            None => self.i18n.t("memory.unreadable").to_string(),
        }
    }

    // ---- derived facts ---------------------------------------------------

    fn total_bytes(&self) -> Option<u64> {
        self.memory.map(|info| info.total_bytes)
    }

    fn available_bytes(&self) -> Option<u64> {
        self.memory.map(|info| info.available_bytes)
    }

    fn recommendation(&self, mode: Mode) -> Option<u64> {
        self.total_bytes()
            .and_then(|total| backend::recommendation(total, mode))
    }

    /// The value that Apply would write, or `None` when there is nothing valid
    /// to write yet. Custom input is judged by the backend, so the UI never
    /// keeps a second copy of the validation rules.
    fn pending_gb(&self) -> Option<u64> {
        match self.mode {
            ModeSel::Heavy => self.recommendation(Mode::Heavy),
            ModeSel::Balanced => self.recommendation(Mode::Balanced),
            ModeSel::Custom => self
                .total_bytes()
                .and_then(|total| backend::validate_gb(&self.custom_text, total).ok()),
        }
    }

    fn can_write(&self) -> bool {
        self.snapshot.is_some() && self.total_bytes().is_some()
    }

    fn can_restore(&self) -> bool {
        matches!(self.backup, BackupState::Usable { .. })
    }

    /// Recommendation that is used for warnings and for the chart tick: the
    /// heavy-mod value, which is the strategy the tool leads with.
    fn warnings(&self, gb: u64) -> Vec<Msg> {
        let mut out = Vec::new();
        let value = self.gb_text(gb);
        if self.mode == ModeSel::Balanced {
            out.push(Msg::Key("warning.heavyNeeds"));
        }
        if let Some(rec) = self.recommendation(Mode::Heavy)
            && gb > rec
        {
            out.push(fill(
                "warning.above",
                &[("value", value.clone()), ("limit", self.gb_text(rec))],
            ));
        }
        let bytes = gb.saturating_mul(BYTES_PER_GIB);
        if let Some(snapshot) = &self.snapshot
            && bytes < snapshot.heap_bytes
        {
            out.push(fill(
                "warning.lower",
                &[
                    ("value", value.clone()),
                    ("current", self.bytes_text(snapshot.heap_bytes)),
                ],
            ));
        }
        if let Some(free) = self.available_bytes()
            && bytes as f64 >= free as f64 * 0.9
        {
            out.push(fill(
                "warning.free",
                &[("value", value.clone()), ("free", self.bytes_text(free))],
            ));
        }
        out
    }

    fn error_note(&self, err: &AppError, extra: &[Msg]) -> Note {
        let mut lines = vec![Msg::ErrFix(err.code)];
        lines.extend(extra.iter().cloned());
        if !err.detail.is_empty() {
            lines.push(fill("error.detail", &[("detail", err.detail.clone())]));
        }
        Note {
            kind: 4,
            title: Msg::ErrTitle(err.code),
            lines,
        }
    }

    /// Keeps the selected strategy usable when recommendations disappear
    /// (physical RAM below 8 GB, or memory detection failed).
    fn normalize_mode(&mut self) {
        let unusable = match self.mode {
            ModeSel::Heavy => self.recommendation(Mode::Heavy).is_none(),
            ModeSel::Balanced => self.recommendation(Mode::Balanced).is_none(),
            ModeSel::Custom => false,
        };
        if unusable {
            self.mode = ModeSel::Custom;
        }
    }

    fn backup_file_name(&self) -> Option<String> {
        let path = match &self.backup {
            BackupState::Usable { path, .. } => path,
            BackupState::Unusable { path, .. } => path,
            _ => return None,
        };
        Some(
            path.file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| path.display().to_string()),
        )
    }

    // ---- transitions -----------------------------------------------------

    /// Applies a finished background job and optionally asks for a follow-up.
    fn absorb(&mut self, done: Done) -> Option<Job> {
        self.busy = false;
        match done {
            Done::Detected {
                memory,
                installs,
                fixed_dir,
            } => {
                self.detected = true;
                match memory {
                    Ok(info) => self.memory = Some(info),
                    Err(err) => {
                        self.memory = None;
                        self.alert =
                            Some(self.error_note(&err, &[Msg::Key("memory.writeDisabled")]));
                        self.status = None;
                    }
                }
                self.normalize_mode();

                if let Some(dir) = fixed_dir {
                    let path = dir.join(CONFIG_NAME);
                    self.path = Some(path.clone());
                    self.status = Some(Note {
                        kind: 1,
                        title: Msg::Key("location.fixedDir"),
                        lines: vec![Msg::Raw(path.display().to_string())],
                    });
                    return Some(Job::Read {
                        path,
                        announce: false,
                    });
                }

                match installs {
                    Ok(found) if found.len() == 1 => {
                        let path = found[0].join(CONFIG_NAME);
                        self.path = Some(path.clone());
                        self.status = None;
                        return Some(Job::Read {
                            path,
                            announce: false,
                        });
                    }
                    Ok(found) if found.len() > 1 => {
                        self.status = None;
                        self.dialog = Some(Dialog::Install { options: found });
                    }
                    Ok(_) => {
                        self.status = Some(Note {
                            kind: 1,
                            title: Msg::Key("status.notfound"),
                            lines: vec![Msg::Key("status.notfoundHint")],
                        });
                    }
                    Err(err) => {
                        self.alert = Some(self.error_note(&err, &[]));
                        self.status = None;
                    }
                }
                None
            }

            Done::Read {
                path,
                config,
                backup,
                announce,
            } => {
                self.path = Some(path);
                match config {
                    Ok(snapshot) => {
                        self.snapshot = Some(snapshot);
                        self.config_error = None;
                        if announce {
                            self.status = Some(Note {
                                kind: 2,
                                title: Msg::Key("status.reloadOk"),
                                lines: vec![fill(
                                    "status.current",
                                    &[("value", self.current_limit_text())],
                                )],
                            });
                            self.alert = None;
                        }
                    }
                    Err(err) => {
                        self.snapshot = None;
                        self.config_error = Some(err);
                        if announce {
                            self.status = Some(Note {
                                kind: 1,
                                title: Msg::Key("status.currentUnreadable"),
                                lines: Vec::new(),
                            });
                        }
                    }
                }
                self.backup = backup;
                self.normalize_mode();
                None
            }

            Done::Applied {
                result,
                unchanged,
                backup,
                backup_path,
            } => {
                match result {
                    Ok(snapshot) => {
                        let value = self.bytes_text(snapshot.heap_bytes);
                        self.snapshot = Some(snapshot);
                        self.config_error = None;
                        self.alert = None;
                        self.status = Some(if unchanged {
                            Note {
                                kind: 1,
                                title: fill("status.same", &[("value", value)]),
                                lines: Vec::new(),
                            }
                        } else {
                            Note {
                                kind: 2,
                                title: fill("status.applied", &[("value", value)]),
                                lines: vec![
                                    fill(
                                        "status.backup",
                                        &[("path", backup_path.display().to_string())],
                                    ),
                                    Msg::Key("status.nextLaunch"),
                                ],
                            }
                        });
                    }
                    Err(err) => {
                        self.alert = Some(self.error_note(&err, &[]));
                        self.status = None;
                    }
                }
                self.backup = backup;
                None
            }

            Done::Prepared { result, age_secs } => {
                match result {
                    Ok(plan) => {
                        self.alert = None;
                        self.dialog = Some(Dialog::Restore { plan, age_secs });
                    }
                    Err(err) => {
                        self.alert = Some(self.error_note(&err, &[]));
                        self.status = None;
                    }
                }
                None
            }

            Done::Restored {
                result,
                backup,
                backup_path,
            } => {
                match result {
                    Ok(snapshot) => {
                        let value = self.bytes_text(snapshot.heap_bytes);
                        self.snapshot = Some(snapshot);
                        self.config_error = None;
                        self.alert = None;
                        self.status = Some(Note {
                            kind: 2,
                            title: fill("status.restored", &[("value", value)]),
                            lines: vec![
                                fill(
                                    "status.backupKept",
                                    &[("path", backup_path.display().to_string())],
                                ),
                                Msg::Key("status.restoreOverwrote"),
                            ],
                        });
                    }
                    Err(err) => {
                        self.alert = Some(self.error_note(&err, &[]));
                        self.status = None;
                    }
                }
                self.backup = backup;
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// background work
// ---------------------------------------------------------------------------

enum Job {
    Detect {
        fixed_dir: Option<PathBuf>,
    },
    Read {
        path: PathBuf,
        announce: bool,
    },
    Apply {
        snapshot: Snapshot,
        gb: u64,
        total_bytes: u64,
    },
    Prepare {
        path: PathBuf,
    },
    Restore {
        plan: RestorePlan,
    },
}

enum Done {
    Detected {
        memory: Result<MemoryInfo, AppError>,
        installs: Result<Vec<PathBuf>, AppError>,
        fixed_dir: Option<PathBuf>,
    },
    Read {
        path: PathBuf,
        config: Result<Snapshot, AppError>,
        backup: BackupState,
        announce: bool,
    },
    Applied {
        result: Result<Snapshot, AppError>,
        unchanged: bool,
        backup: BackupState,
        backup_path: PathBuf,
    },
    Prepared {
        result: Result<RestorePlan, AppError>,
        /// Age of the backup, measured in the worker so that rendering the
        /// confirmation never touches the file system.
        age_secs: Option<u64>,
    },
    Restored {
        result: Result<Snapshot, AppError>,
        backup: BackupState,
        backup_path: PathBuf,
    },
}

fn age_secs(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now()
        .duration_since(modified)
        .ok()
        .map(|elapsed| elapsed.as_secs())
}

/// Inspect the backup itself; target write permissions are checked when restoring.
fn backup_state(config: &Path) -> BackupState {
    let path = backend::backup_path(config);
    match path.try_exists() {
        Ok(false) => return BackupState::Missing,
        Err(err) => {
            return BackupState::Unusable {
                err: AppError {
                    code: "io",
                    detail: format!("Cannot inspect backup '{}': {err}", path.display()),
                },
                path,
            };
        }
        Ok(true) => {}
    }
    match backend::read_config(&path) {
        Ok(snapshot) => BackupState::Usable {
            age_secs: age_secs(&path),
            limit_bytes: snapshot.heap_bytes,
            path,
        },
        Err(err) => BackupState::Unusable { path, err },
    }
}

fn run_job(job: Job) -> Done {
    match job {
        Job::Detect { fixed_dir } => {
            let memory = backend::system_memory();
            let installs = if fixed_dir.is_some() {
                Ok(Vec::new())
            } else {
                backend::find_installations()
            };
            Done::Detected {
                memory,
                installs,
                fixed_dir,
            }
        }
        Job::Read { path, announce } => {
            let config = backend::read_config(&path);
            let backup = backup_state(&path);
            Done::Read {
                path,
                config,
                backup,
                announce,
            }
        }
        Job::Apply {
            snapshot,
            gb,
            total_bytes,
        } => {
            let backup_path = backend::backup_path(&snapshot.path);
            let written = backend::write_limit(&snapshot, gb, total_bytes);
            // Equal bytes means the file was already correct: no write, no
            // backup rotation.
            let unchanged = match &written {
                Ok(result) => result.bytes == snapshot.bytes,
                Err(_) => false,
            };
            let backup = backup_state(&snapshot.path);
            Done::Applied {
                result: written,
                unchanged,
                backup,
                backup_path,
            }
        }
        Job::Prepare { path } => {
            let result = backend::prepare_restore(&path);
            let age = result
                .as_ref()
                .ok()
                .and_then(|plan| age_secs(&plan.backup.path));
            Done::Prepared {
                result,
                age_secs: age,
            }
        }
        Job::Restore { plan } => {
            let config_path = plan.path.clone();
            let backup_path = plan.backup.path.clone();
            let result = backend::restore_config(&plan);
            let backup = backup_state(&config_path);
            Done::Restored {
                result,
                backup,
                backup_path,
            }
        }
    }
}

fn worker(
    jobs: Receiver<Job>,
    window: Weak<AppWindow>,
    state: Arc<Mutex<State>>,
    queue: Sender<Job>,
) {
    while let Ok(job) = jobs.recv() {
        let done = run_job(job);
        let focus = match &done {
            Done::Applied { .. } => Some(0),
            Done::Restored { .. } => Some(1),
            Done::Prepared { result: Err(_), .. } => Some(1),
            _ => None,
        };
        let state = Arc::clone(&state);
        let queue = queue.clone();
        let posted = window.upgrade_in_event_loop(move |ui| {
            let mut guard = lock(&state);
            if let Some(next) = guard.absorb(done) {
                dispatch(&mut guard, &queue, next);
            }
            render(&ui, &guard);
            if let Some(kind) = focus {
                ui.invoke_focus_dialog_trigger(kind);
            }
        });
        if posted.is_err() {
            break;
        }
    }
}

/// Hands a job to the worker and marks the window busy so nothing re-enters.
///
/// A closed channel means the worker thread is gone and the job never ran, so
/// the stale success is cleared and the failure is reported instead of being
/// swallowed into a window that looks like it finished.
fn dispatch(state: &mut State, queue: &Sender<Job>, job: Job) {
    match queue.send(job) {
        Ok(()) => {
            state.busy = true;
            state.close_blocked = false;
        }
        Err(err) => {
            eprintln!("background worker unavailable: {err}");
            state.busy = false;
            state.close_blocked = false;
            state.status = None;
            state.alert = Some(Note {
                kind: 4,
                title: Msg::Key("worker.gone"),
                lines: vec![
                    Msg::Key("worker.goneFix"),
                    fill("error.detail", &[("detail", err.to_string())]),
                ],
            });
        }
    }
}

// ---------------------------------------------------------------------------
// rendering
// ---------------------------------------------------------------------------

fn model(values: Vec<String>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        values
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    ))
}

fn texts(state: &State) -> Texts {
    let t = |key: &str| SharedString::from(state.i18n.t(key));
    Texts {
        window_title: t("window.title"),
        app_title: t("app.title"),
        app_subtitle: t("app.subtitle"),
        font_family: t("app.fontFamily"),
        language_label: t("app.language"),
        about_open: t("about.open"),
        location_title: t("location.title"),
        location_choose: t("location.choose"),
        location_reload: t("location.reload"),
        memory_title: t("memory.title"),
        memory_total: t("memory.total"),
        memory_free: t("memory.free"),
        memory_current: t("memory.current"),
        chart_system: t("chart.system"),
        chart_pending: t("chart.pending"),
        chart_axis_min: t("chart.axisMin"),
        strategy_title: t("strategy.title"),
        strategy_note: t("strategy.note"),
        method_label: t("strategy.methodLabel"),
        heavy_label: t("strategy.heavy"),
        heavy_summary: t("strategy.heavySummary"),
        heavy_description: t("strategy.heavyDescription"),
        balanced_label: t("strategy.balanced"),
        balanced_summary: t("strategy.balancedSummary"),
        balanced_description: t("strategy.balancedDescription"),
        custom_label: t("strategy.custom"),
        custom_summary: t("strategy.customSummary"),
        custom_description: t("strategy.customDescription"),
        custom_hint: t("strategy.customHint"),
        custom_placeholder: t("strategy.customPlaceholder"),
        unit_gb: t("unit.gb"),
        diff_label: t("diff.label"),
        action_restore: t("action.restore"),
    }
}

fn memory_view(state: &State) -> MemoryView {
    let i18n = &state.i18n;
    let total = state.total_bytes();
    let free = state.available_bytes();
    let rec_mode = if state.mode == ModeSel::Balanced {
        Mode::Balanced
    } else {
        Mode::Heavy
    };
    let rec_label = i18n.t(if state.mode == ModeSel::Balanced {
        "memory.balanced"
    } else {
        "memory.heavy"
    });
    let rec = state.recommendation(rec_mode);
    let pending = state.pending_gb();

    let mut view = MemoryView {
        total: SharedString::from(if !state.detected {
            i18n.t("memory.detecting").to_string()
        } else {
            match total {
                Some(bytes) => state.bytes_text(bytes),
                None => i18n.t("memory.unavailable").to_string(),
            }
        }),
        free: SharedString::from(if !state.detected {
            i18n.t("memory.detecting").to_string()
        } else {
            match free {
                Some(bytes) => state.bytes_text(bytes),
                None => i18n.t("memory.unavailable").to_string(),
            }
        }),
        current: SharedString::from(if !state.detected {
            i18n.t("memory.reading").to_string()
        } else if state.path.is_none() {
            i18n.t("memory.noGame").to_string()
        } else {
            state.current_limit_text()
        }),
        rec_label: SharedString::from(rec_label),
        rec_value: SharedString::from(if !state.detected {
            String::from("—")
        } else {
            match rec {
                Some(gb) => state.gb_text(gb),
                None if total.is_some() => i18n.t("memory.noRecommendation").to_string(),
                None => i18n.t("memory.unavailable").to_string(),
            }
        }),
        chart_shown: state.detected && total.is_some(),
        total_label: SharedString::new(),
        used_ratio: 0.0,
        legend_used: SharedString::new(),
        legend_free: SharedString::new(),
        pending_label: SharedString::new(),
        pending_ratio: 0.0,
        pending_shown: false,
        rec_ratio: 0.0,
        rec_shown: false,
        legend_tick: SharedString::new(),
        axis_max: SharedString::new(),
    };

    if let Some(total_bytes) = total.filter(|bytes| *bytes > 0) {
        let ratio = |bytes: u64| (bytes as f32 / total_bytes as f32).clamp(0.0, 1.0);
        let used = total_bytes.saturating_sub(free.unwrap_or(0));
        view.total_label = SharedString::from(state.bytes_text(total_bytes));
        view.axis_max = SharedString::from(state.bytes_text(total_bytes));
        view.used_ratio = ratio(used);
        view.legend_used = SharedString::from(
            state
                .i18n
                .tp("chart.used", &[("value", &state.bytes_text(used))]),
        );
        view.legend_free = SharedString::from(state.i18n.tp(
            "chart.free",
            &[("free", &state.bytes_text(free.unwrap_or(0)))],
        ));

        let pending_bytes = pending.map(|gb| gb.saturating_mul(BYTES_PER_GIB));
        let show_pending =
            state.can_write() && matches!(pending_bytes, Some(bytes) if bytes <= total_bytes);
        view.pending_shown = show_pending;
        view.pending_label = SharedString::from(match (show_pending, pending) {
            (true, Some(gb)) => state.gb_text(gb),
            _ => state.i18n.t("chart.none").to_string(),
        });
        if let Some(bytes) = pending_bytes.filter(|_| show_pending) {
            view.pending_ratio = ratio(bytes);
        }

        match rec {
            Some(gb) => {
                view.rec_shown = true;
                view.rec_ratio = ratio(gb.saturating_mul(BYTES_PER_GIB));
                view.legend_tick = SharedString::from(state.i18n.tp(
                    "chart.recommendation",
                    &[("name", rec_label), ("value", &state.gb_text(gb))],
                ));
            }
            None => {
                view.legend_tick =
                    SharedString::from(state.i18n.t("chart.noRecommendation").to_string());
            }
        }
    }

    view
}

fn strategy_view(state: &State) -> StrategyView {
    let i18n = &state.i18n;
    let heavy = state.recommendation(Mode::Heavy);
    let balanced = state.recommendation(Mode::Balanced);
    let can_write = state.can_write();
    let pending = state.pending_gb();

    // `None` means three different things: detection has not finished yet, it
    // failed, or the machine really is below the recommendation threshold.
    let note = |value: Option<u64>| match value {
        Some(gb) => state.gb_text(gb),
        None if !state.detected => i18n.t("memory.detecting").to_string(),
        None if state.total_bytes().is_none() => i18n.t("memory.unavailable").to_string(),
        None => i18n.t("strategy.unavailable").to_string(),
    };

    let mut diff_from = String::new();
    let mut diff_to = String::new();
    let mut diff_note = String::new();
    let mut diff_a11y;
    if state.busy && !state.detected {
        diff_note = i18n.t("status.detecting").to_string();
    } else if state.path.is_none() {
        diff_note = i18n.t("diff.noGame").to_string();
    } else if state.snapshot.is_none() {
        diff_note = i18n.t("diff.unreadable").to_string();
    } else if !can_write {
        diff_note = i18n.t("memory.writeDisabled").to_string();
    } else {
        let current = state.current_limit_text();
        match pending {
            None => {
                diff_note = i18n.tp("diff.noValue", &[("current", &current)]);
            }
            Some(gb) => {
                let same = state
                    .snapshot
                    .as_ref()
                    .map(|snapshot| gb.saturating_mul(BYTES_PER_GIB) == snapshot.heap_bytes)
                    .unwrap_or(false);
                diff_from = current.clone();
                if same {
                    diff_note = i18n.t("diff.same").to_string();
                } else {
                    diff_to = state.gb_text(gb);
                }
            }
        }
    }
    diff_a11y = format!("{} {}", i18n.t("diff.label"), diff_note);
    if !diff_to.is_empty() {
        diff_a11y = i18n.tp(
            "diff.changedTo",
            &[("current", &diff_from), ("next", &diff_to)],
        );
    }

    let backup_note = match &state.backup {
        BackupState::Unknown => String::new(),
        BackupState::Missing => i18n.t("backup.none").to_string(),
        BackupState::Usable {
            limit_bytes,
            age_secs,
            ..
        } => i18n.tp(
            "backup.available",
            &[
                ("name", &state.backup_file_name().unwrap_or_default()),
                ("age", &state.age_text(*age_secs)),
                ("limit", &state.bytes_text(*limit_bytes)),
            ],
        ),
        BackupState::Unusable { err, .. } => {
            // The only place this failure is shown, because Restore is disabled:
            // keep the raw detail and point at repairing or preserving the
            // backup, never at restoring from the damaged one.
            let mut text = i18n.tp(
                "backup.unusable",
                &[
                    ("name", &state.backup_file_name().unwrap_or_default()),
                    ("reason", &state.error_text(err.code, "title")),
                ],
            );
            if !err.detail.is_empty() {
                text.push('\n');
                text.push_str(&i18n.tp("error.detail", &[("detail", &err.detail)]));
            }
            text.push('\n');
            text.push_str(i18n.t("backup.unusableFix"));
            text
        }
    };

    StrategyView {
        heavy_note: SharedString::from(note(heavy)),
        balanced_note: SharedString::from(note(balanced)),
        heavy_enabled: heavy.is_some() && can_write,
        balanced_enabled: balanced.is_some() && can_write,
        custom_enabled: can_write,
        custom_error: SharedString::from(
            state
                .custom_error
                .as_ref()
                .map(|msg| state.resolve(msg))
                .unwrap_or_default(),
        ),
        diff_from: SharedString::from(diff_from),
        diff_to: SharedString::from(diff_to),
        diff_note: SharedString::from(diff_note),
        diff_a11y: SharedString::from(diff_a11y),
        apply_label: SharedString::from(match pending {
            Some(gb) if can_write => i18n.tp("action.applyValue", &[("value", &state.gb_text(gb))]),
            _ => i18n.t("action.apply").to_string(),
        }),
        apply_enabled: can_write,
        restore_enabled: state.can_restore(),
        backup_note: SharedString::from(backup_note),
    }
}

fn dialog_view(state: &State) -> (DialogView, Vec<String>, Vec<String>) {
    let i18n = &state.i18n;
    let empty = DialogView {
        open: false,
        kind: -1,
        title: SharedString::new(),
        detail: SharedString::new(),
        note: SharedString::new(),
        warn_title: SharedString::new(),
        confirm: SharedString::new(),
        confirm_enabled: false,
        cancel: SharedString::from(i18n.t("action.cancel")),
        label: SharedString::new(),
    };
    let Some(dialog) = &state.dialog else {
        return (empty, Vec::new(), Vec::new());
    };

    let mut view = DialogView {
        open: true,
        kind: dialog.kind(),
        ..empty
    };
    let mut warnings = Vec::new();
    let mut options = Vec::new();

    match dialog {
        Dialog::Apply { gb, snapshot } => {
            let value = state.gb_text(*gb);
            let same = gb.saturating_mul(BYTES_PER_GIB) == snapshot.heap_bytes;
            view.title = SharedString::from(i18n.t("apply.title"));
            view.detail = SharedString::from(if same {
                i18n.tp("apply.same", &[("value", &value)])
            } else {
                i18n.tp(
                    "apply.change",
                    &[
                        ("current", &state.bytes_text(snapshot.heap_bytes)),
                        ("next", &value),
                    ],
                )
            });
            view.note = SharedString::from(i18n.tp(
                "apply.target",
                &[("path", &snapshot.path.display().to_string())],
            ));
            warnings = state
                .warnings(*gb)
                .iter()
                .map(|msg| state.resolve(msg))
                .collect();
            if !warnings.is_empty() {
                view.warn_title = SharedString::from(i18n.t("warning.title"));
            }
            view.confirm = SharedString::from(i18n.tp("action.applyValue", &[("value", &value)]));
            view.confirm_enabled = true;
        }
        Dialog::Restore { plan, age_secs } => {
            let name = plan
                .backup
                .path
                .file_name()
                .map(|value| value.to_string_lossy().to_string())
                .unwrap_or_else(|| plan.backup.path.display().to_string());
            view.title = SharedString::from(i18n.t("restore.title"));
            // Nothing is claimed about the current target: the only facts here
            // are the backup that was just verified and the path it replaces.
            view.detail = SharedString::from(i18n.tp(
                "restore.detail",
                &[
                    ("name", &name),
                    ("age", &state.age_text(*age_secs)),
                    ("limit", &state.bytes_text(plan.backup.heap_bytes)),
                ],
            ));
            view.note = SharedString::from(format!(
                "{}\n{}",
                i18n.t("restore.note"),
                i18n.tp(
                    "apply.target",
                    &[("path", &plan.path.display().to_string())]
                )
            ));
            view.confirm = SharedString::from(i18n.t("restore.confirm"));
            view.confirm_enabled = true;
        }
        Dialog::Install { options: found } => {
            view.title = SharedString::from(i18n.t("install.title"));
            view.detail = SharedString::from(i18n.t("install.note"));
            view.label = SharedString::from(i18n.t("install.label"));
            view.confirm = SharedString::from(i18n.t("install.use"));
            view.confirm_enabled = !found.is_empty();
            options = found.iter().map(|dir| dir.display().to_string()).collect();
        }
        Dialog::About => {
            view.title = SharedString::from(i18n.t("about.title"));
            view.cancel = SharedString::from(i18n.t("action.close"));
            view.confirm = SharedString::new();
            view.confirm_enabled = false;
        }
    }

    (view, warnings, options)
}

fn render(ui: &AppWindow, state: &State) {
    let i18n = &state.i18n;
    ui.set_texts(texts(state));
    ui.set_memory(memory_view(state));
    ui.set_strategy(strategy_view(state));
    ui.set_mode(state.mode.index());
    ui.set_busy(state.busy);
    // A refused close request outranks the phase label: the user pressed the
    // close button, so the reason the window stayed open has to be visible.
    ui.set_busy_text(SharedString::from(if state.close_blocked {
        i18n.t("status.workingNoClose")
    } else if !state.detected {
        i18n.t("status.detecting")
    } else {
        i18n.t("status.working")
    }));
    // `language-index` is only written once at startup: it is two-way bound to
    // the picker, and writing it here could echo back as a selection event.
    ui.set_theme_label(SharedString::from(i18n.t("app.theme")));
    ui.set_themes(model(
        ["theme.system", "theme.light", "theme.dark"]
            .iter()
            .map(|key| i18n.t(key).to_owned())
            .collect(),
    ));

    // game location
    let (path_text, path_muted) = match (&state.path, state.detected) {
        (Some(path), _) => (path.display().to_string(), false),
        (None, false) => (i18n.t("location.loading").to_string(), true),
        (None, true) => (i18n.t("location.notfound").to_string(), true),
    };
    ui.set_path_text(SharedString::from(path_text));
    ui.set_path_muted(path_muted);

    match &state.config_error {
        Some(err) => {
            // With no backup on disk there is nothing to restore from, so the
            // hint points at the folder instead of a recovery that cannot run.
            let hint = if matches!(state.backup, BackupState::Missing) {
                Msg::Key("location.notGameRoot")
            } else {
                Msg::Key("location.invalidRecovery")
            };
            let note = state.error_note(err, &[hint]);
            ui.set_problem_title(SharedString::from(state.resolve(&note.title)));
            ui.set_problem_lines(model(
                note.lines.iter().map(|msg| state.resolve(msg)).collect(),
            ));
        }
        None => {
            ui.set_problem_title(SharedString::new());
            ui.set_problem_lines(model(Vec::new()));
        }
    }

    // live warnings for the value that would be written
    let warnings: Vec<String> = match state.pending_gb().filter(|_| state.can_write()) {
        Some(gb) => state
            .warnings(gb)
            .iter()
            .map(|msg| state.resolve(msg))
            .collect(),
        None => Vec::new(),
    };
    ui.set_warn_title(SharedString::from(if warnings.is_empty() {
        String::new()
    } else {
        i18n.t("warning.title").to_string()
    }));
    ui.set_warnings(model(warnings));

    match &state.status {
        Some(note) => {
            ui.set_status_kind(note.kind);
            ui.set_status_title(SharedString::from(state.resolve(&note.title)));
            ui.set_status_lines(model(
                note.lines.iter().map(|msg| state.resolve(msg)).collect(),
            ));
        }
        None => {
            ui.set_status_title(SharedString::new());
            ui.set_status_lines(model(Vec::new()));
        }
    }

    match &state.alert {
        Some(note) => {
            ui.set_alert_title(SharedString::from(state.resolve(&note.title)));
            ui.set_alert_lines(model(
                note.lines.iter().map(|msg| state.resolve(msg)).collect(),
            ));
        }
        None => {
            ui.set_alert_title(SharedString::new());
            ui.set_alert_lines(model(Vec::new()));
        }
    }

    let (dialog, dialog_warnings, install_options) = dialog_view(state);
    ui.set_dialog(dialog);
    ui.set_dialog_warnings(model(dialog_warnings));
    ui.set_install_options(model(install_options));

    ui.set_about_copyright(SharedString::from(i18n.t("about.copyright")));
    ui.set_about_license(SharedString::from(i18n.t("about.license")));
    ui.set_about_slint(SharedString::from(i18n.t("about.slint")));
    ui.set_about_source_label(SharedString::from(i18n.t("about.source")));
    ui.set_about_license_label(SharedString::from(i18n.t("about.licenseLabel")));
}

// ---------------------------------------------------------------------------
// command line
// ---------------------------------------------------------------------------

enum CliProblem {
    Unknown(String),
    MissingValue(String),
}

struct Cli {
    game_dir: Option<PathBuf>,
    language: Option<String>,
    help: bool,
}

fn parse_cli() -> Result<Cli, CliProblem> {
    let mut cli = Cli {
        game_dir: None,
        language: None,
        help: false,
    };
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        let name = arg.to_string_lossy().to_string();
        match name.as_str() {
            "--game-dir" => {
                let value = args
                    .next()
                    .ok_or_else(|| CliProblem::MissingValue(name.clone()))?;
                cli.game_dir = Some(PathBuf::from(value));
            }
            "--language" => {
                let value = args
                    .next()
                    .ok_or_else(|| CliProblem::MissingValue(name.clone()))?;
                cli.language = Some(value.to_string_lossy().to_string());
            }
            "--help" | "-h" => cli.help = true,
            _ => return Err(CliProblem::Unknown(name)),
        }
    }
    Ok(cli)
}

/// Shows a message before any window exists. The process is a windowed
/// application, so a dialog is the only reliable channel; stderr is written as
/// well for the case where it was started from a console.
fn show_message(title: &str, body: &str, error: bool) {
    eprintln!("{title}\n{body}");
    let _ = rfd::MessageDialog::new()
        .set_level(if error {
            rfd::MessageLevel::Error
        } else {
            rfd::MessageLevel::Info
        })
        .set_title(title)
        .set_description(body)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

/// Joins one message across every bundled language, because the language is
/// not known (or not valid) while the command line is being parsed.
fn all_languages(i18n: &I18n, key: &str, params: &[(&str, &str)]) -> String {
    i18n.every_locale(key, params).join("\n")
}

fn main() -> std::process::ExitCode {
    let cli = parse_cli();

    let i18n = match I18n::new(cli.as_ref().ok().and_then(|cli| cli.language.as_deref())) {
        Ok(i18n) => i18n,
        Err(detail) => {
            // The catalogs are compiled in, so this can only be a packaging
            // mistake. Report it verbatim instead of showing an empty window.
            show_message(
                "Cannot start / 無法啟動",
                &format!("Bundled locale resources are incomplete: {detail}"),
                true,
            );
            return std::process::ExitCode::from(3);
        }
    };

    let cli = match cli {
        Ok(cli) => cli,
        Err(problem) => {
            let body = match &problem {
                CliProblem::Unknown(arg) => all_languages(&i18n, "cli.unknownArg", &[("arg", arg)]),
                CliProblem::MissingValue(arg) => {
                    all_languages(&i18n, "cli.missingValue", &[("arg", arg)])
                }
            };
            let usage = all_languages(&i18n, "cli.usage", &[]);
            show_message(
                &all_languages(&i18n, "cli.title", &[]).replace('\n', " / "),
                &format!("{body}\n\n{usage}"),
                true,
            );
            return std::process::ExitCode::from(2);
        }
    };

    if let Some(language) = &cli.language
        && !i18n.knows(language)
    {
        let body = all_languages(
            &i18n,
            "cli.badLanguage",
            &[("value", language), ("list", &i18n.code_list())],
        );
        let usage = all_languages(&i18n, "cli.usage", &[]);
        show_message(
            &all_languages(&i18n, "cli.title", &[]).replace('\n', " / "),
            &format!("{body}\n\n{usage}"),
            true,
        );
        return std::process::ExitCode::from(2);
    }

    if cli.help {
        let body = [
            all_languages(&i18n, "cli.usage", &[]),
            all_languages(&i18n, "cli.helpGameDir", &[]),
            all_languages(&i18n, "cli.helpLanguage", &[]),
        ]
        .join("\n\n");
        show_message(
            &all_languages(&i18n, "cli.helpTitle", &[]).replace('\n', " / "),
            &body,
            false,
        );
        return std::process::ExitCode::SUCCESS;
    }

    // Captured before `i18n` moves into the window, so a startup failure can
    // still be reported in every bundled language.
    let fatal_title = all_languages(&i18n, "fatal.title", &[]).replace('\n', " / ");
    match run(i18n, cli.game_dir) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(detail) => {
            show_message(&fatal_title, &detail, true);
            std::process::ExitCode::FAILURE
        }
    }
}

/// Reads the current state under the lock, recovering from a poisoned mutex so
/// a panic in one handler cannot freeze the whole window.
fn lock(state: &Arc<Mutex<State>>) -> std::sync::MutexGuard<'_, State> {
    match state.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn run(i18n: I18n, fixed_dir: Option<PathBuf>) -> Result<(), String> {
    let ui = AppWindow::new().map_err(|err| err.to_string())?;
    let languages = i18n.labels();
    let active_language = i18n.active_index();

    let state = Arc::new(Mutex::new(State::new(i18n)));
    let (queue, jobs) = mpsc::channel::<Job>();

    ui.set_languages(model(languages));
    ui.set_language_index(active_language as i32);
    ui.set_source_url(SharedString::from(SOURCE_URL));
    ui.set_license_text(SharedString::from(LICENSE_TEXT));

    {
        // A write is a multi-step file transaction: closing the window ends the
        // process and would abandon it half way. Close requests are refused
        // while work is in flight, and the busy line says so instead of letting
        // the window look hung.
        let state = Arc::clone(&state);
        let window = ui.as_weak();
        ui.window().on_close_requested(move || {
            let mut guard = lock(&state);
            if !guard.busy {
                return slint::CloseRequestResponse::HideWindow;
            }
            guard.close_blocked = true;
            if let Some(ui) = window.upgrade() {
                render(&ui, &guard);
            }
            slint::CloseRequestResponse::KeepWindowShown
        });
    }

    {
        let state = Arc::clone(&state);
        let queue = queue.clone();
        let window = ui.as_weak();
        ui.on_choose_folder(move || {
            let ui = window.unwrap();
            let (title, start_dir) = {
                let mut guard = lock(&state);
                if guard.busy || guard.dialog.is_some() {
                    return;
                }
                // Busy keeps every background control dead while the native
                // picker owns the screen, so it cannot be opened twice.
                guard.busy = true;
                guard.close_blocked = false;
                let title = guard.i18n.t("location.choose").to_string();
                let start_dir = guard
                    .path
                    .as_ref()
                    .and_then(|path| path.parent())
                    .map(|parent| parent.to_path_buf());
                render(&ui, &guard);
                (title, start_dir)
            };

            // The lock is released first: the picker runs its own modal loop,
            // during which our own callbacks can still fire.
            let mut picker = rfd::FileDialog::new().set_title(title);
            if let Some(dir) = start_dir {
                picker = picker.set_directory(dir);
            }
            let picked = picker.pick_folder();

            let mut guard = lock(&state);
            guard.busy = false;
            let Some(dir) = picked else {
                render(&ui, &guard);
                return;
            };
            let path = dir.join(CONFIG_NAME);
            guard.path = Some(path.clone());
            guard.custom_error = None;
            guard.alert = None;
            guard.status = Some(Note {
                kind: 2,
                title: Msg::Key("status.folderOk"),
                lines: vec![Msg::Raw(path.display().to_string())],
            });
            // Whether the folder really holds a configuration is decided by the
            // read worker: it keeps the real OS error instead of turning an
            // access failure into a claim that the file is not there.
            dispatch(
                &mut guard,
                &queue,
                Job::Read {
                    path,
                    announce: false,
                },
            );
            render(&ui, &guard);
        });
    }

    {
        let state = Arc::clone(&state);
        let queue = queue.clone();
        let window = ui.as_weak();
        let fixed_dir = fixed_dir.clone();
        ui.on_reload(move || {
            let ui = window.unwrap();
            let mut guard = lock(&state);
            if guard.busy || guard.dialog.is_some() {
                return;
            }
            guard.custom_error = None;
            let job = match guard.path.clone() {
                Some(path) => Job::Read {
                    path,
                    announce: true,
                },
                None => Job::Detect {
                    fixed_dir: fixed_dir.clone(),
                },
            };
            dispatch(&mut guard, &queue, job);
            render(&ui, &guard);
        });
    }

    {
        let state = Arc::clone(&state);
        let window = ui.as_weak();
        ui.on_select_mode(move |index| {
            let ui = window.unwrap();
            let mut guard = lock(&state);
            if guard.busy || guard.dialog.is_some() {
                return;
            }
            if let Some(mode) = ModeSel::from_index(index) {
                guard.mode = mode;
                guard.custom_error = None;
            }
            render(&ui, &guard);
            if guard.mode == ModeSel::Custom {
                ui.invoke_focus_custom();
            }
        });
    }

    {
        let state = Arc::clone(&state);
        let window = ui.as_weak();
        ui.on_custom_edited(move |value| {
            let ui = window.unwrap();
            let mut guard = lock(&state);
            guard.custom_text = value.to_string();
            guard.custom_error = None;
            if guard.mode != ModeSel::Custom && guard.can_write() {
                guard.mode = ModeSel::Custom;
            }
            render(&ui, &guard);
        });
    }

    {
        let state = Arc::clone(&state);
        let window = ui.as_weak();
        ui.on_apply_clicked(move || {
            let ui = window.unwrap();
            let mut guard = lock(&state);
            if guard.busy || guard.dialog.is_some() || !guard.can_write() {
                return;
            }
            let Some(total_bytes) = guard.total_bytes() else {
                return;
            };
            if guard.mode == ModeSel::Custom {
                if let Err(err) = backend::validate_gb(&guard.custom_text, total_bytes) {
                    let typed = guard.custom_text.trim().to_string();
                    let digits = !typed.is_empty()
                        && typed.chars().all(|ch| ch.is_ascii_digit())
                        && typed.chars().any(|ch| ch != '0');
                    let message = if err.code != "invalid_value" {
                        Msg::ErrTitle(err.code)
                    } else if digits {
                        fill(
                            "validation.belowTotal",
                            &[("total", guard.bytes_text(total_bytes))],
                        )
                    } else {
                        Msg::Key("validation.integer")
                    };
                    guard.custom_error = Some(message);
                    render(&ui, &guard);
                    ui.invoke_focus_custom();
                    return;
                }
                guard.custom_error = None;
            }
            let pending = guard.pending_gb();
            let snapshot = guard.snapshot.clone();
            let (Some(gb), Some(snapshot)) = (pending, snapshot) else {
                return;
            };
            if snapshot
                .xms_bytes
                .is_some_and(|initial| initial > gb * BYTES_PER_GIB)
            {
                guard.status = None;
                guard.alert = Some(Note {
                    kind: 4,
                    title: Msg::ErrTitle("xms_conflict"),
                    lines: vec![Msg::ErrFix("xms_conflict")],
                });
                render(&ui, &guard);
                return;
            }
            guard.dialog = Some(Dialog::Apply { gb, snapshot });
            render(&ui, &guard);
        });
    }

    {
        let state = Arc::clone(&state);
        let queue = queue.clone();
        let window = ui.as_weak();
        ui.on_restore_clicked(move || {
            let ui = window.unwrap();
            let mut guard = lock(&state);
            if guard.busy || guard.dialog.is_some() || !guard.can_restore() {
                return;
            }
            let Some(path) = guard.path.clone() else {
                return;
            };
            // Re-validate the backup right before showing the confirmation, so
            // the plan that gets confirmed is the one that was just verified.
            dispatch(&mut guard, &queue, Job::Prepare { path });
            render(&ui, &guard);
        });
    }

    {
        let state = Arc::clone(&state);
        let queue = queue.clone();
        let window = ui.as_weak();
        ui.on_confirm_dialog(move || {
            let ui = window.unwrap();
            let mut guard = lock(&state);
            if guard.busy {
                return;
            }
            let Some(dialog) = guard.dialog.take() else {
                return;
            };
            match dialog {
                Dialog::Apply { gb, snapshot } => {
                    if let Some(total_bytes) = guard.total_bytes() {
                        dispatch(
                            &mut guard,
                            &queue,
                            Job::Apply {
                                snapshot,
                                gb,
                                total_bytes,
                            },
                        );
                    }
                }
                Dialog::Restore { plan, .. } => {
                    dispatch(&mut guard, &queue, Job::Restore { plan });
                }
                Dialog::Install { options } => {
                    let index = ui.get_install_index().max(0) as usize;
                    if let Some(dir) = options.get(index).or_else(|| options.first()) {
                        let path = dir.join(CONFIG_NAME);
                        guard.path = Some(path.clone());
                        guard.status = Some(Note {
                            kind: 2,
                            title: Msg::Key("status.folderOk"),
                            lines: vec![Msg::Raw(path.display().to_string())],
                        });
                        dispatch(
                            &mut guard,
                            &queue,
                            Job::Read {
                                path,
                                announce: false,
                            },
                        );
                    }
                }
                Dialog::About => {}
            }
            render(&ui, &guard);
        });
    }

    {
        let state = Arc::clone(&state);
        let window = ui.as_weak();
        ui.on_cancel_dialog(move || {
            let ui = window.unwrap();
            let mut guard = lock(&state);
            let trigger = guard.dialog.as_ref().map(Dialog::kind);
            match guard.dialog.take() {
                Some(Dialog::Apply { .. }) | Some(Dialog::Restore { .. }) => {
                    guard.status = Some(Note {
                        kind: 1,
                        title: Msg::Key("status.cancelled"),
                        lines: Vec::new(),
                    });
                    guard.alert = None;
                }
                Some(Dialog::Install { .. }) => {
                    guard.status = Some(Note {
                        kind: 1,
                        title: Msg::Key("status.notfound"),
                        lines: vec![Msg::Key("status.notfoundHint")],
                    });
                }
                Some(Dialog::About) | None => {}
            }
            render(&ui, &guard);
            if let Some(kind) = trigger {
                ui.invoke_focus_dialog_trigger(kind);
            }
        });
    }

    {
        let state = Arc::clone(&state);
        let window = ui.as_weak();
        ui.on_select_language(move |index| {
            let ui = window.unwrap();
            let mut guard = lock(&state);
            if index >= 0 {
                guard.i18n.set_active_index(index as usize);
            }
            render(&ui, &guard);
        });
    }

    {
        let state = Arc::clone(&state);
        let window = ui.as_weak();
        ui.on_open_about(move || {
            let ui = window.unwrap();
            let mut guard = lock(&state);
            if guard.dialog.is_some() {
                return;
            }
            guard.dialog = Some(Dialog::About);
            render(&ui, &guard);
        });
    }

    // ---- first paint and startup detection -------------------------------
    {
        let mut guard = lock(&state);
        dispatch(&mut guard, &queue, Job::Detect { fixed_dir });
        render(&ui, &guard);
    }

    let window = ui.as_weak();
    let worker_state = Arc::clone(&state);
    let worker_queue = queue.clone();
    std::thread::Builder::new()
        .name("pz-backend".to_string())
        .spawn(move || worker(jobs, window, worker_state, worker_queue))
        .map_err(|err| err.to_string())?;

    ui.show().map_err(|err| err.to_string())?;
    let mut area = windows_sys::Win32::Foundation::RECT::default();
    let available = unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::SystemParametersInfoW(
            windows_sys::Win32::UI::WindowsAndMessaging::SPI_GETWORKAREA,
            0,
            (&mut area as *mut windows_sys::Win32::Foundation::RECT).cast(),
            0,
        )
    };
    if available == 0 {
        return Err(format!(
            "Cannot query desktop work area: {}",
            std::io::Error::last_os_error()
        ));
    }
    // Winit creates its native window on event-loop resume, after show() returns.
    // Fit after that point so the real monitor/override scale is available.
    let window = ui.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_millis(100), move || {
        let Some(ui) = window.upgrade() else { return };
        let scale = ui.window().scale_factor();
        let width = ((1120.0 * scale) as u32).min(
            (area.right - area.left)
                .saturating_sub((48.0 * scale) as i32)
                .max(1) as u32,
        );
        let height = ((760.0 * scale) as u32).min(
            (area.bottom - area.top)
                .saturating_sub((80.0 * scale) as i32)
                .max(1) as u32,
        );
        ui.window()
            .set_size(slint::PhysicalSize::new(width, height));
        ui.window().set_position(slint::PhysicalPosition::new(
            area.left + (24.0 * scale) as i32,
            area.top + (24.0 * scale) as i32,
        ));
    });
    ui.run().map_err(|err| err.to_string())?;

    // The worker is detached on purpose: it owns a sender for follow-up jobs,
    // so the channel never closes on its own and joining would hang. Returning
    // from `main` ends the process and with it the thread.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> State {
        State::new(I18n::new(None).expect("bundled locales must load"))
    }

    #[test]
    fn valid_backup_is_not_misreported_as_broken_for_a_read_only_target() {
        let nonce = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("pz-gui-backup-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join(CONFIG_NAME);
        std::fs::write(&path, br#"{"vmArgs":["-Xmx8g"]}"#).unwrap();
        std::fs::write(backend::backup_path(&path), br#"{"vmArgs":["-Xmx4g"]}"#).unwrap();
        let permissions = std::fs::metadata(&path).unwrap().permissions();
        let mut read_only = permissions.clone();
        read_only.set_readonly(true);
        std::fs::set_permissions(&path, read_only).unwrap();
        let usable = matches!(backup_state(&path), BackupState::Usable { limit_bytes, .. } if limit_bytes == 4 * BYTES_PER_GIB);
        std::fs::set_permissions(&path, permissions).unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        assert!(
            usable,
            "backup validity must not depend on target write permissions"
        );
    }

    /// A dead worker must never look like a finished operation.
    #[test]
    fn dispatch_reports_a_closed_worker_channel() {
        let (queue, jobs) = mpsc::channel::<Job>();
        drop(jobs);
        let mut state = state();
        state.busy = true;
        state.status = Some(Note {
            kind: 2,
            title: Msg::Key("status.applied"),
            lines: Vec::new(),
        });

        dispatch(
            &mut state,
            &queue,
            Job::Read {
                path: PathBuf::from("ProjectZomboid64.json"),
                announce: false,
            },
        );

        assert!(
            !state.busy,
            "a job that was never sent must not keep the window busy"
        );
        assert!(
            state.status.is_none(),
            "the earlier success must not survive"
        );
        let alert = state
            .alert
            .expect("a job that was never sent must be reported");
        assert_eq!(alert.kind, 4, "the failure must be shown as an error");
    }

    /// Detection that has not finished, detection that failed, and a machine
    /// that really is too small are three different facts.
    #[test]
    fn strategy_notes_keep_detection_states_apart() {
        let mut state = state();
        assert_eq!(
            strategy_view(&state).heavy_note.as_str(),
            state.i18n.t("memory.detecting")
        );

        state.detected = true;
        assert_eq!(
            strategy_view(&state).heavy_note.as_str(),
            state.i18n.t("memory.unavailable")
        );

        state.memory = Some(MemoryInfo {
            total_bytes: 4 * BYTES_PER_GIB,
            available_bytes: BYTES_PER_GIB,
        });
        assert_eq!(
            strategy_view(&state).heavy_note.as_str(),
            state.i18n.t("strategy.unavailable")
        );
    }
}
