// SPDX-License-Identifier: AGPL-3.0-only
//! Append-only audit logging of mutating, administrative, and denied operations
//! (ADR-0011, ADR-0014).
//!
//! Every state-changing operation (collection create/delete, point
//! upsert/delete) and every access-control denial is recorded with the acting
//! principal, the action, the target resource, and the outcome. Successful
//! reads are deliberately not recorded — they do not change state and would
//! drown the signal; a *denied* read still is, because a denial is a security
//! event regardless of the action.
//!
//! Each record is emitted as a structured `tracing` event (target
//! `quiver::audit`) and, when [`Config::audit_log`] names a file, appended to it
//! as one JSON object per line (JSON Lines) for ingestion by an external log
//! pipeline. Writes are serialized and flushed per line so records never
//! interleave; an audit write failure is logged loudly but does not fail the
//! caller's operation (availability over fail-closed — see `docs/security/audit.md`).
//!
//! Secrets are never written: a key's actor identity is its configured `id`, or
//! else a short SHA-256 fingerprint of its secret (`key:<hex>`), which is
//! preimage-resistant — the log can attribute an action to a key without
//! revealing the key.
//!
//! [`Config::audit_log`]: crate::Config::audit_log

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::Error;

/// The outcome of an audited operation.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Outcome {
    /// Authorized and completed.
    Ok,
    /// Refused by RBAC — too low a role, or a collection outside the key's scope.
    Denied,
    /// Authorized, but the engine returned an error.
    Error,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Denied => "denied",
            Outcome::Error => "error",
        }
    }

    /// Classify an authorized operation by its result. (Denials are recorded at
    /// the authorization guard, before the engine runs, so they never reach
    /// here.)
    pub(crate) fn of<T>(result: &Result<T, Error>) -> Self {
        match result {
            Ok(_) => Outcome::Ok,
            Err(_) => Outcome::Error,
        }
    }
}

// One audit record, serialized as a single JSON line. `ts_ms` is Unix epoch
// milliseconds in UTC (a monotonic, timezone-free integer — no date dependency).
#[derive(Serialize)]
struct Record<'a> {
    ts_ms: u64,
    actor: &'a str,
    action: &'a str,
    resource: &'a str,
    outcome: &'a str,
}

/// An append-only audit sink: a `tracing` event always, plus a JSON-Lines file
/// when one is configured.
pub(crate) struct AuditLog {
    sink: Option<Mutex<File>>,
}

impl AuditLog {
    /// Open the audit log. With `path = None` only `tracing` events are emitted;
    /// with a path, records are also appended to that file (created if absent,
    /// never truncated).
    pub(crate) fn open(path: Option<&Path>) -> Result<Self, Error> {
        let sink = match path {
            Some(path) => {
                let file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .map_err(Error::Io)?;
                Some(Mutex::new(file))
            }
            None => None,
        };
        Ok(Self { sink })
    }

    /// Record one audited operation: a `tracing` event plus, if a file is
    /// configured, one appended JSON line.
    pub(crate) fn record(&self, actor: &str, action: &str, resource: &str, outcome: Outcome) {
        let outcome = outcome.as_str();
        // A structured event always — secrets are never among the fields.
        tracing::info!(target: "quiver::audit", actor, action, resource, outcome, "audit");

        let Some(sink) = &self.sink else { return };
        let ts_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let record = Record {
            ts_ms,
            actor,
            action,
            resource,
            outcome,
        };
        // Serialization cannot realistically fail (all fields are plain), but
        // never panic on the request path.
        let Ok(mut line) = serde_json::to_string(&record) else {
            tracing::error!(target: "quiver::audit", "failed to serialize an audit record");
            return;
        };
        line.push('\n');
        match sink.lock() {
            Ok(mut file) => {
                if let Err(e) = file.write_all(line.as_bytes()).and_then(|()| file.flush()) {
                    tracing::error!(target: "quiver::audit", error = %e, "failed to append an audit record");
                }
            }
            Err(_) => tracing::error!(target: "quiver::audit", "audit sink lock poisoned"),
        }
    }

    /// Record a denied operation — the convenience used at the authorization
    /// guard, where the engine never runs.
    pub(crate) fn deny(&self, actor: &str, action: &str, resource: &str) {
        self.record(actor, action, resource, Outcome::Denied);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn read_lines(path: &Path) -> Vec<serde_json::Value> {
        let mut contents = String::new();
        File::open(path)
            .expect("open audit log")
            .read_to_string(&mut contents)
            .expect("read audit log");
        contents
            .lines()
            .map(|l| serde_json::from_str(l).expect("each line is valid JSON"))
            .collect()
    }

    #[test]
    fn open_none_records_without_a_file() {
        // The tracing-only sink must accept records without panicking.
        let log = AuditLog::open(None).expect("open tracing-only audit log");
        log.record("actor", "upsert", "c", Outcome::Ok);
        log.deny("actor", "upsert", "c");
    }

    #[test]
    fn records_are_appended_as_json_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.log");

        let log = AuditLog::open(Some(&path)).expect("open file audit log");
        log.record("ci-admin", "create_collection", "acme.docs", Outcome::Ok);
        log.deny("key:abcd", "upsert", "acme.docs");
        log.record("ci-admin", "upsert", "acme.docs", Outcome::Error);

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["actor"], "ci-admin");
        assert_eq!(lines[0]["action"], "create_collection");
        assert_eq!(lines[0]["resource"], "acme.docs");
        assert_eq!(lines[0]["outcome"], "ok");
        assert!(lines[0]["ts_ms"].as_u64().is_some());
        assert_eq!(lines[1]["outcome"], "denied");
        assert_eq!(lines[2]["outcome"], "error");
    }

    #[test]
    fn open_appends_rather_than_truncates() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit.log");

        AuditLog::open(Some(&path))
            .expect("open")
            .record("a", "upsert", "c", Outcome::Ok);
        // Re-opening the same path must preserve the earlier record.
        AuditLog::open(Some(&path))
            .expect("reopen")
            .record("a", "delete_points", "c", Outcome::Ok);

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["action"], "upsert");
        assert_eq!(lines[1]["action"], "delete_points");
    }
}
