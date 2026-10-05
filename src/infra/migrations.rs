//! Migration planning for `rbb migrate --check`: which migrations are pending, whether the
//! database still matches this binary, and a rehearsal that applies the pending ones inside a
//! transaction that is always rolled back.
//!
//! The rehearsal does the real work (rewrites tables, builds indexes) and takes the same locks
//! the real migration will, so it shows how long the upgrade takes and which existing tables it
//! blocks. Because of that, on a large board it belongs on a restored copy of the database, not
//! on the live one.

use sqlx::migrate::Migration;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::doctor::{self, AppliedMigration, Status};

/// Lock modes that block ordinary reads or writes of a table while the migration runs, weakest
/// first.
const BLOCKING_MODES: &[&str] = &[
    "ShareLock",
    "ShareRowExclusiveLock",
    "ExclusiveLock",
    "AccessExclusiveLock",
];

fn strength(mode: &str) -> usize {
    BLOCKING_MODES.iter().position(|m| *m == mode).unwrap_or(0)
}

/// An existing table a rehearsed migration locked.
#[derive(Debug, Clone, PartialEq)]
pub struct TableLock {
    pub table: String,
    pub mode: String,
    /// The planner's row estimate, to judge how long the table stays locked.
    pub rows: i64,
}

impl TableLock {
    /// What the lock means for the running board.
    pub fn blocks(&self) -> &'static str {
        if self.mode == "AccessExclusiveLock" {
            "reads and writes"
        } else {
            "writes"
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Applied cleanly in the rehearsal.
    Applied {
        elapsed: Duration,
        locks: Vec<TableLock>,
    },
    /// Runs outside a transaction, so it can't be rehearsed.
    NotRehearsed,
    /// Failed; the real migration would fail the same way.
    Failed(String),
    /// Not attempted because an earlier one failed.
    Skipped,
}

#[derive(Debug, Clone)]
pub struct Step {
    pub version: i64,
    pub description: String,
    pub outcome: Outcome,
}

#[derive(Debug, Default)]
pub struct Report {
    /// Mismatches between the binary and the database (from the same checks as `rbb doctor`).
    pub problems: Vec<String>,
    pub applied: usize,
    pub steps: Vec<Step>,
}

impl Report {
    /// Whether `rbb migrate` is expected to succeed.
    pub fn ok(&self) -> bool {
        self.problems.is_empty()
            && self
                .steps
                .iter()
                .all(|s| matches!(s.outcome, Outcome::Applied { .. } | Outcome::NotRehearsed))
    }
}

async fn applied_migrations(db: &sqlx::PgPool) -> anyhow::Result<Vec<AppliedMigration>> {
    let has_table: bool = sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
        .fetch_one(db)
        .await?;
    if !has_table {
        return Ok(vec![]);
    }
    Ok(sqlx::query_as::<_, (i64, Vec<u8>, bool)>(
        "SELECT version, checksum, success FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|(version, checksum, success)| AppliedMigration {
        version,
        checksum,
        success,
    })
    .collect())
}

/// Compare `migrations` with the database and, unless `rehearse` is false, apply the pending
/// ones in a transaction that is rolled back. `lock_timeout` bounds each wait for a lock, so a
/// busy table makes the rehearsal fail fast instead of queueing behind live traffic.
pub async fn check(
    db: &sqlx::PgPool,
    migrations: &[Migration],
    rehearse: bool,
    lock_timeout: Duration,
) -> anyhow::Result<Report> {
    let applied = applied_migrations(db).await?;
    let embedded: Vec<(i64, Vec<u8>)> = migrations
        .iter()
        .map(|m| (m.version, m.checksum.to_vec()))
        .collect();
    let problems: Vec<String> = doctor::compare_migrations(&embedded, &applied)
        .into_iter()
        .filter(|c| c.status == Status::Fail)
        .map(|c| match c.fix {
            Some(f) => format!("{} — {f}", c.detail),
            None => c.detail,
        })
        .collect();
    let pending: Vec<&Migration> = migrations
        .iter()
        .filter(|m| !applied.iter().any(|a| a.version == m.version))
        .collect();
    let mut report = Report {
        problems,
        applied: applied.len(),
        steps: vec![],
    };
    if !rehearse || !report.problems.is_empty() {
        report.steps = pending
            .iter()
            .map(|m| Step {
                version: m.version,
                description: m.description.to_string(),
                outcome: Outcome::Skipped,
            })
            .collect();
        return Ok(report);
    }

    let mut tx = db.begin().await?;
    sqlx::query(&format!(
        "SET LOCAL lock_timeout = {}",
        lock_timeout.as_millis().max(1)
    ))
    .execute(&mut *tx)
    .await?;
    // Tables that exist before the rehearsal: locks on tables a migration creates block no one.
    let existing: HashMap<i64, i64> = sqlx::query_as::<_, (i64, f32)>(
        "SELECT oid::int8, reltuples FROM pg_class WHERE relkind IN ('r', 'p')",
    )
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|(oid, rows)| (oid, rows.max(0.0) as i64))
    .collect();
    let mut seen: HashMap<i64, String> = HashMap::new();
    let mut failed = false;
    for m in pending {
        let outcome = if failed {
            Outcome::Skipped
        } else if m.no_tx {
            Outcome::NotRehearsed
        } else {
            let t0 = Instant::now();
            match sqlx::raw_sql(&m.sql).execute(&mut *tx).await {
                Ok(_) => {
                    let held: Vec<(i64, String, String)> = sqlx::query_as(
                        "SELECT DISTINCT c.oid::int8, c.relname::text, l.mode FROM pg_locks l
                         JOIN pg_class c ON c.oid = l.relation
                         WHERE l.pid = pg_backend_pid() AND l.granted AND c.relkind IN ('r', 'p')
                           AND l.mode = ANY($1)
                         ORDER BY 2, 3",
                    )
                    .bind(BLOCKING_MODES)
                    .fetch_all(&mut *tx)
                    .await?;
                    let elapsed = t0.elapsed();
                    // The strongest lock per existing table, reported once per rehearsal (locks
                    // are held until the transaction ends, so later migrations see them again).
                    let mut locks: Vec<(i64, TableLock)> = vec![];
                    for (oid, table, mode) in held {
                        let Some(rows) = existing.get(&oid) else {
                            continue;
                        };
                        match locks.iter_mut().find(|(o, _)| *o == oid) {
                            Some((_, l)) if strength(&mode) > strength(&l.mode) => l.mode = mode,
                            Some(_) => {}
                            None => locks.push((
                                oid,
                                TableLock {
                                    table,
                                    mode,
                                    rows: *rows,
                                },
                            )),
                        }
                    }
                    let locks = locks
                        .into_iter()
                        .filter(|(oid, l)| {
                            let new = seen
                                .get(oid)
                                .is_none_or(|m| strength(&l.mode) > strength(m));
                            if new {
                                seen.insert(*oid, l.mode.clone());
                            }
                            new
                        })
                        .map(|(_, l)| l)
                        .collect();
                    Outcome::Applied { elapsed, locks }
                }
                Err(e) => {
                    failed = true;
                    Outcome::Failed(e.to_string())
                }
            }
        };
        report.steps.push(Step {
            version: m.version,
            description: m.description.to_string(),
            outcome,
        });
    }
    // Never commit: the rehearsal must leave the database exactly as it was.
    tx.rollback().await?;
    Ok(report)
}

