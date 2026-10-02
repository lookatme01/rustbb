//! Caps on open live-update (SSE) streams: in total, per client address and per member.
//!
//! A stream holds a connection open for up to an hour, so the right limit is how many are open
//! at once, not how often they are requested. A [`StreamGuard`] takes a slot when a stream opens
//! and gives it back when the stream ends, however it ends.

use dashmap::DashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct StreamLimits {
    pub max_total: usize,
    pub max_per_ip: usize,
    pub max_per_user: usize,
    total: AtomicUsize,
    per_ip: DashMap<String, usize>,
    per_user: DashMap<i32, usize>,
}

/// Why a stream was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    Total,
    Address,
    Member,
}

impl StreamLimits {
    pub fn new(max_total: usize, max_per_ip: usize, max_per_user: usize) -> Self {
        StreamLimits {
            max_total,
            max_per_ip,
            max_per_user,
            total: AtomicUsize::new(0),
            per_ip: DashMap::new(),
            per_user: DashMap::new(),
        }
    }

    pub fn open(&self) -> usize {
        self.total.load(Ordering::Relaxed)
    }

    /// Take a slot for a stream from `ip` (and member `uid`, 0 for guests).
    pub fn acquire(self: &Arc<Self>, ip: &str, uid: i32) -> Result<StreamGuard, Refused> {
        if self.total.fetch_add(1, Ordering::AcqRel) >= self.max_total {
            self.total.fetch_sub(1, Ordering::AcqRel);
            return Err(Refused::Total);
        }
        {
            let mut n = self.per_ip.entry(ip.to_string()).or_insert(0);
            if *n >= self.max_per_ip {
                drop(n);
                self.total.fetch_sub(1, Ordering::AcqRel);
                return Err(Refused::Address);
            }
            *n += 1;
        }
        if uid > 0 {
            let mut n = self.per_user.entry(uid).or_insert(0);
            if *n >= self.max_per_user {
                drop(n);
                self.release_ip(ip);
                self.total.fetch_sub(1, Ordering::AcqRel);
                return Err(Refused::Member);
            }
            *n += 1;
        }
        Ok(StreamGuard {
            limits: self.clone(),
            ip: ip.to_string(),
            uid,
        })
    }

    fn release_ip(&self, ip: &str) {
        self.per_ip.remove_if_mut(ip, |_, n| {
            *n = n.saturating_sub(1);
            *n == 0
        });
    }
}

/// An open stream's slot; released on drop.
pub struct StreamGuard {
    limits: Arc<StreamLimits>,
    ip: String,
    uid: i32,
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.limits.release_ip(&self.ip);
        if self.uid > 0 {
            self.limits.per_user.remove_if_mut(&self.uid, |_, n| {
                *n = n.saturating_sub(1);
                *n == 0
            });
        }
        self.limits.total.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_and_releases() {
        let l = Arc::new(StreamLimits::new(3, 2, 1));
        let a = l.acquire("1.1.1.1", 0).unwrap();
        let _b = l.acquire("1.1.1.1", 7).unwrap();
        assert_eq!(l.acquire("1.1.1.1", 0).err(), Some(Refused::Address));
        assert_eq!(l.acquire("2.2.2.2", 7).err(), Some(Refused::Member));
        let _c = l.acquire("2.2.2.2", 0).unwrap();
        assert_eq!(l.acquire("3.3.3.3", 0).err(), Some(Refused::Total));
        drop(a);
        assert_eq!(l.open(), 2);
        assert!(l.acquire("3.3.3.3", 0).is_ok());
        assert_eq!(l.open(), 2, "the temporary guard was released");
    }
}
