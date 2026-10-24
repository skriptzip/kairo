//! Database access for SQLite, PostgreSQL and MariaDB/MySQL through sqlx's `any` driver.
//!
//! SQL is written once, in a dialect all three understand, with two conventions that
//! [`translate`] turns into the real thing:
//! - parameters are plain `?` (bound in order, so a value used twice is bound twice);
//!   PostgreSQL gets `$1, $2, ...`
//! - identifiers that are reserved words in some dialect are written in backticks
//!   (MariaDB keeps them, SQLite and PostgreSQL get double quotes)
//!
//! Everything else that differs (upserts, DDL, `RETURNING`) is handled by the helpers here.

use std::{
    collections::HashMap,
    fmt::Write,
    sync::{Mutex, OnceLock},
};

use sqlx::{
    Any, AnyPool, Row,
    any::{AnyArguments, AnyPoolOptions, AnyRow},
    pool::PoolConnection,
    query::Query,
};

pub use sqlx::AnyConnection as Conn;
pub type Tx = sqlx::Transaction<'static, Any>;
pub type Result<T> = std::result::Result<T, sqlx::Error>;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dialect {
    Sqlite,
    Postgres,
    MySql,
}

static DIALECT: OnceLock<Dialect> = OnceLock::new();

/// The dialect of the connected database (SQLite until [`Db::connect`] says otherwise).
pub fn dialect() -> Dialect {
    DIALECT.get().copied().unwrap_or(Dialect::Sqlite)
}

fn translate(sql: &str) -> String {
    let dialect = dialect();
    let mut out = String::with_capacity(sql.len() + 8);
    let mut n = 0;
    for c in sql.chars() {
        match c {
            '?' if dialect == Dialect::Postgres => {
                n += 1;
                let _ = write!(out, "${n}");
            }
            '`' if dialect != Dialect::MySql => out.push('"'),
            c => out.push(c),
        }
    }
    out
}

/// Translated SQL with a `'static` lifetime. The set of statements is fixed at compile time
/// (plus a few `format!`s over constants), so the cache stays tiny.
fn sql(text: &str) -> &'static str {
    static CACHE: OnceLock<Mutex<HashMap<String, &'static str>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    if let Some(found) = cache.get(text) {
        return found;
    }
    let translated: &'static str = Box::leak(translate(text).into_boxed_str());
    cache.insert(text.to_string(), translated);
    translated
}

pub fn query<'q>(text: &str) -> Query<'q, Any, AnyArguments<'q>> {
    sqlx::query(sql(text))
}

// ---------------------------------------------------------------------------------------
// Reading rows. The `any` driver is strict about integer widths, so be forgiving.
// ---------------------------------------------------------------------------------------

/// MariaDB reports `LONGTEXT` as a blob, so fall back to decoding the bytes as UTF-8.
pub fn s(row: &AnyRow, i: usize) -> Result<String> {
    row.try_get::<String, _>(i).or_else(|err| {
        let bytes = row.try_get::<Vec<u8>, _>(i).map_err(|_| err)?;
        String::from_utf8(bytes).map_err(|e| sqlx::Error::Decode(Box::new(e)))
    })
}

pub fn opt_s(row: &AnyRow, i: usize) -> Result<Option<String>> {
    row.try_get::<Option<String>, _>(i).or_else(|err| {
        let bytes = row.try_get::<Option<Vec<u8>>, _>(i).map_err(|_| err)?;
        bytes.map(|b| String::from_utf8(b).map_err(|e| sqlx::Error::Decode(Box::new(e)))).transpose()
    })
}

pub fn n(row: &AnyRow, i: usize) -> Result<i64> {
    row.try_get::<i64, _>(i)
        .or_else(|_| row.try_get::<i32, _>(i).map(i64::from))
        .or_else(|_| row.try_get::<i16, _>(i).map(i64::from))
}

pub fn opt_n(row: &AnyRow, i: usize) -> Result<Option<i64>> {
    row.try_get::<Option<i64>, _>(i)
        .or_else(|_| row.try_get::<Option<i32>, _>(i).map(|v| v.map(i64::from)))
        .or_else(|_| row.try_get::<Option<i16>, _>(i).map(|v| v.map(i64::from)))
}

// ---------------------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------------------

