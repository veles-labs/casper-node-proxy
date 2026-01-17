//! Diesel-backed data models and runtime network configuration.

use diesel::prelude::*;

use crate::schema::{config, network};

/// Row from the `network` table.
#[derive(Debug, Clone, Queryable, Identifiable)]
#[diesel(table_name = network)]
#[diesel(primary_key(network_name))]
pub struct NetworkRow {
    pub network_name: String,
    pub chain_name: String,
}

/// Row from the `config` table.
#[derive(Debug, Clone, Queryable, Identifiable, Associations)]
#[diesel(table_name = config)]
#[diesel(primary_key(network_name))]
#[diesel(belongs_to(NetworkRow, foreign_key = network_name))]
pub struct ConfigRow {
    pub network_name: String,
    pub rest: String,
    pub sse: String,
    pub rpc: String,
    pub binary: String,
    pub gossip: String,
}

/// Combined runtime configuration for a network entry.
#[derive(Debug, Clone)]
pub struct NetworkConfig {
    pub network_name: String,
    pub chain_name: String,
    pub rest: String,
    pub sse: String,
    pub rpc: String,
    pub binary: String,
    pub gossip: String,
}

impl NetworkConfig {
    /// Merge the DB rows into a single runtime config object.
    pub fn from_rows(network: NetworkRow, config: ConfigRow) -> Self {
        Self {
            network_name: network.network_name,
            chain_name: network.chain_name,
            rest: config.rest,
            sse: config.sse,
            rpc: config.rpc,
            binary: config.binary,
            gossip: config.gossip,
        }
    }
}
