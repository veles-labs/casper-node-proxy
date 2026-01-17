CREATE TABLE network (
    network_name TEXT PRIMARY KEY NOT NULL,
    chain_name TEXT NOT NULL
);

CREATE TABLE config (
    network_name TEXT PRIMARY KEY NOT NULL,
    rest TEXT NOT NULL,
    sse TEXT NOT NULL,
    rpc TEXT NOT NULL,
    binary TEXT NOT NULL,
    gossip TEXT NOT NULL,
    FOREIGN KEY (network_name) REFERENCES network (network_name)
);
