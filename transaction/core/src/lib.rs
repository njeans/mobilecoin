// Copyright (c) 2018-2022 The MobileCoin Foundation

//! MobileCoin transaction data types, transaction construction and validation
//! routines

#![no_std]
#![deny(missing_docs)]

extern crate alloc;

#[cfg(test)]
#[macro_use]
extern crate std;

#[macro_use]
extern crate lazy_static;

use crate::onetime_keys::create_shared_secret;
use mc_crypto_keys::{KeyError, RistrettoPrivate, RistrettoPublic};

mod amount;
mod blockchain;
mod domain_separators;
mod input_rules;
mod memo;
mod signed_contingent_input;
mod token;
mod tx_error;
mod tx_out_gift_code;

pub mod constants;
pub mod encrypted_fog_hint;
pub mod fog_hint;
pub mod membership_proofs;
pub mod mint;
pub mod onetime_keys;
pub mod range_proofs;
pub mod ring_signature;
pub mod tx;
pub mod validation;

#[cfg(test)]
pub mod proptest_fixtures;

pub use amount::{Amount, AmountError, Commitment, CompressedCommitment, MaskedAmount};
pub use blockchain::*;
pub use input_rules::{InputRuleError, InputRules};
pub use memo::{EncryptedMemo, MemoError, MemoPayload};
pub use signed_contingent_input::{
    SignedContingentInput, SignedContingentInputError, UnmaskedAmount,
};
pub use token::{tokens, Token, TokenId};
pub use tx::MemoContext;
pub use tx_error::{NewMemoError, NewTxError, ViewKeyMatchError};
pub use tx_out_gift_code::TxOutGiftCode;

use core::convert::TryFrom;
use mc_account_keys::AccountKey;
use onetime_keys::recover_public_subaddress_spend_key;
use tx::TxOut;

/// Get the shared secret for a transaction output.
///
/// # Arguments
/// * `view_key` - The recipient's private View key.
/// * `tx_public_key` - The public key of the transaction.
pub fn get_tx_out_shared_secret(
    view_key: &RistrettoPrivate,
    tx_public_key: &RistrettoPublic,
) -> RistrettoPublic {
    create_shared_secret(tx_public_key, view_key)
}

/// Helper which checks if a particular subaddress of an account key matches a
/// TxOut
///
/// This is not the most efficient way to check when you have many subaddresses,
/// for that you should create a table and use
/// recover_public_subaddress_spend_key directly.
///
/// However some clients are only using one or two subaddresses.
/// Validating that a TxOut is owned by the change subaddress is a frequently
/// needed operation.
pub fn subaddress_matches_tx_out(
    acct: &AccountKey,
    subaddress_index: u64,
    output: &TxOut,
) -> Result<bool, KeyError> {
    let sub_addr_spend = recover_public_subaddress_spend_key(
        acct.view_private_key(),
        &RistrettoPublic::try_from(&output.target_key)?,
        &RistrettoPublic::try_from(&output.public_key)?,
    );
    Ok(sub_addr_spend == RistrettoPublic::from(&acct.subaddress_spend_private(subaddress_index)))
}
