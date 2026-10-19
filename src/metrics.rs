//! Process-wide Prometheus metrics, rendered by the HTTP server at /metrics.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

const BUCKETS: [f64; 12] = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.85, 0.9, 0.95, 1.0];

pub struct Metrics {
    /// (category, action, stage) -> count
    pub replies: BTreeMap<(String, String, String), u64>,
    bad_buckets: [u64; 12], // cumulative, Prometheus style
    bad_count: u64,
    bad_sum: f64,
    pub api_errors: BTreeMap<&'static str, u64>,
    pub post_checks: BTreeMap<&'static str, u64>,
    pub last_poll: u64,
    pub token_expiry: u64,
}

static METRICS: Mutex<Metrics> = Mutex::new(Metrics {
    replies: BTreeMap::new(),
    bad_buckets: [0; 12],
    bad_count: 0,
    bad_sum: 0.0,
    api_errors: BTreeMap::new(),
    post_checks: BTreeMap::new(),
    last_poll: 0,
    token_expiry: 0,
});

pub fn metrics() -> MutexGuard<'static, Metrics> {
    METRICS.lock().unwrap_or_else(|e| e.into_inner())
}

impl Metrics {
    pub fn observe_bad(&mut self, p: f64) {
        for (i, le) in BUCKETS.iter().enumerate() {
            if p <= *le {
                self.bad_buckets[i] += 1;
            }
        }
        self.bad_count += 1;
        self.bad_sum += p;
    }

    pub fn render(&self) -> String {
        let mut s = String::from("# TYPE threads_replies_total counter\n");
        for ((c, a, st), v) in &self.replies {
            s += &format!("threads_replies_total{{category=\"{c}\",action=\"{a}\",stage=\"{st}\"}} {v}\n");
        }
        s += "# TYPE threads_bad_probability histogram\n";
        for (le, n) in BUCKETS.iter().zip(self.bad_buckets) {
            s += &format!("threads_bad_probability_bucket{{le=\"{le}\"}} {n}\n");
        }
        s += &format!(
            "threads_bad_probability_bucket{{le=\"+Inf\"}} {c}\nthreads_bad_probability_sum {}\nthreads_bad_probability_count {c}\n",
            self.bad_sum,
            c = self.bad_count
        );
        s += "# TYPE threads_api_errors_total counter\n";
        for (e, v) in &self.api_errors {
            s += &format!("threads_api_errors_total{{endpoint=\"{e}\"}} {v}\n");
        }
        s += "# TYPE threads_post_checks_total counter\n";
        for (r, v) in &self.post_checks {
            s += &format!("threads_post_checks_total{{result=\"{r}\"}} {v}\n");
        }
        s += &format!(
            "# TYPE threads_last_poll_success_timestamp_seconds gauge\nthreads_last_poll_success_timestamp_seconds {}\n\
             # TYPE threads_token_expiry_timestamp_seconds gauge\nthreads_token_expiry_timestamp_seconds {}\n",
            self.last_poll, self.token_expiry
        );
        s
    }
}
