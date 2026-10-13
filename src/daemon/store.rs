use crate::types::MetricSample;
use rusqlite::{params, Connection};
use std::sync::Mutex;

const RETENTION_MS: i64 = 7 * 24 * 3_600_000;

/// Historical metrics in SQLite (`~/.localstack/metrics.db`).
pub struct MetricsStore {
    db: Mutex<Connection>,
}

impl MetricsStore {
    pub fn open(file: &std::path::Path) -> rusqlite::Result<Self> {
        let db = Connection::open(file)?;
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS metrics (
               service TEXT NOT NULL, ts INTEGER NOT NULL,
               cpu REAL NOT NULL, mem REAL NOT NULL, net_rx REAL, net_tx REAL);
             CREATE INDEX IF NOT EXISTS metrics_service_ts ON metrics(service, ts);",
        )?;
        Ok(MetricsStore { db: Mutex::new(db) })
    }

    pub fn record(&self, service: &str, s: &MetricSample) {
        let db = self.db.lock().unwrap();
        let _ = db
            .prepare_cached("INSERT INTO metrics (service, ts, cpu, mem, net_rx, net_tx) VALUES (?, ?, ?, ?, ?, ?)")
            .and_then(|mut st| st.execute(params![service, s.ts, s.cpu, s.mem, s.net_rx, s.net_tx]));
    }

    /// Samples since `since`, averaged into at most `points` buckets.
    pub fn query(&self, service: &str, since: i64, now: i64, points: i64) -> Vec<MetricSample> {
        let points = points.max(1);
        let bucket = (((now - since) as f64 / points as f64).ceil() as i64).max(1000);
        let db = self.db.lock().unwrap();
        let Ok(mut st) = db.prepare_cached(
            "SELECT (ts / ?1) * ?1 AS b, AVG(cpu), AVG(mem), AVG(net_rx), AVG(net_tx)
             FROM metrics WHERE service = ?2 AND ts >= ?3 GROUP BY b ORDER BY b",
        ) else {
            return vec![];
        };
        st.query_map(params![bucket, service, since], |r| {
            Ok(MetricSample { ts: r.get(0)?, cpu: r.get(1)?, mem: r.get(2)?, net_rx: r.get(3)?, net_tx: r.get(4)? })
        })
        .map(|rows| rows.filter_map(Result::ok).collect())
        .unwrap_or_default()
    }

    pub fn prune(&self, now: i64) {
        let _ = self.db.lock().unwrap().execute("DELETE FROM metrics WHERE ts < ?", [now - RETENTION_MS]);
    }

    pub fn forget(&self, service: &str) {
        let _ = self.db.lock().unwrap().execute("DELETE FROM metrics WHERE service = ?", [service]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn buckets() {
        let dir = std::env::temp_dir().join(format!("lstack-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let s = MetricsStore::open(&dir.join("m.db")).unwrap();
        let now = 1_000_000_000;
        for i in 0..60 {
            s.record("a", &MetricSample { ts: now - 60_000 + i * 1000, cpu: i as f64, mem: 100.0, net_rx: None, net_tx: None });
        }
        s.record("b", &MetricSample { ts: now, cpu: 1.0, mem: 1.0, ..Default::default() });
        let q = s.query("a", now - 60_000, now, 6);
        assert_eq!(q.len(), 6);
        assert!((q[0].cpu - 4.5).abs() < 1e-9);
        s.forget("a");
        assert!(s.query("a", 0, now, 10).is_empty());
        assert_eq!(s.query("b", 0, now, 10).len(), 1);
        std::fs::remove_dir_all(dir).ok();
    }
}