#[derive(Clone)]
pub struct Db {
    pool: AnyPool,
}

/// Turns the configured URL into one sqlx understands and detects the dialect.
/// Accepts `sqlite://path`, `postgres://...`, `postgresql://...`, `mysql://...`, `mariadb://...`.
fn parse_url(url: &str) -> std::result::Result<(Dialect, String), String> {
    let (scheme, rest) = url.split_once(':').ok_or_else(|| format!("invalid database URL {url:?}"))?;
    match scheme {
        "sqlite" => Ok((Dialect::Sqlite, url.to_string())),
        "postgres" | "postgresql" => Ok((Dialect::Postgres, url.to_string())),
        "mysql" => Ok((Dialect::MySql, url.to_string())),
        "mariadb" => Ok((Dialect::MySql, format!("mysql:{rest}"))),
        other => Err(format!("unsupported database {other:?}; use sqlite, postgres, mysql or mariadb")),
    }
}

/// `sqlite://` URL for a plain file path, creating the file if needed.
pub fn sqlite_url(path: &str) -> String {
    if path == ":memory:" { "sqlite::memory:".into() } else { format!("sqlite://{path}?mode=rwc") }
}

impl Db {
    /// Connects and brings the schema up to date.
    pub async fn connect(url: &str) -> std::result::Result<Db, Box<dyn std::error::Error>> {
        let db = Db::open(url).await?;
        init(&mut *db.conn().await?).await?;
        Ok(db)
    }

    /// Connects without touching the schema.
    pub async fn open(url: &str) -> std::result::Result<Db, Box<dyn std::error::Error>> {
        let (dialect, url) = parse_url(url)?;
        DIALECT.get_or_init(|| dialect);
        sqlx::any::install_default_drivers();

        let mut options = AnyPoolOptions::new();
        options = if dialect == Dialect::Sqlite {
            // One connection: SQLite has a single writer anyway, and an in-memory database
            // only exists inside its connection.
            options.max_connections(1).idle_timeout(None).max_lifetime(None)
        } else {
            let size = std::env::var("KAIRO_DB_POOL").ok().and_then(|v| v.parse().ok()).unwrap_or(10);
            options.max_connections(size)
        };
        // A database container usually starts alongside this server: wait for it a while.
        let attempts = if dialect == Dialect::Sqlite { 1 } else { 30 };
        let mut tried = 1;
        let pool = loop {
            match options.clone().connect(&url).await {
                Ok(pool) => break pool,
                Err(err) if tried < attempts => {
                    eprintln!("database not reachable yet ({err}), retrying");
                    tried += 1;
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                Err(err) => return Err(err.into()),
            }
        };
        Ok(Db { pool })
    }

    pub async fn begin(&self) -> Result<Tx> {
        self.pool.begin().await
    }

    pub async fn conn(&self) -> Result<PoolConnection<Any>> {
        self.pool.acquire().await
    }

    pub fn name(&self) -> &'static str {
        match dialect() {
            Dialect::Sqlite => "sqlite",
            Dialect::Postgres => "postgresql",
            Dialect::MySql => "mariadb/mysql",
        }
    }
}

// ---------------------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------------------

/// Column type for long text (MariaDB's `TEXT` stops at 64 KB).
pub fn long_text() -> &'static str {
    if dialect() == Dialect::MySql { "LONGTEXT" } else { "TEXT" }
}

/// `CREATE TABLE IF NOT EXISTS`. Keys are `VARCHAR(255)` because MariaDB cannot index `TEXT`;
/// the MariaDB tables are binary-collated so comparisons are exact like in the other two.
pub async fn create_table(conn: &mut Conn, name: &str, columns: &str) -> Result<()> {
    let options = if dialect() == Dialect::MySql {
        " ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_bin"
    } else {
        ""
    };
    query(&format!("CREATE TABLE IF NOT EXISTS {name} ({columns}){options}")).execute(conn).await?;
    Ok(())
}

