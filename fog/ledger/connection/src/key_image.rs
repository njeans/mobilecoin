// Copyright (c) 2018-2022 The MobileCoin Foundation

use super::Error;
use displaydoc::Display;
use grpcio::{ChannelBuilder, Environment};
use mc_attest_verifier::Verifier;
use mc_common::logger::{o, Logger};
use mc_fog_api::{ledger::KeyImageResultCode, ledger_grpc::FogKeyImageApiClient};
use mc_fog_enclave_connection::EnclaveConnection;
use mc_fog_types::ledger::{
    CheckKeyImagesRequest, CheckKeyImagesResponse, KeyImageQuery, KeyImageResult,
};
use mc_fog_uri::FogLedgerUri;
use mc_transaction_core::{ring_signature::KeyImage, BlockIndex};
use mc_util_grpc::{ConnectionUriGrpcioChannel, GrpcRetryConfig};
use std::sync::Arc;

/// An attested connection to the Fog Key Image service.
pub struct FogKeyImageGrpcClient {
    conn: EnclaveConnection<FogLedgerUri, FogKeyImageApiClient>,
    grpc_retry_config: GrpcRetryConfig,
    uri: FogLedgerUri,
}

impl FogKeyImageGrpcClient {
    /// Create a new client object
    ///
    /// Arguments:
    /// uri: The uri to connect to
    /// grpc_retry_config: The retry policy to use when connecting
    /// verifier: The attestation verifier
    /// env: The grpc environment (thread pool) to use for this connection
    /// logger: for logging
    pub fn new(
        uri: FogLedgerUri,
        grpc_retry_config: GrpcRetryConfig,
        verifier: Verifier,
        env: Arc<Environment>,
        logger: Logger,
    ) -> Self {
        let logger = logger.new(o!("mc.ledger.cxn" => uri.to_string()));

        let ch = ChannelBuilder::default_channel_builder(env).connect_to_uri(&uri, &logger);

        let grpc_client = FogKeyImageApiClient::new(ch);

        Self {
            conn: EnclaveConnection::new(uri.clone(), grpc_client, verifier, logger),
            grpc_retry_config,
            uri,
        }
    }

    /// Make a private request to check the validity of several key images
    pub fn check_key_images(
        &mut self,
        key_images: &[KeyImage],
    ) -> Result<CheckKeyImagesResponse, Error> {
        let request = CheckKeyImagesRequest {
            queries: key_images
                .iter()
                .map(|key_image| KeyImageQuery {
                    key_image: *key_image,
                    start_block: 0,
                })
                .collect(),
        };

        let retry_config = self.grpc_retry_config;

        let response: CheckKeyImagesResponse = retry_config
            .retry(|| self.conn.retriable_encrypted_enclave_request(&request, &[]))
            .map_err(|err| Error::Connection(self.uri.clone(), err))?;

        Ok(response)
    }
}

/// An extension trait that adds a convenience method to check the status of a
/// key image result.
pub trait KeyImageResultExtension {
    /// Check the status of a key image query. A `None` value indicates the key
    /// image has not been found. Some(spent_at) indicates the key image
    /// appeared at block index `spent_at`.
    fn status(&self) -> Result<Option<BlockIndex>, KeyImageQueryError>;
}

impl KeyImageResultExtension for KeyImageResult {
    /// Map the protobuf KeyImageResult type to a more idiomatic rust Result
    /// type
    fn status(&self) -> Result<Option<BlockIndex>, KeyImageQueryError> {
        if self.key_image_result_code == KeyImageResultCode::Spent as u32 {
            Ok(Some(self.spent_at))
        } else if self.key_image_result_code == KeyImageResultCode::NotSpent as u32 {
            Ok(None)
        } else if self.key_image_result_code == KeyImageResultCode::KeyImageError as u32 {
            Err(KeyImageQueryError::KeyImageError)
        } else {
            Err(KeyImageQueryError::UnknownStatus(
                self.key_image_result_code,
            ))
        }
    }
}

/// Errors that occur from an individual check key image query
#[derive(Display, Debug, Eq, PartialEq)]
pub enum KeyImageQueryError {
    /// Nonspecific server error handling the request
    // FIXME: The server should at least seperate "invalid key image", "rate
    // limited", "database", from other error types
    KeyImageError,
    /// Unknown status code: {0}
    UnknownStatus(u32),
}
