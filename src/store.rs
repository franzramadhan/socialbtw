//! Processed replies (so each is classified once) and the current token.
//! SQLite file by default; PostgreSQL when DATABASE_URL is set, so the app keeps
//! no local state.

use crate::util::{fatal, log, now, var};
use rusqlite::{params, Connection, OptionalExtension};
use rustls::pki_types::{pem::PemObject, CertificateDer};
use serde_json::json;
use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

/// One decided reply, as stored.
pub struct Decision<'a> {
    pub id: &'a str,
    pub post_id: &'a str,
    pub username: &'a str,
    pub text: &'a str,
    pub category: &'a str,
    pub bad_p: f64,
    pub action: &'a str,
    pub stage: &'a str,
    pub reason: &'a str,
}

pub enum Store {
    Sqlite(Connection),
    Postgres { url: String, tls: tokio_postgres_rustls::MakeRustlsConnect, client: RefCell<Option<postgres::Client>> },
}

const SQLITE_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS replies (
     id TEXT PRIMARY KEY, post_id TEXT, username TEXT, text TEXT,
     category TEXT, bad_probability REAL, action TEXT, stage TEXT,
     reason TEXT, decided_at INTEGER);
 CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT);";

const PG_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS replies (
     id TEXT PRIMARY KEY, post_id TEXT, username TEXT, text TEXT,
     category TEXT, bad_probability DOUBLE PRECISION, action TEXT, stage TEXT,
     reason TEXT, decided_at BIGINT);
 CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT);";

/// DATABASE_URL if set, else the SQLite file DB_PATH.
pub fn db_spec() -> String {
    let url = var("DATABASE_URL", Some(""));
    if url.is_empty() { var("DB_PATH", Some("socialbtw.db")) } else { url }
}

