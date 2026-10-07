//! Application use cases.
//!
//! Layering: HTTP handlers (`routes`, `admin`) extract input, call a use case, and render the
//! result. Use cases own the transaction: every write a use case makes, including its audit
//! records and the outbox jobs for its side effects, goes through one [`Uow`] and commits or
//! rolls back together. Nothing outside the database (mail, plugin hooks, live updates, cache
//! invalidation) happens before the commit. Authorization decisions come from `domain`.

pub mod accounts;

use crate::app::{App, LiveEvent};
use crate::audit::Actor;
use crate::error::AppResult;
use crate::infra::cluster::{self, Broadcast};
use crate::infra::outbox::{self, Job};
use sqlx::{PgConnection, Postgres, Transaction};

/// A unit of work: one transaction plus the side effects that run only once it commits.
pub struct Uow {
    tx: Transaction<'static, Postgres>,
    jobs: Vec<(Job, Option<String>)>,
    broadcasts: Vec<Broadcast>,
    live: Vec<LiveEvent>,
    mailed: bool,
}

impl Uow {
    pub async fn begin(app: &App) -> AppResult<Uow> {
        Ok(Uow {
            tx: app.db.begin().await?,
            jobs: vec![],
            broadcasts: vec![],
            live: vec![],
            mailed: false,
        })
    }

    /// The transaction, for queries.
    pub fn conn(&mut self) -> &mut PgConnection {
        &mut self.tx
    }

    /// Record an account audit event as part of this unit of work.
    pub async fn audit(
        &mut self,
        actor: &Actor,
        uid: i32,
        action: &str,
        details: serde_json::Value,
    ) -> AppResult<()> {
        crate::audit::record(&mut self.tx, actor, uid, action, details).await?;
        Ok(())
    }

    /// Queue an email; it is handed to the mail worker only if this unit of work commits.
    pub async fn mail(&mut self, to: &str, subject: &str, body: &str) -> AppResult<()> {
        crate::mail::queue_in(&mut self.tx, to, subject, body).await?;
        self.mailed = true;
        Ok(())
    }

    /// Run a background job after commit.
    pub fn job(&mut self, job: Job) {
        self.jobs.push((job, None));
    }

    /// Run a background job after commit, at most once per `key`.
    pub fn job_once(&mut self, job: Job, key: String) {
        self.jobs.push((job, Some(key)));
    }

    /// Plugin action hook after commit.
    pub fn hook(&mut self, name: &str, data: serde_json::Value) {
        self.job(Job::Hook {
            name: name.into(),
            data,
        });
    }

    /// Reload these shared cache parts on every node after commit.
    pub fn invalidate(&mut self, parts: &[&str]) {
        self.broadcasts.push(Broadcast::Cache(
            parts.iter().map(|p| p.to_string()).collect(),
        ));
    }

    /// Guest pages with these tags changed.
    pub fn page_tags(&mut self, tags: Vec<String>) {
        if !tags.is_empty() {
            self.broadcasts.push(Broadcast::PageTags(tags));
        }
    }

    /// Push a live update to connected browsers after commit.
    pub fn live(&mut self, ev: LiveEvent) {
        self.live.push(ev);
    }

    /// Commit, then apply this node's caches and live updates and wake the workers.
    pub async fn commit(mut self, app: &App) -> AppResult<()> {
        for (job, key) in &self.jobs {
            outbox::enqueue(&mut self.tx, job, key.as_deref()).await?;
        }
        let mut ids = Vec::with_capacity(self.broadcasts.len());
        for b in &self.broadcasts {
            ids.push(cluster::record(&mut self.tx, &app.node_id, b).await?);
        }
        for ev in &self.live {
            // NOTIFY is transactional: other nodes hear it only if this commits.
            if let Some(payload) = app.live_payload(ev) {
                sqlx::query("SELECT pg_notify('rbb_live', $1)")
                    .bind(payload)
                    .execute(&mut *self.tx)
                    .await?;
            }
        }
        self.tx.commit().await?;
        for (id, b) in ids.into_iter().zip(&self.broadcasts) {
            match app.apply_broadcast(b).await {
                Ok(()) => {
                    app.cluster_cursor.lock().unwrap().mark(id);
                }
                Err(e) => {
                    // The listener retries it like an event from another node.
                    tracing::warn!(error = %e, event = id, "local cache invalidation failed");
                    app.cluster_cursor.lock().unwrap().defer(id);
                }
            }
        }
        for ev in self.live {
            app.publish(ev);
        }
        if !self.jobs.is_empty() {
            app.outbox_wake.notify_one();
        }
        if self.mailed {
            app.mail_wake.notify_one();
        }
        Ok(())
    }
}
