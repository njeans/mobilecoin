// Copyright (c) 2018-2022 The MobileCoin Foundation

//! Connection mock and test utilities

use mc_util_uri::ConsensusClientUri;
use std::str::FromStr;

mod blockchain;
mod user_tx;

pub fn test_client_uri(node_id: u32) -> ConsensusClientUri {
    ConsensusClientUri::from_str(&format!("mc://node{}.test.com/", node_id))
        .expect("Could not construct client uri from string")
}

pub use crate::{blockchain::MockBlockchainConnection, user_tx::MockUserTxConnection};