/// `CREATE INDEX IF NOT EXISTS`, which MySQL lacks.
pub async fn create_index(conn: &mut Conn, name: &str, table: &str, columns: &str) -> Result<()> {
    if dialect() == Dialect::MySql {
        let exists = query(
            "SELECT index_name FROM information_schema.statistics
             WHERE table_schema = DATABASE() AND table_name = ? AND index_name = ?",
        )
        .bind(table.to_string())
        .bind(name.to_string())
        .fetch_optional(&mut *conn)
        .await?
        .is_some();
        if exists {
            return Ok(());
        }
    }
    let guard = if dialect() == Dialect::MySql { "" } else { "IF NOT EXISTS " };
    query(&format!("CREATE INDEX {guard}{name} ON {table} ({columns})")).execute(conn).await?;
    Ok(())
}

/// `INSERT ... ON CONFLICT DO UPDATE` in the dialect at hand. Binds one `?` per column in
/// `columns` order; on conflict the `update` columns take the new values.
pub fn upsert(table: &str, columns: &[&str], key: &[&str], update: &[&str]) -> String {
    let names = columns.join(", ");
    let marks = vec!["?"; columns.len()].join(", ");
    let clause = if dialect() == Dialect::MySql {
        let sets: Vec<String> = update.iter().map(|c| format!("{c} = VALUES({c})")).collect();
        format!("ON DUPLICATE KEY UPDATE {}", sets.join(", "))
    } else {
        let sets: Vec<String> = update.iter().map(|c| format!("{c} = excluded.{c}")).collect();
        format!("ON CONFLICT ({}) DO UPDATE SET {}", key.join(", "), sets.join(", "))
    };
    format!("INSERT INTO {table} ({names}) VALUES ({marks}) {clause}")
}

pub async fn next_rev(conn: &mut Conn) -> Result<i64> {
    // No `UPDATE ... RETURNING` in MariaDB. The row lock taken here also serializes writers.
    query("UPDATE counter SET rev = rev + 1").execute(&mut *conn).await?;
    current_rev(conn).await
}

pub async fn current_rev(conn: &mut Conn) -> Result<i64> {
    n(&query("SELECT rev FROM counter").fetch_one(conn).await?, 0)
}

/// Creates missing tables. Safe to run repeatedly.
pub async fn init(conn: &mut Conn) -> Result<()> {
    create_table(conn, "counter", "rev BIGINT NOT NULL").await?;
    if query("SELECT rev FROM counter").fetch_optional(&mut *conn).await?.is_none() {
        query("INSERT INTO counter (rev) VALUES (0)").execute(&mut *conn).await?;
    }
    let text = long_text();
    create_table(
        conn,
        "calendars",
        "id VARCHAR(255) PRIMARY KEY, owner VARCHAR(255) NOT NULL, name VARCHAR(255) NOT NULL,
         color VARCHAR(32) NOT NULL, rev BIGINT NOT NULL, deleted BIGINT NOT NULL DEFAULT 0",
    )
    .await?;
    create_index(conn, "calendars_owner_rev", "calendars", "owner, rev").await?;
    create_table(
        conn,
        "events",
        &format!(
            "id VARCHAR(255) PRIMARY KEY, owner VARCHAR(255) NOT NULL, calendar_id VARCHAR(255) NOT NULL,
             title VARCHAR(2048) NOT NULL, location VARCHAR(2048) NOT NULL, description {text} NOT NULL,
             starts_at VARCHAR(32) NOT NULL, ends_at VARCHAR(32) NOT NULL, tz VARCHAR(100) NOT NULL,
             status VARCHAR(32) NOT NULL DEFAULT 'busy',
             rev BIGINT NOT NULL, deleted BIGINT NOT NULL DEFAULT 0,
             `repeat` VARCHAR(32) NOT NULL DEFAULT '', repeat_until VARCHAR(32),
             repeat_interval BIGINT NOT NULL DEFAULT 1, repeat_days BIGINT NOT NULL DEFAULT 0,
             repeat_nth BIGINT NOT NULL DEFAULT 0, exceptions {text} NOT NULL, remind BIGINT,
             attendees {text} NOT NULL, color VARCHAR(32), private BIGINT NOT NULL DEFAULT 0,
             categories {text} NOT NULL"
        ),
    )
    .await?;
    create_index(conn, "events_owner_rev", "events", "owner, rev").await?;
    create_index(conn, "events_calendar", "events", "calendar_id").await?;
    crate::share::init_db(conn).await?;
    crate::auth::init_db(conn).await
}

