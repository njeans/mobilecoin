// Copyright (c) 2018-2020 MobileCoin Inc.

//! Peer-to-Peer Networking.

extern crate alloc;

mod connection;
mod consensus_msg;
mod error;
mod sync;
mod threaded_broadcaster;
mod threaded_broadcaster_retry;
mod traits;

pub use crate::{
    connection::PeerConnection,
    consensus_msg::{ConsensusMsg, ConsensusMsgError, TxProposeAAD, VerifiedConsensusMsg},
    error::{Error, Result},
    threaded_broadcaster::ThreadedBroadcaster,
    threaded_broadcaster_retry::{
        FibonacciRetryPolicy as ThreadedBroadcasterFibonacciRetryPolicy,
        RetryPolicy as ThreadedBroadcasterRetryPolicy, DEFAULT_RETRY_MAX_ATTEMPTS,
    },
    traits::{ConsensusConnection, RetryableConsensusConnection},
};
