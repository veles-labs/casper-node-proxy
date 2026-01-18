//! Database helpers for configuring and loading network metadata.

use diesel::prelude::*;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel::{pg::PgConnection, sqlite::SqliteConnection};
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::metrics::AppMetrics;
use crate::models::{ConfigRow, NetworkConfig, NetworkRow};
use crate::schema::{config, network};

/// Shared Diesel connection pool type across supported backends.
pub enum DbPool {
    Sqlite(Pool<ConnectionManager<SqliteConnection>>),
    Postgres(Pool<ConnectionManager<PgConnection>>),
}

/// Embedded Diesel migrations stored under `migrations/`.
pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

/// Create a connection pool for the configured database URL.
pub fn init_pool(database_url: &str) -> Result<DbPool, Box<dyn std::error::Error + Send + Sync>> {
    match backend_from_url(database_url)? {
        DbBackend::Sqlite => {
            let manager = ConnectionManager::<SqliteConnection>::new(database_url);
            let pool = Pool::builder().build(manager)?;
            Ok(DbPool::Sqlite(pool))
        }
        DbBackend::Postgres => {
            let manager = ConnectionManager::<PgConnection>::new(database_url);
            let pool = Pool::builder().build(manager)?;
            Ok(DbPool::Postgres(pool))
        }
    }
}

/// Apply pending migrations on startup.
pub fn run_migrations(
    pool: &DbPool,
    metrics: &AppMetrics,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let result = match pool {
        DbPool::Sqlite(pool) => {
            let start = Instant::now();
            match pool.get() {
                Ok(mut conn) => {
                    metrics.observe_db_pool_wait("migrations", start.elapsed());
                    conn.run_pending_migrations(MIGRATIONS)
                        .map(|_| ())
                        .map_err(|err| err.into())
                }
                Err(err) => Err(err.into()),
            }
        }
        DbPool::Postgres(pool) => {
            let start = Instant::now();
            match pool.get() {
                Ok(mut conn) => {
                    metrics.observe_db_pool_wait("migrations", start.elapsed());
                    conn.run_pending_migrations(MIGRATIONS)
                        .map(|_| ())
                        .map_err(|err| err.into())
                }
                Err(err) => Err(err.into()),
            }
        }
    };
    match &result {
        Ok(()) => metrics.increment_db_migration("success"),
        Err(_) => metrics.increment_db_migration("failure"),
    }
    result
}

/// Load network + config rows into in-memory config objects.
pub fn load_networks(
    pool: &DbPool,
    metrics: &AppMetrics,
) -> Result<Vec<NetworkConfig>, Box<dyn std::error::Error + Send + Sync>> {
    let rows: Vec<(NetworkRow, ConfigRow)> = match pool {
        DbPool::Sqlite(pool) => {
            let start = Instant::now();
            let mut conn = pool.get()?;
            metrics.observe_db_pool_wait("load_networks", start.elapsed());
            network::table.inner_join(config::table).load(&mut conn)?
        }
        DbPool::Postgres(pool) => {
            let start = Instant::now();
            let mut conn = pool.get()?;
            metrics.observe_db_pool_wait("load_networks", start.elapsed());
            network::table.inner_join(config::table).load(&mut conn)?
        }
    };

    let configs = rows
        .into_iter()
        .map(|(net, cfg)| NetworkConfig::from_rows(net, cfg))
        .collect();

    Ok(configs)
}

pub fn spawn_pool_metrics(pool: &DbPool, metrics: Arc<AppMetrics>) {
    match pool {
        DbPool::Sqlite(pool) => spawn_pool_metrics_inner(pool.clone(), metrics),
        DbPool::Postgres(pool) => spawn_pool_metrics_inner(pool.clone(), metrics),
    }
}

fn spawn_pool_metrics_inner<C>(pool: Pool<ConnectionManager<C>>, metrics: Arc<AppMetrics>)
where
    C: diesel::Connection + diesel::r2d2::R2D2Connection + 'static,
{
    update_pool_metrics(&pool, &metrics);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        loop {
            interval.tick().await;
            update_pool_metrics(&pool, &metrics);
        }
    });
}

fn update_pool_metrics<C>(pool: &Pool<ConnectionManager<C>>, metrics: &AppMetrics)
where
    C: diesel::Connection + diesel::r2d2::R2D2Connection + 'static,
{
    let state = pool.state();
    metrics.set_db_pool_stats(state.connections as u32, state.idle_connections as u32);
}

enum DbBackend {
    Sqlite,
    Postgres,
}

fn backend_from_url(
    database_url: &str,
) -> Result<DbBackend, Box<dyn std::error::Error + Send + Sync>> {
    let scheme = database_url.split_once("://").map(|(scheme, _)| scheme);
    match scheme {
        Some("postgres") | Some("postgresql") => Ok(DbBackend::Postgres),
        Some("sqlite") => Ok(DbBackend::Sqlite),
        Some(unsupported) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("unsupported DATABASE_URL scheme: {unsupported}"),
        )
        .into()),
        None => Ok(DbBackend::Sqlite),
    }
}