/// The report as printed by `rbb migrate --check`.
pub fn render(report: &Report, rehearsed: bool) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "applied migrations: {}", report.applied);
    for p in &report.problems {
        let _ = writeln!(out, "problem: {p}");
    }
    if report.steps.is_empty() {
        if report.problems.is_empty() {
            let _ = writeln!(out, "pending migrations: none; the database is up to date");
        }
        return out;
    }
    let _ = writeln!(out, "pending migrations: {}", report.steps.len());
    for s in &report.steps {
        let _ = write!(out, "  {:04} {}: ", s.version, s.description);
        match &s.outcome {
            Outcome::Applied { elapsed, locks } => {
                let _ = writeln!(out, "ok in {}", human(*elapsed));
                for l in locks {
                    let _ = writeln!(
                        out,
                        "         locks {} (~{} rows, {}) — blocks {}",
                        l.table,
                        l.rows,
                        l.mode,
                        l.blocks()
                    );
                }
            }
            Outcome::NotRehearsed => {
                let _ = writeln!(out, "not rehearsed (runs outside a transaction)");
            }
            Outcome::Failed(e) => {
                let _ = writeln!(out, "FAILED: {e}");
            }
            Outcome::Skipped if rehearsed && report.problems.is_empty() => {
                let _ = writeln!(out, "not tried (an earlier migration failed)");
            }
            Outcome::Skipped => {
                let _ = writeln!(out, "pending");
            }
        }
    }
    if rehearsed && report.problems.is_empty() {
        let total: Duration = report
            .steps
            .iter()
            .filter_map(|s| match s.outcome {
                Outcome::Applied { elapsed, .. } => Some(elapsed),
                _ => None,
            })
            .sum();
        let _ = writeln!(
            out,
            "rehearsal rolled back; nothing was changed (took {})",
            human(total)
        );
        if report.ok() {
            let _ = writeln!(
                out,
                "next: back up the database (pg_dump --format=custom), then run `rbb migrate`"
            );
        }
    }
    out
}

fn human(d: Duration) -> String {
    if d < Duration::from_secs(1) {
        format!("{} ms", d.as_millis())
    } else {
        format!("{:.1} s", d.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(outcome: Outcome) -> Step {
        Step {
            version: 28,
            description: "add things".into(),
            outcome,
        }
    }

    #[test]
    fn report_is_ok_only_without_failures_or_problems() {
        let applied = Outcome::Applied {
            elapsed: Duration::from_millis(3),
            locks: vec![],
        };
        let mut r = Report {
            steps: vec![step(applied.clone()), step(Outcome::NotRehearsed)],
            ..Default::default()
        };
        assert!(r.ok());
        r.steps.push(step(Outcome::Failed("boom".into())));
        assert!(!r.ok());
        let r = Report {
            problems: vec!["changed".into()],
            steps: vec![step(applied)],
            ..Default::default()
        };
        assert!(!r.ok());
    }

    #[test]
    fn render_lists_locks_and_next_step() {
        let r = Report {
            applied: 27,
            steps: vec![step(Outcome::Applied {
                elapsed: Duration::from_millis(1500),
                locks: vec![TableLock {
                    table: "posts".into(),
                    mode: "AccessExclusiveLock".into(),
                    rows: 3_000_000,
                }],
            })],
            ..Default::default()
        };
        let s = render(&r, true);
        assert!(s.contains("0028 add things: ok in 1.5 s"), "{s}");
        assert!(
            s.contains(
                "locks posts (~3000000 rows, AccessExclusiveLock) — blocks reads and writes"
            ),
            "{s}"
        );
        assert!(s.contains("nothing was changed"), "{s}");
        assert!(s.contains("run `rbb migrate`"), "{s}");
    }

    #[test]
    fn up_to_date_says_so() {
        let s = render(
            &Report {
                applied: 27,
                ..Default::default()
            },
            true,
        );
        assert!(s.contains("none; the database is up to date"), "{s}");
    }
}
