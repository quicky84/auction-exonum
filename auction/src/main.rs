extern crate auction_exonum as auction;
extern crate exonum;
extern crate time_wizz;

use exonum::blockchain::{GenesisConfig, ValidatorKeys};
use exonum::node::{Node, NodeApiConfig, NodeConfig};
use exonum::storage::MemoryDB;

use std::sync::mpsc::{Receiver, Sender};
use std::sync::mpsc;
use std::thread;

use time_wizz::TimeService;
use auction::AuctionService;

fn node_config() -> NodeConfig {
    let (consensus_public_key, consensus_secret_key) = exonum::crypto::gen_keypair();
    let (service_public_key, service_secret_key) = exonum::crypto::gen_keypair();

    let validator_keys = ValidatorKeys {
        consensus_key: consensus_public_key,
        service_key: service_public_key,
    };

    let genesis = GenesisConfig::new(vec![validator_keys].into_iter());

    let api_address = "0.0.0.0:8000".parse().unwrap();
    let api_cfg = NodeApiConfig {
        public_api_address: Some(api_address),
        ..Default::default()
    };

    let peer_address = "0.0.0.0:2000".parse().unwrap();

    NodeConfig {
        listen_address: peer_address,
        peers: vec![],
        service_public_key,
        service_secret_key,
        consensus_public_key,
        consensus_secret_key,
        genesis,
        external_address: None,
        network: Default::default(),
        whitelist: Default::default(),
        api: api_cfg,
        mempool: Default::default(),
        services_configs: Default::default(),
        database: None,
    }
}

fn main() {
    exonum::helpers::init_logger().unwrap();

    let (send, recv) = mpsc::channel();

    println!("Creating in-memory database...");
    let mut ts = TimeService::new();
    ts.subscribe(send.clone());

    let node = Node::new(
        MemoryDB::new(),
        vec![Box::new(AuctionService), Box::new(ts)],
        node_config(),
    );

    thread::spawn(move || loop {
        println!("{:?}", recv.recv().unwrap());
    });

    println!("Starting a single node...");
    println!("Blockchain is ready for transactions!");
    node.run().unwrap();
}
