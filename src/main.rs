//! Hide unwanted replies on your own Threads posts.
//!
//! Polls your recent posts, classifies new top-level replies (classifier),
//! optionally asks a chat model about uncertain ones (reviewer), and hides the
//! bad ones through the Threads API. Decisions go to stdout as JSON lines and
//! to Prometheus metrics.

mod classifier;
mod http;
mod llm;
mod metrics;
mod monitor;
mod notify;
mod oauth;
mod reviewer;
mod server;
mod store;
mod threads;
mod util;

use serde_json::json;
use util::{fatal, log, parsed};

extern "C" fn on_term(_: libc::c_int) {
    // As PID 1 in a container the default SIGTERM action is ignored; exit right away.
    // Each database write is its own transaction, so nothing is half-written.
    unsafe { libc::_exit(0) }
}

fn main() {
    unsafe {
        libc::signal(libc::SIGTERM, on_term as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_term as *const () as libc::sighandler_t);
    }
    let oauth = oauth::OAuth::from_env();
    let monitor = monitor::Monitor::from_env(http::real_http()); // creates the tables first
    if oauth.is_none() && monitor.threads().token().is_empty() {
        fatal("set THREADS_ACCESS_TOKEN, or THREADS_APP_ID + THREADS_APP_SECRET + OAUTH_REDIRECT_URI");
    }
    log(json!({"level": "info", "msg": "http", "oauth": oauth.is_some()}));
    server::serve(parsed("METRICS_PORT", "9090"), oauth, store::db_spec());
    monitor.run();
}
