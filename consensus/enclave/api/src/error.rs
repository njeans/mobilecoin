// Copyright (c) 2018-2022 The MobileCoin Foundation

//! Enclave API Errors

use crate::FeeMapError;
use alloc::string::String;
use displaydoc::Display;
use mc_attest_core::{IntelSealingError, ParseSealedError, SgxError};
use mc_attest_enclave_api::Error as AttestEnclaveError;
use mc_crypto_keys::{KeyError, SignatureError};
use mc_crypto_message_cipher::CipherError as MessageCipherError;
use mc_sgx_compat::sync::PoisonError;
use mc_transaction_core::{mint::MintValidationError, validation::TransactionValidationError};
use mc_util_serial::{
    decode::Error as RmpDecodeError, encode::Error as RmpEncodeError,
    DecodeError as ProstDecodeError, EncodeError as ProstEncodeError,
};
use serde::{Deserialize, Serialize};

/// An enumeration of errors which can occur inside a consensus enclave.
#[derive(Clone, Debug, Deserialize, Display, PartialEq, PartialOrd, Serialize)]
pub enum Error {
    /// Error communicating with SGX: {0}
    Sgx(SgxError),

    /// Attested AKE error: {0}
    Attest(AttestEnclaveError),

    /// Local cache cipher error: {0}
    CacheCipher(MessageCipherError),

    /// Error while serializing/deserializing
    Serialization,

    /// Another thread crashed while holding a lock
    Poison,

    /// Malformed transaction: {0}
    MalformedTx(TransactionValidationError),

    /// Malformed minting transaction: {0}
    MalformedMintingTx(MintValidationError),

    /// Invalid membership proof provided by local system
    InvalidLocalMembershipProof,

    /// Invalid membership root element provided by local system
    InvalidLocalMembershipRootElement,

    /// Form block error: {0}
    FormBlock(String),

    /// Signature error
    Signature,

    /// Fee public address error: {0}
    FeePublicAddress(String),

    /// Invalid fee configuration: {0}
    FeeMap(FeeMapError),

    /// Enclave not initialized
    NotInitialized,

    /// Block Version Error: {0}
    BlockVersion(String),

    /// Sealing Error: {0}
    IntelSealing(IntelSealingError),

    /// Missing governors signature
    MissingGovernorsSignature,

    /// Invalid governors signature
    InvalidGovernorsSignature,

    /// Failed parsing minting trust root public key
    ParseMintingTrustRootPublicKey(KeyError),
}

impl From<ParseSealedError> for Error {
    fn from(src: ParseSealedError) -> Self {
        Error::IntelSealing(src.into())
    }
}

impl From<IntelSealingError> for Error {
    fn from(src: IntelSealingError) -> Self {
        Error::IntelSealing(src)
    }
}

impl From<MessageCipherError> for Error {
    fn from(src: MessageCipherError) -> Self {
        Error::CacheCipher(src)
    }
}

impl<T> From<PoisonError<T>> for Error {
    fn from(_src: PoisonError<T>) -> Self {
        Error::Poison
    }
}

impl From<SgxError> for Error {
    fn from(src: SgxError) -> Self {
        Error::Sgx(src)
    }
}

impl From<RmpEncodeError> for Error {
    fn from(_src: RmpEncodeError) -> Error {
        Error::Serialization
    }
}

impl From<RmpDecodeError> for Error {
    fn from(_src: RmpDecodeError) -> Error {
        Error::Serialization
    }
}

impl From<ProstEncodeError> for Error {
    fn from(_src: ProstEncodeError) -> Error {
        Error::Serialization
    }
}

impl From<ProstDecodeError> for Error {
    fn from(_src: ProstDecodeError) -> Error {
        Error::Serialization
    }
}

impl From<TransactionValidationError> for Error {
    fn from(src: TransactionValidationError) -> Error {
        Error::MalformedTx(src)
    }
}

impl From<MintValidationError> for Error {
    fn from(src: MintValidationError) -> Error {
        Error::MalformedMintingTx(src)
    }
}

impl From<AttestEnclaveError> for Error {
    fn from(src: AttestEnclaveError) -> Error {
        Error::Attest(src)
    }
}

impl From<SignatureError> for Error {
    fn from(_src: SignatureError) -> Error {
        Error::Signature
    }
}

impl From<FeeMapError> for Error {
    fn from(src: FeeMapError) -> Error {
        Error::FeeMap(src)
    }
}
