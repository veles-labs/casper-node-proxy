//! Database helpers for configuring and loading network metadata.

use diesel::prelude::*;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel::{pg::PgConnection, sqlite::SqliteConnection};
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};

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
pub fn run_migrations(pool: &DbPool) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match pool {
        DbPool::Sqlite(pool) => {
            let mut conn = pool.get()?;
            conn.run_pending_migrations(MIGRATIONS)?;
        }
        DbPool::Postgres(pool) => {
            let mut conn = pool.get()?;
            conn.run_pending_migrations(MIGRATIONS)?;
        }
    }
    Ok(())
}

/// Load network + config rows into in-memory config objects.
pub fn load_networks(
    pool: &DbPool,
) -> Result<Vec<NetworkConfig>, Box<dyn std::error::Error + Send + Sync>> {
    let rows: Vec<(NetworkRow, ConfigRow)> = match pool {
        DbPool::Sqlite(pool) => {
            let mut conn = pool.get()?;
            network::table.inner_join(config::table).load(&mut conn)?
        }
        DbPool::Postgres(pool) => {
            let mut conn = pool.get()?;
            network::table.inner_join(config::table).load(&mut conn)?
        }
    };

    let configs = rows
        .into_iter()
        .map(|(net, cfg)| NetworkConfig::from_rows(net, cfg))
        .collect();

    Ok(configs)
}

enum DbBackend {
    Sqlite,
    Postgres,
}

fn backend_from_url(
    database_url: &str,
) -> Result<DbBackend, Box<dyn std::error::Error + Send + Sync>> {
    let scheme = database_url
        .split_once("://")
        .map(|(scheme, _)| scheme);
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
