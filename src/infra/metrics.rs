//! Process metrics in the Prometheus text format, served at `/metrics`.
//!
//! Label values must come from small fixed sets (route templates, status classes, job kinds),
//! never from user input or ids, so the number of series stays bounded.

use dashmap::DashMap;
use std::fmt::Write;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Histogram bucket upper bounds for durations, in seconds.
pub const SECONDS: &[f64] = &[
    0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];
/// Histogram bucket upper bounds for small counts (statements per request…).
pub const COUNTS: &[f64] = &[1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 500.0];

struct Histogram {
    bounds: &'static [f64],
    counts: Vec<AtomicU64>,
    count: AtomicU64,
    /// Sum, in millionths.
    sum_micro: AtomicU64,
}

impl Histogram {
    fn new(bounds: &'static [f64]) -> Self {
        Histogram {
            bounds,
            counts: bounds.iter().map(|_| AtomicU64::new(0)).collect(),
            count: AtomicU64::new(0),
            sum_micro: AtomicU64::new(0),
        }
    }
}

#[derive(Default)]
struct Registry {
    counters: DashMap<(&'static str, String), AtomicU64>,
    /// Gauges hold f64 bits.
    gauges: DashMap<(&'static str, String), AtomicU64>,
    histograms: DashMap<(&'static str, String), Histogram>,
    help: DashMap<&'static str, &'static str>,
}

static REG: LazyLock<Registry> = LazyLock::new(Registry::default);

fn labels(l: &[(&str, &str)]) -> String {
    if l.is_empty() {
        return String::new();
    }
    let inner: Vec<String> = l
        .iter()
        .map(|(k, v)| {
            format!(
                "{k}=\"{}\"",
                v.replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('\n', " ")
            )
        })
        .collect();
    format!("{{{}}}", inner.join(","))
}

/// Describe a metric (shown as `# HELP`).
pub fn describe(name: &'static str, help: &'static str) {
    REG.help.insert(name, help);
}

pub fn counter(name: &'static str, v: u64) {
    counter_with(name, &[], v)
}

pub fn counter_with(name: &'static str, l: &[(&str, &str)], v: u64) {
    REG.counters
        .entry((name, labels(l)))
        .or_insert_with(|| AtomicU64::new(0))
        .fetch_add(v, Ordering::Relaxed);
}

pub fn gauge(name: &'static str, v: f64) {
    gauge_with(name, &[], v)
}

pub fn gauge_with(name: &'static str, l: &[(&str, &str)], v: f64) {
    REG.gauges
        .entry((name, labels(l)))
        .or_insert_with(|| AtomicU64::new(0))
        .store(v.to_bits(), Ordering::Relaxed);
}

/// Add to a gauge (may be negative), e.g. for open connections.
pub fn gauge_add(name: &'static str, l: &[(&str, &str)], d: f64) {
    let e = REG
        .gauges
        .entry((name, labels(l)))
        .or_insert_with(|| AtomicU64::new(0f64.to_bits()));
    let _ = e.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |b| {
        Some((f64::from_bits(b) + d).to_bits())
    });
}

/// Record a duration in seconds.
pub fn observe(name: &'static str, l: &[(&str, &str)], seconds: f64) {
    observe_in(name, l, seconds, SECONDS)
}

/// Record a value into a histogram with the given bucket bounds (fixed per metric).
pub fn observe_in(name: &'static str, l: &[(&str, &str)], value: f64, bounds: &'static [f64]) {
    let h = REG
        .histograms
        .entry((name, labels(l)))
        .or_insert_with(|| Histogram::new(bounds));
    for (i, b) in h.bounds.iter().enumerate() {
        if value <= *b {
            h.counts[i].fetch_add(1, Ordering::Relaxed);
        }
    }
    h.count.fetch_add(1, Ordering::Relaxed);
    h.sum_micro
        .fetch_add((value * 1e6).max(0.0) as u64, Ordering::Relaxed);
}

/// Everything in the Prometheus text exposition format.
pub fn render() -> String {
    let mut out = String::new();
    let header = |out: &mut String, name: &str, kind: &str, done: &mut Vec<String>| {
        if !done.iter().any(|d| d == name) {
            if let Some(h) = REG.help.get(name) {
                let _ = writeln!(out, "# HELP {name} {}", *h);
            }
            let _ = writeln!(out, "# TYPE {name} {kind}");
            done.push(name.to_string());
        }
    };
    let mut done = Vec::new();
    let mut counters: Vec<_> = REG
        .counters
        .iter()
        .map(|e| {
            (
                e.key().0,
                e.key().1.clone(),
                e.value().load(Ordering::Relaxed),
            )
        })
        .collect();
    counters.sort();
    for (n, l, v) in counters {
        header(&mut out, n, "counter", &mut done);
        let _ = writeln!(out, "{n}{l} {v}");
    }
    let mut gauges: Vec<_> = REG
        .gauges
        .iter()
        .map(|e| {
            (
                e.key().0,
                e.key().1.clone(),
                f64::from_bits(e.value().load(Ordering::Relaxed)),
            )
        })
        .collect();
    gauges.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    for (n, l, v) in gauges {
        header(&mut out, n, "gauge", &mut done);
        let _ = writeln!(out, "{n}{l} {v}");
    }
    let mut hs: Vec<_> = REG
        .histograms
        .iter()
        .map(|e| (e.key().0, e.key().1.clone()))
        .collect();
    hs.sort();
    for (n, l) in hs {
        let Some(h) = REG.histograms.get(&(n, l.clone())) else {
            continue;
        };
        header(&mut out, n, "histogram", &mut done);
        let inner = l.trim_start_matches('{').trim_end_matches('}');
        let sep = if inner.is_empty() { "" } else { "," };
        for (i, b) in h.bounds.iter().enumerate() {
            let _ = writeln!(
                out,
                "{n}_bucket{{{inner}{sep}le=\"{b}\"}} {}",
                h.counts[i].load(Ordering::Relaxed)
            );
        }
        let count = h.count.load(Ordering::Relaxed);
        let _ = writeln!(out, "{n}_bucket{{{inner}{sep}le=\"+Inf\"}} {count}");
        let _ = writeln!(
            out,
            "{n}_sum{l} {}",
            h.sum_micro.load(Ordering::Relaxed) as f64 / 1e6
        );
        let _ = writeln!(out, "{n}_count{l} {count}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_prometheus_text() {
        counter_with("rbb_test_requests_total", &[("status", "2xx")], 3);
        observe("rbb_test_latency_seconds", &[("route", "/x")], 0.02);
        gauge("rbb_test_gauge", 1.5);
        let s = render();
        assert!(s.contains("rbb_test_requests_total{status=\"2xx\"} 3"));
        assert!(s.contains("rbb_test_latency_seconds_bucket{route=\"/x\",le=\"0.025\"} 1"));
        assert!(s.contains("rbb_test_latency_seconds_bucket{route=\"/x\",le=\"0.01\"} 0"));
        assert!(s.contains("rbb_test_latency_seconds_count{route=\"/x\"} 1"));
        assert!(s.contains("rbb_test_gauge 1.5"));
    }
}
