//! Database helpers for configuring and loading network metadata.

use diesel::prelude::*;
use diesel::r2d2::{ConnectionManager, Pool};
use diesel_migrations::{EmbeddedMigrations, MigrationHarness, embed_migrations};

use crate::models::{ConfigRow, NetworkConfig, NetworkRow};
use crate::schema::{config, network};

/// Shared Diesel connection pool type.
pub type DbPool = Pool<ConnectionManager<SqliteConnection>>;

/// Embedded Diesel migrations stored under `migrations/`.
pub const MIGRATIONS: EmbeddedMigrations = embed_migrations!("migrations");

/// Create a SQLite connection pool for the configured database URL.
pub fn init_pool(database_url: &str) -> Result<DbPool, Box<dyn std::error::Error + Send + Sync>> {
    let manager = ConnectionManager::<SqliteConnection>::new(database_url);
    let pool = Pool::builder().build(manager)?;
    Ok(pool)
}

/// Apply pending migrations on startup.
pub fn run_migrations(pool: &DbPool) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut conn = pool.get()?;
    conn.run_pending_migrations(MIGRATIONS)?;
    Ok(())
}

/// Load network + config rows into in-memory config objects.
pub fn load_networks(
    pool: &DbPool,
) -> Result<Vec<NetworkConfig>, Box<dyn std::error::Error + Send + Sync>> {
    let mut conn = pool.get()?;
    let rows: Vec<(NetworkRow, ConfigRow)> =
        network::table.inner_join(config::table).load(&mut conn)?;

    let configs = rows
        .into_iter()
        .map(|(net, cfg)| NetworkConfig::from_rows(net, cfg))
        .collect();

    Ok(configs)
}
