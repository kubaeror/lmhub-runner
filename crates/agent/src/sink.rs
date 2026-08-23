//! Event sink: appends every [`RunEvent`] to `events.jsonl`, human-readable
//! errors to `errors.log`, and forwards events live to the TUI.
//!
//! Secrets are scrubbed before anything touches disk.

use lmhub_core::{now_ts, redact, CoreError, RunEvent};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;

pub struct EventSink {
    events_file: Mutex<std::io::BufWriter<std::fs::File>>,
    errors_file: Mutex<std::io::BufWriter<std::fs::File>>,
    ui_tx: Option<UnboundedSender<RunEvent>>,
    error_count: AtomicU32,
    warning_count: AtomicU32,
    /// Set when a persistence write fails; `finalize` surfaces it instead of
    /// silently dropping events (disk full, broken pipe, …).
    write_failed: AtomicU32,
}

impl EventSink {
    /// Create `events.jsonl` / `errors.log` inside the run directory
    /// (truncated: each run owns its directory state).
    pub fn create(
        run_dir: &std::path::Path,
        ui_tx: Option<UnboundedSender<RunEvent>>,
    ) -> std::io::Result<Self> {
        let events_path = run_dir.join("events.jsonl");
        let errors_path = run_dir.join("errors.log");
        Ok(Self {
            events_file: Mutex::new(std::io::BufWriter::new(
                std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(events_path)?,
            )),
            errors_file: Mutex::new(std::io::BufWriter::new(
                std::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(errors_path)?,
            )),
            ui_tx,
            error_count: AtomicU32::new(0),
            warning_count: AtomicU32::new(0),
            write_failed: AtomicU32::new(0),
        })
    }

    /// Forward an event to the TUI only — never persisted. Used for high
    /// frequency streaming deltas to keep events.jsonl schema stable.
    pub fn emit_ui_only(&self, event: &RunEvent) {
        if let Some(tx) = &self.ui_tx {
            let _ = tx.send(event.clone());
        }
    }

    /// Record + forward a generic event (no error bookkeeping).
    pub fn emit(&self, event: &RunEvent) {
        self.write_line(&self.events_file, event);
        if let Some(tx) = &self.ui_tx {
            let _ = tx.send(event.clone());
        }
    }

    /// Serialize + append one line; failures are flagged for `finalize`
    /// instead of being swallowed silently.
    fn write_line(&self, file: &Mutex<std::io::BufWriter<std::fs::File>>, event: &RunEvent) {
        let Ok(mut f) = file.lock() else {
            self.note_write_failure("sink mutex poisoned");
            return;
        };
        let Ok(line) = serde_json::to_string(event) else {
            return; // RunEvent is always serializable
        };
        if let Err(e) = writeln!(f, "{line}") {
            self.note_write_failure(&e.to_string());
        }
    }

    fn note_write_failure(&self, detail: &str) {
        self.write_failed.fetch_add(1, Ordering::Relaxed);
        tracing::error!(detail, "run event persistence failed; events may be lost");
    }

    fn log_error_line(&self, ts: &str, kind: &str, message: &str) {
        if let Ok(mut f) = self.errors_file.lock() {
            let one_line = message.replace(['\n', '\r'], " ");
            if let Err(e) = writeln!(f, "{ts}\t{kind}\t{one_line}") {
                self.note_write_failure(&e.to_string());
            }
        } else {
            self.note_write_failure("errors.log mutex poisoned");
        }
        tracing::warn!(kind, message = %redact::scrub(message), "run error");
    }

    /// Provider/tool/sandbox/... failure: goes to errors.log AND events.jsonl.
    pub fn error(&self, kind: &str, message: &str) {
        let ts = now_ts();
        let scrubbed = redact::scrub(message);
        self.error_count.fetch_add(1, Ordering::Relaxed);
        self.log_error_line(&ts, kind, &scrubbed);
        self.emit(&RunEvent::Error {
            ts,
            kind: kind.to_string(),
            message: scrubbed,
        });
    }

    /// Convenience wrapper mapping a CoreError to its stable kind.
    pub fn core_error(&self, err: &CoreError, context: &str) {
        let msg = format!("{context}: {err}");
        self.error(err.kind(), &msg);
    }

    pub fn warning(&self, message: &str) {
        let ts = now_ts();
        let scrubbed = redact::scrub(message);
        self.warning_count.fetch_add(1, Ordering::Relaxed);
        tracing::warn!("{}", scrubbed);
        self.emit(&RunEvent::Warning {
            ts,
            message: scrubbed,
        });
    }

    /// Sandbox violation: dedicated event type + errors.log entry.
    pub fn violation(&self, detail: &str) {
        let ts = now_ts();
        let scrubbed = redact::scrub(detail);
        self.error_count.fetch_add(1, Ordering::Relaxed);
        self.log_error_line(&ts, "sandbox_violation", &scrubbed);
        self.emit(&RunEvent::SandboxViolation {
            ts,
            detail: scrubbed,
        });
    }

    pub fn error_count(&self) -> u32 {
        self.error_count.load(Ordering::Relaxed)
    }

    pub fn warning_count(&self) -> u32 {
        self.warning_count.load(Ordering::Relaxed)
    }

    /// Flush and close both files; must be called before statistics.json is
    /// written so counts and logs are consistent on disk. Fails loudly when
    /// any event could not be persisted.
    pub fn finalize(self) -> std::io::Result<()> {
        let mut failed = self.write_failed.load(Ordering::Relaxed) > 0;
        for f in [&self.events_file, &self.errors_file] {
            let Ok(mut f) = f.lock() else {
                failed = true;
                continue;
            };
            if let Err(e) = f.flush() {
                failed = true;
                tracing::error!(error = %e, "run event flush failed");
            }
        }
        if failed {
            return Err(std::io::Error::other(
                "some run events could not be persisted (disk full?)",
            ));
        }
        // Files close on drop of the BufWriters inside the mutexes.
        Ok(())
    }
}

/// Relative path recorded in statistics.json.
pub fn errors_log_rel_path() -> &'static str {
    "errors.log"
}

/// Where the run directory lives — helper for building absolute paths.
pub fn errors_log_abs_path(run_dir: &std::path::Path) -> PathBuf {
    run_dir.join("errors.log")
}