/// TLS for PostgreSQL: the CA in DATABASE_CA_FILE (PEM), else public roots.
fn pg_tls() -> tokio_postgres_rustls::MakeRustlsConnect {
    let mut roots = rustls::RootCertStore::empty();
    let ca = var("DATABASE_CA_FILE", Some(""));
    if ca.is_empty() {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    } else {
        for cert in CertificateDer::pem_file_iter(&ca).unwrap_or_else(|e| fatal(&format!("DATABASE_CA_FILE {ca}: {e}"))) {
            let cert = cert.unwrap_or_else(|e| fatal(&format!("DATABASE_CA_FILE {ca}: {e}")));
            roots.add(cert).unwrap_or_else(|e| fatal(&format!("DATABASE_CA_FILE {ca}: {e}")));
        }
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap_or_else(|e| fatal(&format!("tls: {e}")))
        .with_root_certificates(roots)
        .with_no_client_auth();
    tokio_postgres_rustls::MakeRustlsConnect::new(config)
}

/// postgres::Error's Display is often just "db error"; add the server message
/// or the underlying cause.
fn pg_err(e: &postgres::Error) -> String {
    match (e.as_db_error(), std::error::Error::source(e)) {
        (Some(db), _) => format!("{e}: {} ({})", db.message(), db.code().code()),
        (None, Some(src)) => format!("{e}: {src}"),
        (None, None) => e.to_string(),
    }
}

impl Store {
    pub fn open(spec: &str) -> Self {
        if spec.starts_with("postgres://") || spec.starts_with("postgresql://") {
            // Connects lazily (and again after a failover), so a database that is
            // still starting does not stop the app.
            return Store::Postgres { url: spec.into(), tls: pg_tls(), client: RefCell::new(None) };
        }
        let db = Connection::open(spec).unwrap_or_else(|e| fatal(&format!("open {spec}: {e}")));
        db.execute_batch(SQLITE_SCHEMA).unwrap_or_else(|e| fatal(&format!("init {spec}: {e}")));
        // The poller and the OAuth callback each hold a connection.
        let _ = db.busy_timeout(Duration::from_secs(5));
        Store::Sqlite(db)
    }

    /// Run `f` on the PostgreSQL client, connecting (and creating the tables) first
    /// when there is no live connection, and retrying once after a dropped one.
    fn pg<T>(&self, f: impl Fn(&mut postgres::Client) -> Result<T, postgres::Error>) -> Result<T, String> {
        let Store::Postgres { url, tls, client } = self else { unreachable!() };
        let mut slot = client.borrow_mut();
        for attempt in 0..2 {
            if slot.as_ref().map_or(true, |c| c.is_closed()) {
                // The error never contains the URL, so the password stays out of logs.
                let mut c = postgres::Client::connect(url, tls.clone()).map_err(|e| format!("postgres connect: {}", pg_err(&e)))?;
                c.batch_execute(PG_SCHEMA).map_err(|e| format!("postgres schema: {}", pg_err(&e)))?;
                *slot = Some(c);
            }
            let c = slot.as_mut().expect("connected above");
            match f(c) {
                Ok(v) => return Ok(v),
                Err(_) if attempt == 0 && c.is_closed() => continue,
                Err(e) => return Err(format!("postgres: {}", pg_err(&e))),
            }
        }
        Err("postgres: connection lost".into())
    }

    pub fn seen(&self, id: &str) -> Result<bool, String> {
        match self {
            Store::Sqlite(db) => db
                .query_row("SELECT 1 FROM replies WHERE id = ?", [id], |_| Ok(()))
                .optional()
                .map(|r| r.is_some())
                .map_err(|e| format!("sqlite: {e}")),
            Store::Postgres { .. } => self.pg(|c| c.query_opt("SELECT 1 FROM replies WHERE id = $1", &[&id]).map(|r| r.is_some())),
        }
    }

    pub fn get(&self, key: &str) -> Option<String> {
        let res = match self {
            Store::Sqlite(db) => db.query_row("SELECT value FROM kv WHERE key = ?", [key], |r| r.get(0)).optional().map_err(|e| format!("sqlite: {e}")),
            Store::Postgres { .. } => self.pg(|c| c.query_opt("SELECT value FROM kv WHERE key = $1", &[&key]).map(|r| r.map(|r| r.get(0)))),
        };
        res.unwrap_or_else(|e| {
            log(json!({"level": "error", "msg": "store read failed", "error": e}));
            None
        })
    }

    pub fn put(&self, key: &str, value: &str) {
        let res = match self {
            Store::Sqlite(db) => db.execute("INSERT OR REPLACE INTO kv VALUES (?, ?)", [key, value]).map(|_| ()).map_err(|e| format!("sqlite: {e}")),
            Store::Postgres { .. } => self.pg(|c| {
                c.execute("INSERT INTO kv VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value", &[&key, &value])
                    .map(|_| ())
            }),
        };
        if let Err(e) = res {
            log(json!({"level": "error", "msg": "store write failed", "error": e}));
        }
    }

    pub fn save(&self, d: &Decision) -> Result<(), String> {
        let at = now() as i64;
        match self {
            Store::Sqlite(db) => db
                .execute(
                    "INSERT OR REPLACE INTO replies VALUES (?,?,?,?,?,?,?,?,?,?)",
                    params![d.id, d.post_id, d.username, d.text, d.category, d.bad_p, d.action, d.stage, d.reason, at],
                )
                .map(|_| ())
                .map_err(|e| format!("sqlite: {e}")),
            Store::Postgres { .. } => self.pg(|c| {
                c.execute(
                    "INSERT INTO replies VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT (id) DO NOTHING",
                    &[&d.id, &d.post_id, &d.username, &d.text, &d.category, &d.bad_p, &d.action, &d.stage, &d.reason, &at],
                )
                .map(|_| ())
            }),
        }
    }

    #[cfg(test)]
    pub fn action(&self, id: &str) -> Option<String> {
        match self {
            Store::Sqlite(db) => db.query_row("SELECT action FROM replies WHERE id = ?", [id], |r| r.get(0)).optional().ok().flatten(),
            Store::Postgres { .. } => self.pg(|c| c.query_opt("SELECT action FROM replies WHERE id = $1", &[&id]).map(|r| r.map(|r| r.get(0)))).ok().flatten(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_store() {
        let store = Store::open(":memory:");
        store.put("k", "v1");
        store.put("k", "v2");
        assert_eq!(store.get("k").as_deref(), Some("v2"));
        let d = Decision { id: "r1", post_id: "p", username: "u", text: "t", category: "spam", bad_p: 0.9, action: "hidden", stage: "classifier", reason: "" };
        store.save(&d).unwrap();
        assert!(store.seen("r1").unwrap());
        assert!(!store.seen("nope").unwrap());
        assert_eq!(store.action("r1").as_deref(), Some("hidden"));
    }

    // Needs a PostgreSQL server: TEST_DATABASE_URL=postgres://... (and DATABASE_CA_FILE
    // for a private CA). Skipped otherwise.
    #[test]
    fn postgres_store() {
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else { return };
        let store = Store::open(&url);
        store.put("k", "v1");
        store.put("k", "v2");
        assert_eq!(store.get("k").as_deref(), Some("v2"));
        let d = Decision { id: "pg-r1", post_id: "p", username: "u", text: "t", category: "spam", bad_p: 0.9, action: "hidden", stage: "classifier", reason: "" };
        store.save(&d).unwrap();
        store.save(&d).unwrap(); // second save is a no-op
        assert!(store.seen("pg-r1").unwrap());
        assert!(!store.seen("pg-nope").unwrap());
        assert_eq!(store.action("pg-r1").as_deref(), Some("hidden"));
        // Drop the first connection server-side, as a failover would.
        Store::open(&url)
            .pg(|c| c.execute("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE pid <> pg_backend_pid() AND datname = current_database()", &[]))
            .unwrap();
        assert_eq!(store.get("k").as_deref(), Some("v2"));
    }
}
