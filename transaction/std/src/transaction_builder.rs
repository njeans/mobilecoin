// Copyright (c) 2018-2022 The MobileCoin Foundation

//! Utility for building and signing a transaction.
//!
//! See https://cryptonote.org/img/cryptonote_transaction.png

use crate::{
    input_materials::InputMaterials, InputCredentials, MemoBuilder, ReservedDestination,
    TxBuilderError,
};
use core::{cmp::min, fmt::Debug};
use mc_account_keys::PublicAddress;
use mc_crypto_keys::{CompressedRistrettoPublic, RistrettoPrivate, RistrettoPublic};
use mc_fog_report_validation::FogPubkeyResolver;
use mc_transaction_core::{
    encrypted_fog_hint::EncryptedFogHint,
    fog_hint::FogHint,
    onetime_keys::create_shared_secret,
    ring_signature::{InputRing, OutputSecret, SignatureRctBulletproofs},
    tokens::Mob,
    tx::{Tx, TxIn, TxOut, TxOutConfirmationNumber, TxPrefix},
    Amount, BlockVersion, MemoContext, MemoPayload, NewMemoError, SignedContingentInput,
    SignedContingentInputError, Token, TokenId,
};
use mc_util_from_random::FromRandom;
use rand_core::{CryptoRng, RngCore};
use std::cmp::Ordering;

/// A trait used to compare the transaction outputs
pub trait TxOutputsOrdering {
    /// comparer method
    fn cmp(a: &CompressedRistrettoPublic, b: &CompressedRistrettoPublic) -> Ordering;
}

/// Default implementation for transaction outputs
pub struct DefaultTxOutputsOrdering;

impl TxOutputsOrdering for DefaultTxOutputsOrdering {
    fn cmp(a: &CompressedRistrettoPublic, b: &CompressedRistrettoPublic) -> Ordering {
        a.cmp(b)
    }
}

/// Helper utility for building and signing a CryptoNote-style transaction,
/// and attaching fog hint and memos as appropriate.
///
/// This is generic over FogPubkeyResolver because there are several reasonable
/// implementations of that.
///
/// This is generic over MemoBuilder to allow injecting a policy for how to
/// use the memos in the TxOuts.
#[derive(Debug)]
pub struct TransactionBuilder<FPR: FogPubkeyResolver> {
    /// The block version that we are targeting for this transaction
    block_version: BlockVersion,
    /// The input materials used to form the transaction.
    input_materials: Vec<InputMaterials>,
    /// The outputs created by the transaction, and associated output secrets.
    outputs_and_secrets: Vec<(TxOut, OutputSecret)>,
    /// The tombstone_block value, a block index in which the transaction
    /// expires, and can no longer be added to the blockchain
    tombstone_block: u64,
    /// The fee paid in connection to this transaction
    /// If mixed transactions feature is off, then everything must be this token
    /// id.
    fee: Amount,
    /// The source of validated fog pubkeys used for this transaction
    fog_resolver: FPR,
    /// The limit on the tombstone block value imposed pubkey_expiry values in
    /// fog pubkeys used so far
    fog_tombstone_block_limit: u64,
    /// An policy object implementing MemoBuilder which constructs memos for
    /// this transaction.
    ///
    /// This is an Option in order to allow working around the borrow checker.
    /// Box<dyn ...> is used because having more generic parameters creates more
    /// types that SDKs must bind to if they support multiple memo builder
    /// types.
    memo_builder: Option<Box<dyn MemoBuilder + 'static + Send + Sync>>,
}

impl<FPR: FogPubkeyResolver> TransactionBuilder<FPR> {
    /// Initializes a new TransactionBuilder.
    ///
    /// # Arguments
    /// * `block_version` - The block version rules to use when building this
    ///   transaction
    /// * `fee` - The fee (and token id) to use for this transaction. Note: The
    ///   fee token id cannot be changed later, and before mixed transactions
    ///   feature, every input and output must have this token id.
    /// * `fog_resolver` - Source of validated fog keys to use with this
    ///   transaction
    /// * `memo_builder` - An object which creates memos for the TxOuts in this
    ///   transaction
    pub fn new<MB: MemoBuilder + 'static + Send + Sync>(
        block_version: BlockVersion,
        fee: Amount,
        fog_resolver: FPR,
        memo_builder: MB,
    ) -> Result<Self, TxBuilderError> {
        TransactionBuilder::new_with_box(block_version, fee, fog_resolver, Box::new(memo_builder))
    }

    /// Initializes a new TransactionBuilder, using a Box<dyn MemoBuilder>
    /// instead of statically typed
    ///
    /// # Arguments
    /// * `block_version` - The block version rules to use when building this
    ///   transaction
    /// * `fee` - The fee (and token id) to use for this transaction. Note: The
    ///   fee token id cannot be changed later, and before mixed transactions
    ///   feature, every input and output must have the same token id as the
    ///   fee.
    /// * `fog_resolver` - Source of validated fog keys to use with this
    ///   transaction
    /// * `memo_builder` - An object which creates memos for the TxOuts in this
    ///   transaction
    pub fn new_with_box(
        block_version: BlockVersion,
        fee: Amount,
        fog_resolver: FPR,
        mut memo_builder: Box<dyn MemoBuilder + Send + Sync>,
    ) -> Result<Self, TxBuilderError> {
        // make sure that the memo builder
        // is initialized to the same fee as the transaction builder
        memo_builder.set_fee(fee)?;
        Ok(TransactionBuilder {
            block_version,
            input_materials: Vec::new(),
            outputs_and_secrets: Vec::new(),
            tombstone_block: u64::max_value(),
            fee,
            fog_resolver,
            fog_tombstone_block_limit: u64::max_value(),
            memo_builder: Some(memo_builder),
        })
    }

    /// Add an Input to the transaction.
    ///
    /// # Arguments
    /// * `input_credentials` - Credentials required to construct a ring
    ///   signature for an input.
    pub fn add_input(&mut self, input_credentials: InputCredentials) {
        self.input_materials
            .push(InputMaterials::Signable(input_credentials));
    }

    /// Add a pre-signed Input to the transaction, also fulfilling any
    /// requirements imposed by the signed rules, so that our transaction
    /// will be valid.
    ///
    /// Note: Before adding a signed_contingent_input, you probably want to:
    /// * validate it (call .validate())
    /// * check if key image appreared already (call .key_image())
    /// * provide merkle proofs of membership for each ring member (see
    ///   .tx_out_global_indices)
    ///
    /// # Arguments
    /// * `signed_contingent_input` - The pre-signed input we are adding
    pub fn add_presigned_input(
        &mut self,
        sci: SignedContingentInput,
    ) -> Result<(), SignedContingentInputError> {
        // TODO: If there is a block version change that could cause an incompatibility,
        // we should check for it here, e.g. if sci.block_version differs from
        // self.block_version
        if let Some(rules) = sci.tx_in.input_rules.as_ref() {
            // Enforce all rules so that our transaction will be valid
            if rules.required_outputs.len() != sci.required_output_amounts.len() {
                return Err(SignedContingentInputError::WrongNumberOfRequiredOutputAmounts);
            }
            // 1. Required outputs
            for (required_output, unmasked_amount) in rules
                .required_outputs
                .iter()
                .zip(sci.required_output_amounts.iter())
            {
                // Check if the required output is already there
                if !self
                    .outputs_and_secrets
                    .iter()
                    .any(|(output, _sec)| output == required_output)
                {
                    // If not, add it
                    self.outputs_and_secrets
                        .push((required_output.clone(), unmasked_amount.clone().into()));
                }
            }
            // 2. Max tombstone block
            if rules.max_tombstone_block != 0 {
                self.impose_tombstone_block_limit(rules.max_tombstone_block);
            }
        }

        self.add_presigned_input_raw(sci);
        Ok(())
    }

    /// Add a pre-signed Input to the transaction, without also fulfilling
    /// any of its rules. You will have to add any required outputs, adjust
    /// tombstone block, etc., for the transaction to be valid.
    ///
    /// Note: Before adding a signed_contingent_input, you probably want to:
    /// * validate it (call .validate())
    /// * check if key image appreared already (call .key_image())
    /// * provide merkle proofs of membership for each ring member (see
    ///   .tx_out_global_indices)
    ///
    /// # Arguments
    /// * `signed_contingent_input` - The pre-signed input we are adding
    pub fn add_presigned_input_raw(&mut self, sci: SignedContingentInput) {
        self.input_materials.push(InputMaterials::Presigned(sci));
    }

    /// Add a non-change output to the transaction.
    ///
    /// If a sender memo credential has been set, this will create an
    /// authenticated sender memo for the TxOut. Otherwise the memo will be
    /// unused.
    ///
    /// # Arguments
    /// * `amount` - The amount of this output
    /// * `recipient` - The recipient's public address
    /// * `rng` - RNG used to generate blinding for commitment
    pub fn add_output<RNG: CryptoRng + RngCore>(
        &mut self,
        amount: Amount,
        recipient: &PublicAddress,
        rng: &mut RNG,
    ) -> Result<(TxOut, TxOutConfirmationNumber), TxBuilderError> {
        // Taking self.memo_builder here means that we can call functions on &mut self,
        // and pass them something that has captured the memo builder.
        // Calling take() on Option<Box> is just moving a pointer.
        let mut mb = self
            .memo_builder
            .take()
            .expect("memo builder is missing, this is a logic error");
        let block_version = self.block_version;
        let result = self.add_output_with_fog_hint_address(
            amount,
            recipient,
            recipient,
            |memo_ctxt| {
                if block_version.e_memo_feature_is_supported() {
                    Some(mb.make_memo_for_output(amount, recipient, memo_ctxt)).transpose()
                } else {
                    Ok(None)
                }
            },
            rng,
        );
        // Put the memo builder back
        self.memo_builder = Some(mb);
        result
    }

    /// Add a standard change output to the transaction.
    ///
    /// The change output is meant to send any value in the inputs not already
    /// sent via outputs or fee, back to the sender's address.
    /// The caller should ensure that the math adds up, and that
    /// change_value + total_outlays + fee = total_input_value
    ///
    /// (Here, outlay means a non-change output).
    ///
    /// A change output should be sent to the dedicated change subaddress of the
    /// sender.
    ///
    /// If provided, a Destination memo is attached to this output, which allows
    /// for recoverable transaction history.
    ///
    /// The use of dedicated change subaddress for change outputs allows to
    /// authenticate the contents of destination memos, which are otherwise
    /// unauthenticated.
    ///
    /// # Arguments
    /// * `amount` - The amount of this change output.
    /// * `change_destination` - An object including both a primary address and
    ///   a change subaddress to use to create this change output. The primary
    ///   address is used for the fog hint, the change subaddress owns the
    ///   change output. These can both be obtained from an account key, but
    ///   this API does not require the account key.
    /// * `rng` - RNG used to generate blinding for commitment
    pub fn add_change_output<RNG: CryptoRng + RngCore>(
        &mut self,
        amount: Amount,
        change_destination: &ReservedDestination,
        rng: &mut RNG,
    ) -> Result<(TxOut, TxOutConfirmationNumber), TxBuilderError> {
        // Taking self.memo_builder here means that we can call functions on &mut self,
        // and pass them something that has captured the memo builder.
        // Calling take() on Option<Box> is just moving a pointer.
        let mut mb = self
            .memo_builder
            .take()
            .expect("memo builder is missing, this is a logic error");
        let block_version = self.block_version;
        let result = self.add_output_with_fog_hint_address(
            amount,
            &change_destination.change_subaddress,
            &change_destination.primary_address,
            |memo_ctxt| {
                if block_version.e_memo_feature_is_supported() {
                    Some(mb.make_memo_for_change_output(amount, change_destination, memo_ctxt))
                        .transpose()
                } else {
                    Ok(None)
                }
            },
            rng,
        );
        // Put the memo builder back
        self.memo_builder = Some(mb);
        result
    }

    /// Add an output to the transaction, using `fog_hint_address` to construct
    /// the fog hint.
    ///
    /// This is a private implementation detail, and generally, fog users expect
    /// that the transactions that they recieve from fog belong to the account
    /// that they are using. The only known use-case where recipient and
    /// fog_hint_address are different is when sending change transactions
    /// to oneself, when oneself is a fog user. Sending the change to the
    /// main subaddress means that you don't have to hit fog once for the
    /// main subaddress and once for the change subaddress, so it cuts the
    /// number of requests in half.
    ///
    /// # Arguments
    /// * `amount` - The amount of this output
    /// * `recipient` - The recipient's public address
    /// * `fog_hint_address` - The public address used to create the fog hint
    /// * `memo_fn` - The memo function to use (see TxOut::new_with_memo)
    /// * `rng` - RNG used to generate blinding for commitment
    fn add_output_with_fog_hint_address<RNG: CryptoRng + RngCore>(
        &mut self,
        amount: Amount,
        recipient: &PublicAddress,
        fog_hint_address: &PublicAddress,
        memo_fn: impl FnOnce(MemoContext) -> Result<Option<MemoPayload>, NewMemoError>,
        rng: &mut RNG,
    ) -> Result<(TxOut, TxOutConfirmationNumber), TxBuilderError> {
        let (hint, pubkey_expiry) = create_fog_hint(fog_hint_address, &self.fog_resolver, rng)?;

        if !self.block_version.mixed_transactions_are_supported()
            && self.fee.token_id != amount.token_id
        {
            return Err(TxBuilderError::MixedTransactionsNotAllowed(
                self.fee.token_id,
                amount.token_id,
            ));
        }

        let (tx_out, shared_secret) =
            create_output_with_fog_hint(self.block_version, amount, recipient, hint, memo_fn, rng)?;

        let (amount, blinding) = tx_out
            .masked_amount
            .get_value(&shared_secret)
            .expect("TransactionBuilder created an invalid Amount");
        let output_secret = OutputSecret { amount, blinding };

        self.impose_tombstone_block_limit(pubkey_expiry);

        self.outputs_and_secrets
            .push((tx_out.clone(), output_secret));

        let confirmation = TxOutConfirmationNumber::from(&shared_secret);

        Ok((tx_out, confirmation))
    }

    /// Sets the tombstone block, clamping to smallest pubkey expiry value.
    ///
    /// # Arguments
    /// * `tombstone_block` - Tombstone block number.
    pub fn set_tombstone_block(&mut self, tombstone_block: u64) -> u64 {
        self.tombstone_block = min(tombstone_block, self.fog_tombstone_block_limit);
        self.tombstone_block
    }

    /// Reduce the fog_tombstone_block_limit value by the amount specified,
    /// and propagate this constraint to self.tombstone_block
    fn impose_tombstone_block_limit(&mut self, pubkey_expiry: u64) {
        // Reduce fog tombstone block limit value if necessary
        self.fog_tombstone_block_limit = min(self.fog_tombstone_block_limit, pubkey_expiry);
        // Reduce tombstone_block value if necessary
        self.tombstone_block = min(self.fog_tombstone_block_limit, self.tombstone_block);
    }

    /// Sets the transaction fee.
    ///
    /// # Arguments
    /// * `fee_value` - Transaction fee value, in smallest representable units.
    pub fn set_fee(&mut self, fee_value: u64) -> Result<(), TxBuilderError> {
        // Set the fee in memo builder first, so that it can signal an error
        // before we set self.fee, and don't have to roll back.
        let mut new_fee = self.fee;
        new_fee.value = fee_value;
        self.memo_builder
            .as_mut()
            .expect("memo builder is missing, this is a logic error")
            .set_fee(new_fee)?;
        self.fee = new_fee;
        Ok(())
    }

    /// Gets the transaction fee.
    pub fn get_fee(&self) -> u64 {
        self.fee.value
    }

    /// Gets the fee token id
    pub fn get_fee_token_id(&self) -> TokenId {
        self.fee.token_id
    }

    /// Consume the builder and return the transaction.
    pub fn build<RNG: CryptoRng + RngCore>(self, rng: &mut RNG) -> Result<Tx, TxBuilderError> {
        self.build_with_comparer_internal::<RNG, DefaultTxOutputsOrdering>(rng)
    }

    /// Consume the builder and return the transaction with a comparer.
    /// Used only in testing library.
    #[cfg(feature = "test-only")]
    pub fn build_with_sorter<RNG: CryptoRng + RngCore, O: TxOutputsOrdering>(
        self,
        rng: &mut RNG,
    ) -> Result<Tx, TxBuilderError> {
        self.build_with_comparer_internal::<RNG, O>(rng)
    }

    /// Consume the builder and return the transaction with a comparer
    /// (internal usage only).
    fn build_with_comparer_internal<RNG: CryptoRng + RngCore, O: TxOutputsOrdering>(
        mut self,
        rng: &mut RNG,
    ) -> Result<Tx, TxBuilderError> {
        // Note: Origin block has block version zero, so some clients like slam that
        // start with a bootstrapped ledger will target block version 0. However,
        // block version zero has no special rules and so targeting block version 0
        // should be the same as targeting block version 1, for the transaction
        // builder. This test is mainly here in case we decide that the
        // transaction builder should stop supporting sufficiently old block
        // versions in the future, then we can replace the zero here with
        // something else.
        if self.block_version < BlockVersion::default() {
            return Err(TxBuilderError::BlockVersionTooOld(*self.block_version, 0));
        }

        if self.block_version > BlockVersion::MAX {
            return Err(TxBuilderError::BlockVersionTooNew(
                *self.block_version,
                *BlockVersion::MAX,
            ));
        }

        if !self.block_version.masked_token_id_feature_is_supported()
            && self.fee.token_id != Mob::ID
        {
            return Err(TxBuilderError::FeatureNotSupportedAtBlockVersion(
                *self.block_version,
                "nonzero token id",
            ));
        }

        if self.input_materials.is_empty() {
            return Err(TxBuilderError::NoInputs);
        }

        // All inputs must have rings of the same size.
        if self
            .input_materials
            .windows(2)
            .any(|win| win[0].ring_size() != win[1].ring_size())
        {
            return Err(TxBuilderError::InvalidRingSize);
        }

        for input in self.input_materials.iter() {
            if !self.block_version.mixed_transactions_are_supported()
                && input.amount().token_id != self.fee.token_id
            {
                return Err(TxBuilderError::MixedTransactionsNotAllowed(
                    self.fee.token_id,
                    input.amount().token_id,
                ));
            }

            match input {
                InputMaterials::Presigned(input) => {
                    if !self.block_version.signed_input_rules_are_supported() {
                        return Err(TxBuilderError::SignedInputRulesNotAllowed);
                    }
                    // TODO: Also validate membership proofs?
                    if input.tx_in.ring.len() != input.tx_in.proofs.len() {
                        return Err(TxBuilderError::MissingMembershipProofs);
                    }
                }
                InputMaterials::Signable(input) => {
                    // TODO: Also validate membership proofs?
                    if input.ring.len() != input.membership_proofs.len() {
                        return Err(TxBuilderError::MissingMembershipProofs);
                    }
                }
            }
        }

        // Construct a list of sorted inputs.
        // Inputs are sorted by the first ring element's public key. Note that each ring
        // is also sorted.
        self.input_materials
            .sort_by(|a, b| a.sort_key().cmp(b.sort_key()));

        let inputs: Vec<TxIn> = self.input_materials.iter().map(TxIn::from).collect();

        // Outputs are sorted according to the rule (but generally by public key)
        self.outputs_and_secrets
            .sort_by(|(a, _), (b, _)| O::cmp(&a.public_key, &b.public_key));

        let (outputs, output_secrets): (Vec<TxOut>, Vec<_>) =
            self.outputs_and_secrets.drain(..).unzip();

        let tx_prefix = TxPrefix::new(inputs, outputs, self.fee, self.tombstone_block);

        let input_rings = self
            .input_materials
            .into_iter()
            .map(Into::into)
            .collect::<Vec<InputRing>>();

        let message = tx_prefix.hash().0;
        let signature = SignatureRctBulletproofs::sign(
            self.block_version,
            &message,
            &input_rings,
            &output_secrets,
            self.fee,
            rng,
        )?;

        Ok(Tx {
            prefix: tx_prefix,
            signature,
        })
    }
}

/// Creates a TxOut that sends `value` to `recipient` using the provided
/// `fog_hint`.
///
/// # Arguments
/// * `block_version` - Block version rules to conform to
/// * `value` - Value of the output, in picoMOB.
/// * `recipient` - Recipient's address.
/// * `fog_hint` - The encrypted fog hint to use
/// * `memo_fn` - The memo function to use -- see TxOut::new_with_memo docu
/// * `rng` -
pub(crate) fn create_output_with_fog_hint<RNG: CryptoRng + RngCore>(
    block_version: BlockVersion,
    amount: Amount,
    recipient: &PublicAddress,
    fog_hint: EncryptedFogHint,
    memo_fn: impl FnOnce(MemoContext) -> Result<Option<MemoPayload>, NewMemoError>,
    rng: &mut RNG,
) -> Result<(TxOut, RistrettoPublic), TxBuilderError> {
    let private_key = RistrettoPrivate::from_random(rng);
    let mut tx_out = TxOut::new_with_memo(amount, recipient, &private_key, fog_hint, memo_fn)?;

    if !block_version.e_memo_feature_is_supported() {
        tx_out.e_memo = None;
    }
    if !block_version.masked_token_id_feature_is_supported() {
        tx_out.masked_amount.masked_token_id.clear();
    }

    let shared_secret = create_shared_secret(recipient.view_public_key(), &private_key);
    Ok((tx_out, shared_secret))
}

/// Create a fog hint, using the fog_resolver collection in self.
///
/// # Arguments
/// * `recipient` - Recipient's address.
/// * `fog_resolver` - Set of validated fog pubkey data
/// * `rng` - Entropy for the encryption.
///
/// # Returns
/// * `encrypted_fog_hint` - The fog hint to use for a TxOut.
/// * `pubkey_expiry` - The block at which this fog pubkey expires, or
///   u64::max_value() Imposes a limit on tombstone block for the transaction
pub(crate) fn create_fog_hint<RNG: RngCore + CryptoRng, FPR: FogPubkeyResolver>(
    recipient: &PublicAddress,
    fog_resolver: &FPR,
    rng: &mut RNG,
) -> Result<(EncryptedFogHint, u64), TxBuilderError> {
    if recipient.fog_report_url().is_none() {
        return Ok((EncryptedFogHint::fake_onetime_hint(rng), u64::max_value()));
    }

    // Find fog pubkey from set of pre-fetched fog pubkeys
    let validated_fog_pubkey = fog_resolver.get_fog_pubkey(recipient)?;

    Ok((
        FogHint::from(recipient).encrypt(&validated_fog_pubkey.pubkey, rng),
        validated_fog_pubkey.pubkey_expiry,
    ))
}

#[cfg(test)]
pub mod transaction_builder_tests {
    use super::*;
    use crate::{
        test_utils::{get_input_credentials, get_ring, get_transaction},
        BurnRedemptionMemoBuilder, EmptyMemoBuilder, MemoType, RTHMemoBuilder,
        SenderMemoCredential,
    };
    use assert_matches::assert_matches;
    use maplit::btreemap;
    use mc_account_keys::{
        burn_address, burn_address_view_private, AccountKey, ShortAddressHash,
        CHANGE_SUBADDRESS_INDEX, DEFAULT_SUBADDRESS_INDEX,
    };
    use mc_fog_report_validation_test_utils::{FullyValidatedFogPubkey, MockFogResolver};
    use mc_transaction_core::{
        constants::{MAX_INPUTS, MAX_OUTPUTS, MILLIMOB_TO_PICOMOB},
        get_tx_out_shared_secret,
        onetime_keys::*,
        ring_signature::KeyImage,
        subaddress_matches_tx_out,
        tx::TxOutMembershipProof,
        validation::{validate_signature, validate_tx_out},
        NewTxError, TokenId,
    };
    use rand::{rngs::StdRng, SeedableRng};
    use std::convert::TryFrom;

    // Helper which produces a list of block_version, TokenId pairs to iterate over
    // in tests
    fn get_block_version_token_id_pairs() -> Vec<(BlockVersion, TokenId)> {
        vec![
            (BlockVersion::try_from(0).unwrap(), TokenId::from(0)),
            (BlockVersion::try_from(1).unwrap(), TokenId::from(0)),
            (BlockVersion::try_from(2).unwrap(), TokenId::from(0)),
            (BlockVersion::try_from(2).unwrap(), TokenId::from(1)),
            (BlockVersion::try_from(2).unwrap(), TokenId::from(2)),
        ]
    }

    #[test]
    // Spend a single input and send its full value to a single recipient.
    fn test_simple_transaction() {
        let mut rng: StdRng = SeedableRng::from_seed([1u8; 32]);

        for (block_version, token_id) in get_block_version_token_id_pairs() {
            let fpr = MockFogResolver::default();
            let sender = AccountKey::random(&mut rng);
            let recipient = AccountKey::random(&mut rng);
            let value = 1475 * MILLIMOB_TO_PICOMOB;
            let amount = Amount { value, token_id };

            // Mint an initial collection of outputs, including one belonging to Alice.
            let input_credentials =
                get_input_credentials(block_version, amount, &sender, &fpr, &mut rng);

            let membership_proofs = input_credentials.membership_proofs.clone();
            let key_image = KeyImage::from(&input_credentials.input_secret.onetime_private_key);

            let mut transaction_builder = TransactionBuilder::new(
                block_version,
                Amount::new(Mob::MINIMUM_FEE, token_id),
                fpr,
                EmptyMemoBuilder::default(),
            )
            .unwrap();

            transaction_builder.add_input(input_credentials);
            let (_txout, confirmation) = transaction_builder
                .add_output(
                    Amount::new(value - Mob::MINIMUM_FEE, token_id),
                    &recipient.default_subaddress(),
                    &mut rng,
                )
                .unwrap();

            let tx = transaction_builder.build(&mut rng).unwrap();

            // The transaction should have a single input.
            assert_eq!(tx.prefix.inputs.len(), 1);

            assert_eq!(tx.prefix.inputs[0].proofs.len(), membership_proofs.len());

            let expected_key_images = vec![key_image];
            assert_eq!(tx.key_images(), expected_key_images);

            // The transaction should have one output.
            assert_eq!(tx.prefix.outputs.len(), 1);

            let output: &TxOut = tx.prefix.outputs.get(0).unwrap();

            validate_tx_out(block_version, output).unwrap();

            // The output should belong to the correct recipient.
            assert!(
                subaddress_matches_tx_out(&recipient, DEFAULT_SUBADDRESS_INDEX, output).unwrap()
            );

            // The output should have the correct value and confirmation number
            {
                let public_key = RistrettoPublic::try_from(&output.public_key).unwrap();
                assert!(confirmation.validate(&public_key, recipient.view_private_key()));
            }

            // The transaction should have a valid signature.
            assert!(validate_signature(block_version, &tx, &mut rng).is_ok());
        }
    }

    #[test]
    // Spend a single input and send its full value to a single fog recipient.
    fn test_simple_fog_transaction() {
        let mut rng: StdRng = SeedableRng::from_seed([1u8; 32]);

        for (block_version, token_id) in get_block_version_token_id_pairs() {
            let sender = AccountKey::random(&mut rng);
            let recipient = AccountKey::random_with_fog(&mut rng);
            let ingest_private_key = RistrettoPrivate::from_random(&mut rng);

            let fog_resolver = MockFogResolver(btreemap! {
                                recipient
                        .default_subaddress()
                        .fog_report_url()
                        .unwrap()
                        .to_string()
                =>
                    FullyValidatedFogPubkey {
                        pubkey: RistrettoPublic::from(&ingest_private_key),
                        pubkey_expiry: 1000,
                    },
            });

            let value = 1475 * MILLIMOB_TO_PICOMOB;
            let amount = Amount { value, token_id };

            let input_credentials =
                get_input_credentials(block_version, amount, &sender, &fog_resolver, &mut rng);

            let membership_proofs = input_credentials.membership_proofs.clone();
            let key_image = KeyImage::from(&input_credentials.input_secret.onetime_private_key);

            let mut transaction_builder = TransactionBuilder::new(
                block_version,
                Amount::new(Mob::MINIMUM_FEE, token_id),
                fog_resolver,
                EmptyMemoBuilder::default(),
            )
            .unwrap();

            transaction_builder.add_input(input_credentials);
            let (_txout, confirmation) = transaction_builder
                .add_output(
                    Amount::new(value - Mob::MINIMUM_FEE, token_id),
                    &recipient.default_subaddress(),
                    &mut rng,
                )
                .unwrap();

            let tx = transaction_builder.build(&mut rng).unwrap();

            // The transaction should have a single input.
            assert_eq!(tx.prefix.inputs.len(), 1);

            assert_eq!(tx.prefix.inputs[0].proofs.len(), membership_proofs.len());

            let expected_key_images = vec![key_image];
            assert_eq!(tx.key_images(), expected_key_images);

            // The transaction should have one output.
            assert_eq!(tx.prefix.outputs.len(), 1);

            let output: &TxOut = tx.prefix.outputs.get(0).unwrap();

            validate_tx_out(block_version, output).unwrap();

            // The output should belong to the correct recipient.
            assert!(
                subaddress_matches_tx_out(&recipient, DEFAULT_SUBADDRESS_INDEX, output).unwrap()
            );

            // The output should have the correct value and confirmation number
            {
                let public_key = RistrettoPublic::try_from(&output.public_key).unwrap();
                assert!(confirmation.validate(&public_key, recipient.view_private_key()));
            }

            // The output's fog hint should contain the correct public key.
            {
                let mut output_fog_hint = FogHint::new(RistrettoPublic::from_random(&mut rng));
                assert!(bool::from(FogHint::ct_decrypt(
                    &ingest_private_key,
                    &output.e_fog_hint,
                    &mut output_fog_hint,
                )));
                assert_eq!(
                    output_fog_hint.get_view_pubkey(),
                    &CompressedRistrettoPublic::from(
                        recipient.default_subaddress().view_public_key()
                    )
                );
            }

            // The transaction should have a valid signature.
            assert!(validate_signature(block_version, &tx, &mut rng).is_ok());
        }
    }

    #[test]
    // Use a custom PublicAddress to create the fog hint.
    fn test_custom_fog_hint_address() {
        let mut rng: StdRng = SeedableRng::from_seed([1u8; 32]);

        for (block_version, token_id) in get_block_version_token_id_pairs() {
            let sender = AccountKey::random(&mut rng);
            let recipient = AccountKey::random(&mut rng);
            let fog_hint_address = AccountKey::random_with_fog(&mut rng).default_subaddress();
            let ingest_private_key = RistrettoPrivate::from_random(&mut rng);
            let value = 1475 * MILLIMOB_TO_PICOMOB;
            let amount = Amount { value, token_id };

            let fog_resolver = MockFogResolver(btreemap! {
                                fog_hint_address
                        .fog_report_url()
                        .unwrap()
                        .to_string()
                =>
                    FullyValidatedFogPubkey {
                        pubkey: RistrettoPublic::from(&ingest_private_key),
                        pubkey_expiry: 1000,
                    },
            });

            let mut transaction_builder = TransactionBuilder::new(
                block_version,
                Amount::new(Mob::MINIMUM_FEE, token_id),
                fog_resolver.clone(),
                EmptyMemoBuilder::default(),
            )
            .unwrap();

            let input_credentials =
                get_input_credentials(block_version, amount, &sender, &fog_resolver, &mut rng);
            transaction_builder.add_input(input_credentials);

            let (_txout, _confirmation) = transaction_builder
                .add_output_with_fog_hint_address(
                    Amount::new(value - Mob::MINIMUM_FEE, token_id),
                    &recipient.default_subaddress(),
                    &fog_hint_address,
                    |_| Ok(Default::default()),
                    &mut rng,
                )
                .unwrap();

            let tx = transaction_builder.build(&mut rng).unwrap();

            // The transaction should have one output.
            assert_eq!(tx.prefix.outputs.len(), 1);

            let output: &TxOut = tx.prefix.outputs.get(0).unwrap();

            // The output should belong to the correct recipient.
            assert!(
                subaddress_matches_tx_out(&recipient, DEFAULT_SUBADDRESS_INDEX, output).unwrap()
            );

            // The output's fog hint should contain the correct public key.
            {
                let mut output_fog_hint = FogHint::new(RistrettoPublic::from_random(&mut rng));
                assert!(bool::from(FogHint::ct_decrypt(
                    &ingest_private_key,
                    &output.e_fog_hint,
                    &mut output_fog_hint,
                )));
                assert_eq!(
                    output_fog_hint.get_view_pubkey(),
                    &CompressedRistrettoPublic::from(fog_hint_address.view_public_key())
                );
            }
        }
    }

    #[test]
    // Test that fog pubkey expiry limit is enforced on the tombstone block
    fn test_fog_pubkey_expiry_limit_enforced() {
        let mut rng: StdRng = SeedableRng::from_seed([1u8; 32]);

        for (block_version, token_id) in get_block_version_token_id_pairs() {
            let sender = AccountKey::random(&mut rng);
            let recipient = AccountKey::random_with_fog(&mut rng);
            let recipient_address = recipient.default_subaddress();
            let ingest_private_key = RistrettoPrivate::from_random(&mut rng);
            let value = 1475 * MILLIMOB_TO_PICOMOB;
            let amount = Amount { value, token_id };

            let fog_resolver = MockFogResolver(btreemap! {
                                recipient_address
                        .fog_report_url()
                        .unwrap()
                        .to_string()
                =>
                    FullyValidatedFogPubkey {
                        pubkey: RistrettoPublic::from(&ingest_private_key),
                        pubkey_expiry: 1000,
                    },
            });

            {
                let mut transaction_builder = TransactionBuilder::new(
                    block_version,
                    Amount::new(Mob::MINIMUM_FEE, token_id),
                    fog_resolver.clone(),
                    EmptyMemoBuilder::default(),
                )
                .unwrap();

                transaction_builder.set_tombstone_block(2000);

                let input_credentials =
                    get_input_credentials(block_version, amount, &sender, &fog_resolver, &mut rng);
                transaction_builder.add_input(input_credentials);

                let (_txout, _confirmation) = transaction_builder
                    .add_output(
                        Amount::new(value - Mob::MINIMUM_FEE, token_id),
                        &recipient_address,
                        &mut rng,
                    )
                    .unwrap();

                let tx = transaction_builder.build(&mut rng).unwrap();

                // The transaction should have one output.
                assert_eq!(tx.prefix.outputs.len(), 1);

                validate_tx_out(block_version, tx.prefix.outputs.first().unwrap()).unwrap();

                // The tombstone block should be the min of what the user requested, and what
                // fog limits it to
                assert_eq!(tx.prefix.tombstone_block, 1000);
            }

            {
                let mut transaction_builder = TransactionBuilder::new(
                    block_version,
                    Amount::new(Mob::MINIMUM_FEE, token_id),
                    fog_resolver.clone(),
                    EmptyMemoBuilder::default(),
                )
                .unwrap();

                transaction_builder.set_tombstone_block(500);

                let input_credentials =
                    get_input_credentials(block_version, amount, &sender, &fog_resolver, &mut rng);
                transaction_builder.add_input(input_credentials);

                let (_txout, _confirmation) = transaction_builder
                    .add_output(
                        Amount::new(value - Mob::MINIMUM_FEE, token_id),
                        &recipient_address,
                        &mut rng,
                    )
                    .unwrap();

                let tx = transaction_builder.build(&mut rng).unwrap();

                // The transaction should have one output.
                assert_eq!(tx.prefix.outputs.len(), 1);

                validate_tx_out(block_version, tx.prefix.outputs.first().unwrap()).unwrap();

                // The tombstone block should be the min of what the user requested, and what
                // fog limits it to
                assert_eq!(tx.prefix.tombstone_block, 500);
            }
        }
    }

    #[test]
    // Test that sending a fog transaction with change, and recoverable transaction
    // history, produces appropriate memos
    fn test_fog_transaction_with_change() {
        let mut rng: StdRng = SeedableRng::from_seed([1u8; 32]);

        for (block_version, token_id) in get_block_version_token_id_pairs() {
            let sender = AccountKey::random_with_fog(&mut rng);
            let sender_change_dest = ReservedDestination::from(&sender);
            let recipient = AccountKey::random_with_fog(&mut rng);
            let recipient_address = recipient.default_subaddress();
            let ingest_private_key = RistrettoPrivate::from_random(&mut rng);
            let value = 1475 * MILLIMOB_TO_PICOMOB;
            let change_value = 128 * MILLIMOB_TO_PICOMOB;

            let fog_resolver = MockFogResolver(btreemap! {
                                recipient_address
                        .fog_report_url()
                        .unwrap()
                        .to_string()
                =>
                    FullyValidatedFogPubkey {
                        pubkey: RistrettoPublic::from(&ingest_private_key),
                        pubkey_expiry: 1000,
                    },
            });

            {
                let mut transaction_builder = TransactionBuilder::new(
                    block_version,
                    Amount::new(Mob::MINIMUM_FEE, token_id),
                    fog_resolver.clone(),
                    EmptyMemoBuilder::default(),
                )
                .unwrap();

                transaction_builder.set_tombstone_block(2000);

                let input_credentials = get_input_credentials(
                    block_version,
                    Amount { value, token_id },
                    &sender,
                    &fog_resolver,
                    &mut rng,
                );
                transaction_builder.add_input(input_credentials);

                let (_txout, _confirmation) = transaction_builder
                    .add_output(
                        Amount::new(value - change_value - Mob::MINIMUM_FEE, token_id),
                        &recipient_address,
                        &mut rng,
                    )
                    .unwrap();

                transaction_builder
                    .add_change_output(
                        Amount::new(change_value, token_id),
                        &sender_change_dest,
                        &mut rng,
                    )
                    .unwrap();

                let tx = transaction_builder.build(&mut rng).unwrap();

                // The transaction should have two output.
                assert_eq!(tx.prefix.outputs.len(), 2);

                // The tombstone block should be the min of what the user requested, and what
                // fog limits it to
                assert_eq!(tx.prefix.tombstone_block, 1000);

                let output = tx
                    .prefix
                    .outputs
                    .iter()
                    .find(|tx_out| {
                        subaddress_matches_tx_out(&recipient, DEFAULT_SUBADDRESS_INDEX, tx_out)
                            .unwrap()
                    })
                    .expect("Didn't find recipient's output");
                let change = tx
                    .prefix
                    .outputs
                    .iter()
                    .find(|tx_out| {
                        subaddress_matches_tx_out(&sender, CHANGE_SUBADDRESS_INDEX, tx_out).unwrap()
                    })
                    .expect("Didn't find sender's output");

                validate_tx_out(block_version, output).unwrap();
                validate_tx_out(block_version, change).unwrap();

                assert!(
                    !subaddress_matches_tx_out(&recipient, DEFAULT_SUBADDRESS_INDEX, change)
                        .unwrap()
                );
                assert!(
                    !subaddress_matches_tx_out(&sender, DEFAULT_SUBADDRESS_INDEX, change).unwrap()
                );
                assert!(
                    !subaddress_matches_tx_out(&sender, CHANGE_SUBADDRESS_INDEX, output).unwrap()
                );
                assert!(
                    !subaddress_matches_tx_out(&recipient, CHANGE_SUBADDRESS_INDEX, output)
                        .unwrap()
                );

                // The 1st output should belong to the correct recipient and have correct amount
                // and have an empty memo
                {
                    let ss = get_tx_out_shared_secret(
                        recipient.view_private_key(),
                        &RistrettoPublic::try_from(&output.public_key).unwrap(),
                    );
                    let (amount, _) = output.masked_amount.get_value(&ss).unwrap();
                    assert_eq!(amount.value, value - change_value - Mob::MINIMUM_FEE);
                    assert_eq!(amount.token_id, token_id);

                    if block_version.e_memo_feature_is_supported() {
                        let memo = output.e_memo.unwrap().decrypt(&ss);
                        assert_eq!(memo, MemoPayload::default());
                    }
                }

                // The 1st output's fog hint should contain the correct public key.
                {
                    let mut output_fog_hint = FogHint::new(RistrettoPublic::from_random(&mut rng));
                    assert!(bool::from(FogHint::ct_decrypt(
                        &ingest_private_key,
                        &output.e_fog_hint,
                        &mut output_fog_hint,
                    )));
                    assert_eq!(
                        output_fog_hint.get_view_pubkey(),
                        &CompressedRistrettoPublic::from(recipient_address.view_public_key())
                    );
                }

                // The 2nd output should belong to the correct recipient and have correct amount
                // and have empty memo
                {
                    let ss = get_tx_out_shared_secret(
                        sender.view_private_key(),
                        &RistrettoPublic::try_from(&change.public_key).unwrap(),
                    );
                    let (amount, _) = change.masked_amount.get_value(&ss).unwrap();
                    assert_eq!(amount.value, change_value);
                    assert_eq!(amount.token_id, token_id);

                    if block_version.e_memo_feature_is_supported() {
                        let memo = change.e_memo.unwrap().decrypt(&ss);
                        assert_eq!(memo, MemoPayload::default());
                    }
                }

                // The 2nd output's fog hint should contain the correct public key.
                {
                    let mut output_fog_hint = FogHint::new(RistrettoPublic::from_random(&mut rng));
                    assert!(bool::from(FogHint::ct_decrypt(
                        &ingest_private_key,
                        &change.e_fog_hint,
                        &mut output_fog_hint,
                    )));
                    assert_eq!(
                        output_fog_hint.get_view_pubkey(),
                        &CompressedRistrettoPublic::from(
                            sender.default_subaddress().view_public_key()
                        )
                    );
                }
            }
        }
    }

    #[test]
    // Test that sending a fog transaction with change, using add_change_output
    // produces change owned by the sender as expected, with appropriate memos
    fn test_fog_transaction_with_change_and_rth_memos() {
        let mut rng: StdRng = SeedableRng::from_seed([1u8; 32]);

        for (block_version, token_id) in get_block_version_token_id_pairs() {
            let sender = AccountKey::random_with_fog(&mut rng);
            let sender_addr = sender.default_subaddress();
            let sender_change_dest = ReservedDestination::from(&sender);
            let recipient = AccountKey::random_with_fog(&mut rng);
            let recipient_address = recipient.default_subaddress();
            let ingest_private_key = RistrettoPrivate::from_random(&mut rng);
            let value = 1475 * MILLIMOB_TO_PICOMOB;
            let change_value = 128 * MILLIMOB_TO_PICOMOB;

            let fog_resolver = MockFogResolver(btreemap! {
                                recipient_address
                        .fog_report_url()
                        .unwrap()
                        .to_string()
                =>
                    FullyValidatedFogPubkey {
                        pubkey: RistrettoPublic::from(&ingest_private_key),
                        pubkey_expiry: 1000,
                    },
            });

            // Enable both sender and destination memos
            {
                let mut memo_builder = RTHMemoBuilder::default();
                memo_builder.set_sender_credential(SenderMemoCredential::from(&sender));
                memo_builder.enable_destination_memo();

                let mut transaction_builder = TransactionBuilder::new(
                    block_version,
                    Amount::new(Mob::MINIMUM_FEE, token_id),
                    fog_resolver.clone(),
                    memo_builder,
                )
                .unwrap();

                transaction_builder.set_tombstone_block(2000);

                let input_credentials = get_input_credentials(
                    block_version,
                    Amount { value, token_id },
                    &sender,
                    &fog_resolver,
                    &mut rng,
                );
                transaction_builder.add_input(input_credentials);

                let (_txout, _confirmation) = transaction_builder
                    .add_output(
                        Amount::new(value - change_value - Mob::MINIMUM_FEE, token_id),
                        &recipient_address,
                        &mut rng,
                    )
                    .unwrap();

                transaction_builder
                    .add_change_output(
                        Amount::new(change_value, token_id),
                        &sender_change_dest,
                        &mut rng,
                    )
                    .unwrap();

                let tx = transaction_builder.build(&mut rng).unwrap();

                // The transaction should have two output.
                assert_eq!(tx.prefix.outputs.len(), 2);

                // The tombstone block should be the min of what the user requested, and what
                // fog limits it to
                assert_eq!(tx.prefix.tombstone_block, 1000);

                let output = tx
                    .prefix
                    .outputs
                    .iter()
                    .find(|tx_out| {
                        subaddress_matches_tx_out(&recipient, DEFAULT_SUBADDRESS_INDEX, tx_out)
                            .unwrap()
                    })
                    .expect("Didn't find recipient's output");
                let change = tx
                    .prefix
                    .outputs
                    .iter()
                    .find(|tx_out| {
                        subaddress_matches_tx_out(&sender, CHANGE_SUBADDRESS_INDEX, tx_out).unwrap()
                    })
                    .expect("Didn't find sender's output");

                validate_tx_out(block_version, output).unwrap();
                validate_tx_out(block_version, change).unwrap();

                assert!(
                    !subaddress_matches_tx_out(&recipient, DEFAULT_SUBADDRESS_INDEX, change)
                        .unwrap()
                );
                assert!(
                    !subaddress_matches_tx_out(&sender, DEFAULT_SUBADDRESS_INDEX, change).unwrap()
                );
                assert!(
                    !subaddress_matches_tx_out(&sender, CHANGE_SUBADDRESS_INDEX, output).unwrap()
                );
                assert!(
                    !subaddress_matches_tx_out(&recipient, CHANGE_SUBADDRESS_INDEX, output)
                        .unwrap()
                );

                // The 1st output should belong to the correct recipient and have correct amount
                // and have correct memo
                {
                    let ss = get_tx_out_shared_secret(
                        recipient.view_private_key(),
                        &RistrettoPublic::try_from(&output.public_key).unwrap(),
                    );
                    let (amount, _) = output.masked_amount.get_value(&ss).unwrap();
                    assert_eq!(amount.value, value - change_value - Mob::MINIMUM_FEE);
                    assert_eq!(amount.token_id, token_id);

                    if block_version.e_memo_feature_is_supported() {
                        let memo = output.e_memo.unwrap().decrypt(&ss);
                        match MemoType::try_from(&memo).expect("Couldn't decrypt memo") {
                            MemoType::AuthenticatedSender(memo) => {
                                assert_eq!(
                                    memo.sender_address_hash(),
                                    ShortAddressHash::from(&sender_addr),
                                    "lookup based on address hash failed"
                                );
                                assert!(
                                    bool::from(
                                        memo.validate(
                                            &sender_addr,
                                            &recipient
                                                .subaddress_view_private(DEFAULT_SUBADDRESS_INDEX),
                                            &output.public_key,
                                        )
                                    ),
                                    "hmac validation failed"
                                );
                            }
                            _ => {
                                panic!("unexpected memo type")
                            }
                        }
                    }
                }

                // The 2nd output should belong to the correct recipient and have correct amount
                // and have correct memo
                {
                    let ss = get_tx_out_shared_secret(
                        sender.view_private_key(),
                        &RistrettoPublic::try_from(&change.public_key).unwrap(),
                    );
                    let (amount, _) = change.masked_amount.get_value(&ss).unwrap();
                    assert_eq!(amount.value, change_value);
                    assert_eq!(amount.token_id, token_id);

                    if block_version.e_memo_feature_is_supported() {
                        let memo = change.e_memo.unwrap().decrypt(&ss);
                        match MemoType::try_from(&memo).expect("Couldn't decrypt memo") {
                            MemoType::Destination(memo) => {
                                assert_eq!(
                                    memo.get_address_hash(),
                                    &ShortAddressHash::from(&recipient_address),
                                    "lookup based on address hash failed"
                                );
                                assert_eq!(memo.get_num_recipients(), 1);
                                assert_eq!(memo.get_fee(), Mob::MINIMUM_FEE);
                                assert_eq!(
                                    memo.get_total_outlay(),
                                    value - change_value,
                                    "outlay should be amount sent to recipient + fee"
                                );
                            }
                            _ => {
                                panic!("unexpected memo type")
                            }
                        }
                    }
                }
            }

            // Enable both sender and destination memos, and try increasing the fee
            {
                let mut memo_builder = RTHMemoBuilder::default();
                memo_builder.set_sender_credential(SenderMemoCredential::from(&sender));
                memo_builder.enable_destination_memo();

                let mut transaction_builder = TransactionBuilder::new(
                    block_version,
                    Amount::new(Mob::MINIMUM_FEE, token_id),
                    fog_resolver.clone(),
                    memo_builder,
                )
                .unwrap();

                transaction_builder.set_tombstone_block(2000);
                transaction_builder.set_fee(Mob::MINIMUM_FEE * 4).unwrap();

                let input_credentials = get_input_credentials(
                    block_version,
                    Amount { value, token_id },
                    &sender,
                    &fog_resolver,
                    &mut rng,
                );
                transaction_builder.add_input(input_credentials);

                let (_txout, _confirmation) = transaction_builder
                    .add_output(
                        Amount::new(value - change_value - 4 * Mob::MINIMUM_FEE, token_id),
                        &recipient_address,
                        &mut rng,
                    )
                    .unwrap();

                transaction_builder
                    .add_change_output(
                        Amount::new(change_value, token_id),
                        &sender_change_dest,
                        &mut rng,
                    )
                    .unwrap();

                let tx = transaction_builder.build(&mut rng).unwrap();

                // The transaction should have two output.
                assert_eq!(tx.prefix.outputs.len(), 2);

                // The tombstone block should be the min of what the user requested, and what
                // fog limits it to
                assert_eq!(tx.prefix.tombstone_block, 1000);

                let output = tx
                    .prefix
                    .outputs
                    .iter()
                    .find(|tx_out| {
                        subaddress_matches_tx_out(&recipient, DEFAULT_SUBADDRESS_INDEX, tx_out)
                            .unwrap()
                    })
                    .expect("Didn't find recipient's output");
                let change = tx
                    .prefix
                    .outputs
                    .iter()
                    .find(|tx_out| {
                        subaddress_matches_tx_out(&sender, CHANGE_SUBADDRESS_INDEX, tx_out).unwrap()
                    })
                    .expect("Didn't find sender's output");

                validate_tx_out(block_version, output).unwrap();
                validate_tx_out(block_version, change).unwrap();

                assert!(
                    !subaddress_matches_tx_out(&recipient, DEFAULT_SUBADDRESS_INDEX, change)
                        .unwrap()
                );
                assert!(
                    !subaddress_matches_tx_out(&sender, DEFAULT_SUBADDRESS_INDEX, change).unwrap()
                );
                assert!(
                    !subaddress_matches_tx_out(&sender, CHANGE_SUBADDRESS_INDEX, output).unwrap()
                );
                assert!(
                    !subaddress_matches_tx_out(&recipient, CHANGE_SUBADDRESS_INDEX, output)
                        .unwrap()
                );

                // The 1st output should belong to the correct recipient and have correct amount
                // and have correct memo
                {
                    let ss = get_tx_out_shared_secret(
                        recipient.view_private_key(),
                        &RistrettoPublic::try_from(&output.public_key).unwrap(),
                    );
                    let (amount, _) = output.masked_amount.get_value(&ss).unwrap();
                    assert_eq!(amount.value, value - change_value - Mob::MINIMUM_FEE * 4);
                    assert_eq!(amount.token_id, token_id);

                    if block_version.e_memo_feature_is_supported() {
                        let memo = output.e_memo.unwrap().decrypt(&ss);
                        match MemoType::try_from(&memo).expect("Couldn't decrypt memo") {
                            MemoType::AuthenticatedSender(memo) => {
                                assert_eq!(
                                    memo.sender_address_hash(),
                                    ShortAddressHash::from(&sender_addr),
                                    "lookup based on address hash failed"
                                );
                                assert!(
                                    bool::from(
                                        memo.validate(
                                            &sender_addr,
                                            &recipient
                                                .subaddress_view_private(DEFAULT_SUBADDRESS_INDEX),
                                            &output.public_key,
                                        )
                                    ),
                                    "hmac validation failed"
                                );
                            }
                            _ => {
                                panic!("unexpected memo type")
                            }
                        }
                    }
                }

                // The 2nd output should belong to the correct recipient and have correct amount
                // and have correct memo
                {
                    let ss = get_tx_out_shared_secret(
                        sender.view_private_key(),
                        &RistrettoPublic::try_from(&change.public_key).unwrap(),
                    );
                    let (amount, _) = change.masked_amount.get_value(&ss).unwrap();
                    assert_eq!(amount.value, change_value);
                    assert_eq!(amount.token_id, token_id);

                    if block_version.e_memo_feature_is_supported() {
                        let memo = change.e_memo.unwrap().decrypt(&ss);
                        match MemoType::try_from(&memo).expect("Couldn't decrypt memo") {
                            MemoType::Destination(memo) => {
                                assert_eq!(
                                    memo.get_address_hash(),
                                    &ShortAddressHash::from(&recipient_address),
                                    "lookup based on address hash failed"
                                );
                                assert_eq!(memo.get_num_recipients(), 1);
                                assert_eq!(memo.get_fee(), Mob::MINIMUM_FEE * 4);
                                assert_eq!(
                                    memo.get_total_outlay(),
                                    value - change_value,
                                    "outlay should be amount sent to recipient + fee"
                                );
                            }
                            _ => {
                                panic!("unexpected memo type")
                            }
                        }
                    }
                }
            }

            // Enable both sender and destination memos, and set a payment request id
            {
                let mut memo_builder = RTHMemoBuilder::default();
                memo_builder.set_sender_credential(SenderMemoCredential::from(&sender));
                memo_builder.enable_destination_memo();
                memo_builder.set_payment_request_id(42);

                let mut transaction_builder = TransactionBuilder::new(
                    block_version,
                    Amount::new(Mob::MINIMUM_FEE, token_id),
                    fog_resolver.clone(),
                    memo_builder,
                )
                .unwrap();

                transaction_builder.set_tombstone_block(2000);

                let input_credentials = get_input_credentials(
                    block_version,
                    Amount { value, token_id },
                    &sender,
                    &fog_resolver,
                    &mut rng,
                );
                transaction_builder.add_input(input_credentials);

                let (_txout, _confirmation) = transaction_builder
                    .add_output(
                        Amount::new(value - change_value - Mob::MINIMUM_FEE, token_id),
                        &recipient_address,
                        &mut rng,
                    )
                    .unwrap();

                transaction_builder
                    .add_change_output(
                        Amount::new(change_value, token_id),
                        &sender_change_dest,
                        &mut rng,
                    )
                    .unwrap();

                let tx = transaction_builder.build(&mut rng).unwrap();

                // The transaction should have two output.
                assert_eq!(tx.prefix.outputs.len(), 2);

                // The tombstone block should be the min of what the user requested, and what
                // fog limits it to
                assert_eq!(tx.prefix.tombstone_block, 1000);

                let output = tx
                    .prefix
                    .outputs
                    .iter()
                    .find(|tx_out| {
                        subaddress_matches_tx_out(&recipient, DEFAULT_SUBADDRESS_INDEX, tx_out)
                            .unwrap()
                    })
                    .expect("Didn't find recipient's output");
                let change = tx
                    .prefix
                    .outputs
                    .iter()
                    .find(|tx_out| {
                        subaddress_matches_tx_out(&sender, CHANGE_SUBADDRESS_INDEX, tx_out).unwrap()
                    })
                    .expect("Didn't find sender's output");

                validate_tx_out(block_version, output).unwrap();
                validate_tx_out(block_version, change).unwrap();

                assert!(
                    !subaddress_matches_tx_out(&recipient, DEFAULT_SUBADDRESS_INDEX, change)
                        .unwrap()
                );
                assert!(
                    !subaddress_matches_tx_out(&sender, DEFAULT_SUBADDRESS_INDEX, change).unwrap()
                );
                assert!(
                    !subaddress_matches_tx_out(&sender, CHANGE_SUBADDRESS_INDEX, output).unwrap()
                );
                assert!(
                    !subaddress_matches_tx_out(&recipient, CHANGE_SUBADDRESS_INDEX, output)
                        .unwrap()
                );

                // The 1st output should belong to the correct recipient and have correct amount
                // and have correct memo
                {
                    let ss = get_tx_out_shared_secret(
                        recipient.view_private_key(),
                        &RistrettoPublic::try_from(&output.public_key).unwrap(),
                    );
                    let (amount, _) = output.masked_amount.get_value(&ss).unwrap();
                    assert_eq!(amount.value, value - change_value - Mob::MINIMUM_FEE);
                    assert_eq!(amount.token_id, token_id);

                    if block_version.e_memo_feature_is_supported() {
                        let memo = output.e_memo.unwrap().decrypt(&ss);
                        match MemoType::try_from(&memo).expect("Couldn't decrypt memo") {
                            MemoType::AuthenticatedSenderWithPaymentRequestId(memo) => {
                                assert_eq!(
                                    memo.sender_address_hash(),
                                    ShortAddressHash::from(&sender_addr),
                                    "lookup based on address hash failed"
                                );
                                assert!(
                                    bool::from(
                                        memo.validate(
                                            &sender_addr,
                                            &recipient
                                                .subaddress_view_private(DEFAULT_SUBADDRESS_INDEX),
                                            &output.public_key,
                                        )
                                    ),
                                    "hmac validation failed"
                                );
                                assert_eq!(memo.payment_request_id(), 42);
                            }
                            _ => {
                                panic!("unexpected memo type")
                            }
                        }
                    }
                }

                // The 2nd output should belong to the correct recipient and have correct amount
                // and have correct memo
                {
                    let ss = get_tx_out_shared_secret(
                        sender.view_private_key(),
                        &RistrettoPublic::try_from(&change.public_key).unwrap(),
                    );
                    let (amount, _) = change.masked_amount.get_value(&ss).unwrap();
                    assert_eq!(amount.value, change_value);
                    assert_eq!(amount.token_id, token_id);

                    if block_version.e_memo_feature_is_supported() {
                        let memo = change.e_memo.unwrap().decrypt(&ss);
                        match MemoType::try_from(&memo).expect("Couldn't decrypt memo") {
                            MemoType::Destination(memo) => {
                                assert_eq!(
                                    memo.get_address_hash(),
                                    &ShortAddressHash::from(&recipient_address),
                                    "lookup based on address hash failed"
                                );
                                assert_eq!(memo.get_num_recipients(), 1);
                                assert_eq!(memo.get_fee(), Mob::MINIMUM_FEE);
                                assert_eq!(
                                    memo.get_total_outlay(),
                                    value - change_value,
                                    "outlay should be amount sent to recipient + fee"
                                );
                            }
                            _ => {
                                panic!("unexpected memo type")
                            }
                        }
                    }
                }
            }

            // Enable sender memos, and set a payment request id, no destination_memo
            {
                let mut memo_builder = RTHMemoBuilder::default();
                memo_builder.set_sender_credential(SenderMemoCredential::from(&sender));
                memo_builder.set_payment_request_id(47);

                let mut transaction_builder = TransactionBuilder::new(
                    block_version,
                    Amount::new(Mob::MINIMUM_FEE, token_id),
                    fog_resolver.clone(),
                    memo_builder,
                )
                .unwrap();

                transaction_builder.set_tombstone_block(2000);

                let input_credentials = get_input_credentials(
                    block_version,
                    Amount { value, token_id },
                    &sender,
                    &fog_resolver,
                    &mut rng,
                );
                transaction_builder.add_input(input_credentials);

                let (_txout, _confirmation) = transaction_builder
                    .add_output(
                        Amount::new(value - change_value - Mob::MINIMUM_FEE, token_id),
                        &recipient_address,
                        &mut rng,
                    )
                    .unwrap();

                transaction_builder
                    .add_change_output(
                        Amount::new(change_value, token_id),
                        &sender_change_dest,
                        &mut rng,
                    )
                    .unwrap();

                let tx = transaction_builder.build(&mut rng).unwrap();

                // The transaction should have two output.
                assert_eq!(tx.prefix.outputs.len(), 2);

                // The tombstone block should be the min of what the user requested, and what
                // fog limits it to
                assert_eq!(tx.prefix.tombstone_block, 1000);

                let output = tx
                    .prefix
                    .outputs
                    .iter()
                    .find(|tx_out| {
                        subaddress_matches_tx_out(&recipient, DEFAULT_SUBADDRESS_INDEX, tx_out)
                            .unwrap()
                    })
                    .expect("Didn't find recipient's output");
                let change = tx
                    .prefix
                    .outputs
                    .iter()
                    .find(|tx_out| {
                        subaddress_matches_tx_out(&sender, CHANGE_SUBADDRESS_INDEX, tx_out).unwrap()
                    })
                    .expect("Didn't find sender's output");

                validate_tx_out(block_version, output).unwrap();
                validate_tx_out(block_version, change).unwrap();

                assert!(
                    !subaddress_matches_tx_out(&recipient, DEFAULT_SUBADDRESS_INDEX, change)
                        .unwrap()
                );
                assert!(
                    !subaddress_matches_tx_out(&sender, DEFAULT_SUBADDRESS_INDEX, change).unwrap()
                );
                assert!(
                    !subaddress_matches_tx_out(&sender, CHANGE_SUBADDRESS_INDEX, output).unwrap()
                );
                assert!(
                    !subaddress_matches_tx_out(&recipient, CHANGE_SUBADDRESS_INDEX, output)
                        .unwrap()
                );

                // The 1st output should belong to the correct recipient and have correct amount
                // and have correct memo
                {
                    let ss = get_tx_out_shared_secret(
                        recipient.view_private_key(),
                        &RistrettoPublic::try_from(&output.public_key).unwrap(),
                    );
                    let (amount, _) = output.masked_amount.get_value(&ss).unwrap();
                    assert_eq!(amount.value, value - change_value - Mob::MINIMUM_FEE);
                    assert_eq!(amount.token_id, token_id);

                    if block_version.e_memo_feature_is_supported() {
                        let memo = output.e_memo.unwrap().decrypt(&ss);
                        match MemoType::try_from(&memo).expect("Couldn't decrypt memo") {
                            MemoType::AuthenticatedSenderWithPaymentRequestId(memo) => {
                                assert_eq!(
                                    memo.sender_address_hash(),
                                    ShortAddressHash::from(&sender_addr),
                                    "lookup based on address hash failed"
                                );
                                assert!(
                                    bool::from(
                                        memo.validate(
                                            &sender_addr,
                                            &recipient
                                                .subaddress_view_private(DEFAULT_SUBADDRESS_INDEX),
                                            &output.public_key,
                                        )
                                    ),
                                    "hmac validation failed"
                                );
                                assert_eq!(memo.payment_request_id(), 47);
                            }
                            _ => {
                                panic!("unexpected memo type")
                            }
                        }
                    }
                }

                // The 2nd output should belong to the correct recipient and have correct amount
                // and have correct memo
                {
                    let ss = get_tx_out_shared_secret(
                        sender.view_private_key(),
                        &RistrettoPublic::try_from(&change.public_key).unwrap(),
                    );
                    let (amount, _) = change.masked_amount.get_value(&ss).unwrap();
                    assert_eq!(amount.value, change_value);
                    assert_eq!(amount.token_id, token_id);

                    if block_version.e_memo_feature_is_supported() {
                        let memo = change.e_memo.unwrap().decrypt(&ss);
                        match MemoType::try_from(&memo).expect("Couldn't decrypt memo") {
                            MemoType::Unused(_) => {}
                            _ => {
                                panic!("unexpected memo type")
                            }
                        }
                    }
                }
            }

            // Enable destination memo, and set a payment request id, but no sender
            // credential
            {
                let mut memo_builder = RTHMemoBuilder::default();
                memo_builder.enable_destination_memo();
                memo_builder.set_payment_request_id(47);

                let mut transaction_builder = TransactionBuilder::new(
                    block_version,
                    Amount::new(Mob::MINIMUM_FEE, token_id),
                    fog_resolver.clone(),
                    memo_builder,
                )
                .unwrap();

                transaction_builder.set_tombstone_block(2000);

                let input_credentials = get_input_credentials(
                    block_version,
                    Amount { value, token_id },
                    &sender,
                    &fog_resolver,
                    &mut rng,
                );
                transaction_builder.add_input(input_credentials);

                let (_txout, _confirmation) = transaction_builder
                    .add_output(
                        Amount::new(value - change_value - Mob::MINIMUM_FEE, token_id),
                        &recipient_address,
                        &mut rng,
                    )
                    .unwrap();

                transaction_builder
                    .add_change_output(
                        Amount::new(change_value, token_id),
                        &sender_change_dest,
                        &mut rng,
                    )
                    .unwrap();

                let tx = transaction_builder.build(&mut rng).unwrap();

                // The transaction should have two output.
                assert_eq!(tx.prefix.outputs.len(), 2);

                // The tombstone block should be the min of what the user requested, and what
                // fog limits it to
                assert_eq!(tx.prefix.tombstone_block, 1000);

                let output = tx
                    .prefix
                    .outputs
                    .iter()
                    .find(|tx_out| {
                        subaddress_matches_tx_out(&recipient, DEFAULT_SUBADDRESS_INDEX, tx_out)
                            .unwrap()
                    })
                    .expect("Didn't find recipient's output");
                let change = tx
                    .prefix
                    .outputs
                    .iter()
                    .find(|tx_out| {
                        subaddress_matches_tx_out(&sender, CHANGE_SUBADDRESS_INDEX, tx_out).unwrap()
                    })
                    .expect("Didn't find sender's output");

                validate_tx_out(block_version, output).unwrap();
                validate_tx_out(block_version, change).unwrap();

                assert!(
                    !subaddress_matches_tx_out(&recipient, DEFAULT_SUBADDRESS_INDEX, change)
                        .unwrap()
                );
                assert!(
                    !subaddress_matches_tx_out(&sender, DEFAULT_SUBADDRESS_INDEX, change).unwrap()
                );
                assert!(
                    !subaddress_matches_tx_out(&sender, CHANGE_SUBADDRESS_INDEX, output).unwrap()
                );
                assert!(
                    !subaddress_matches_tx_out(&recipient, CHANGE_SUBADDRESS_INDEX, output)
                        .unwrap()
                );

                // The 1st output should belong to the correct recipient and have correct amount
                // and have correct memo
                {
                    let ss = get_tx_out_shared_secret(
                        recipient.view_private_key(),
                        &RistrettoPublic::try_from(&output.public_key).unwrap(),
                    );
                    let (amount, _) = output.masked_amount.get_value(&ss).unwrap();
                    assert_eq!(amount.value, value - change_value - Mob::MINIMUM_FEE);
                    assert_eq!(amount.token_id, token_id);

                    if block_version.e_memo_feature_is_supported() {
                        let memo = output.e_memo.unwrap().decrypt(&ss);
                        match MemoType::try_from(&memo).expect("Couldn't decrypt memo") {
                            MemoType::Unused(_) => {}
                            _ => {
                                panic!("unexpected memo type")
                            }
                        }
                    }
                }

                // The 2nd output should belong to the correct recipient and have correct amount
                // and have correct memo
                {
                    let ss = get_tx_out_shared_secret(
                        sender.view_private_key(),
                        &RistrettoPublic::try_from(&change.public_key).unwrap(),
                    );
                    let (amount, _) = change.masked_amount.get_value(&ss).unwrap();
                    assert_eq!(amount.value, change_value);
                    assert_eq!(amount.token_id, token_id);

                    if block_version.e_memo_feature_is_supported() {
                        let memo = change.e_memo.unwrap().decrypt(&ss);
                        match MemoType::try_from(&memo).expect("Couldn't decrypt memo") {
                            MemoType::Destination(memo) => {
                                assert_eq!(
                                    memo.get_address_hash(),
                                    &ShortAddressHash::from(&recipient_address),
                                    "lookup based on address hash failed"
                                );
                                assert_eq!(memo.get_num_recipients(), 1);
                                assert_eq!(memo.get_fee(), Mob::MINIMUM_FEE);
                                assert_eq!(
                                    memo.get_total_outlay(),
                                    value - change_value,
                                    "outlay should be amount sent to recipient + fee"
                                );
                            }
                            _ => {
                                panic!("unexpected memo type")
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    // Transaction builder with RTH memo builder and custom sender credential
    fn test_transaction_builder_memo_custom_sender() {
        let mut rng: StdRng = SeedableRng::from_seed([1u8; 32]);

        for (block_version, token_id) in get_block_version_token_id_pairs() {
            let alice = AccountKey::random_with_fog(&mut rng);
            let alice_change_dest = ReservedDestination::from(&alice);
            let bob = AccountKey::random_with_fog(&mut rng);
            let bob_address = bob.default_subaddress();
            let charlie = AccountKey::random_with_fog(&mut rng);
            let charlie_addr = charlie.default_subaddress();
            let ingest_private_key = RistrettoPrivate::from_random(&mut rng);
            let value = 1475 * MILLIMOB_TO_PICOMOB;
            let change_value = 128 * MILLIMOB_TO_PICOMOB;

            let fog_resolver = MockFogResolver(btreemap! {
                                bob_address
                        .fog_report_url()
                        .unwrap()
                        .to_string()
                =>
                    FullyValidatedFogPubkey {
                        pubkey: RistrettoPublic::from(&ingest_private_key),
                        pubkey_expiry: 1000,
                    },
            });

            // Enable both sender and destination memos, but use a sender credential from
            // Charlie's identity
            {
                let mut memo_builder = RTHMemoBuilder::default();
                memo_builder.set_sender_credential(SenderMemoCredential::from(&charlie));
                memo_builder.enable_destination_memo();

                let mut transaction_builder = TransactionBuilder::new(
                    block_version,
                    Amount::new(Mob::MINIMUM_FEE, token_id),
                    fog_resolver.clone(),
                    memo_builder,
                )
                .unwrap();

                transaction_builder.set_tombstone_block(2000);

                let input_credentials = get_input_credentials(
                    block_version,
                    Amount { value, token_id },
                    &alice,
                    &fog_resolver,
                    &mut rng,
                );
                transaction_builder.add_input(input_credentials);

                let (_txout, _confirmation) = transaction_builder
                    .add_output(
                        Amount::new(value - change_value - Mob::MINIMUM_FEE, token_id),
                        &bob_address,
                        &mut rng,
                    )
                    .unwrap();

                transaction_builder
                    .add_change_output(
                        Amount::new(change_value, token_id),
                        &alice_change_dest,
                        &mut rng,
                    )
                    .unwrap();

                let tx = transaction_builder.build(&mut rng).unwrap();

                // The transaction should have two output.
                assert_eq!(tx.prefix.outputs.len(), 2);

                // The tombstone block should be the min of what the user requested, and what
                // fog limits it to
                assert_eq!(tx.prefix.tombstone_block, 1000);

                let output = tx
                    .prefix
                    .outputs
                    .iter()
                    .find(|tx_out| {
                        subaddress_matches_tx_out(&bob, DEFAULT_SUBADDRESS_INDEX, tx_out).unwrap()
                    })
                    .expect("Didn't find recipient's output");
                let change = tx
                    .prefix
                    .outputs
                    .iter()
                    .find(|tx_out| {
                        subaddress_matches_tx_out(&alice, CHANGE_SUBADDRESS_INDEX, tx_out).unwrap()
                    })
                    .expect("Didn't find sender's output");

                validate_tx_out(block_version, output).unwrap();
                validate_tx_out(block_version, change).unwrap();

                assert!(
                    !subaddress_matches_tx_out(&bob, DEFAULT_SUBADDRESS_INDEX, change).unwrap()
                );
                assert!(
                    !subaddress_matches_tx_out(&alice, DEFAULT_SUBADDRESS_INDEX, change).unwrap()
                );
                assert!(
                    !subaddress_matches_tx_out(&alice, CHANGE_SUBADDRESS_INDEX, output).unwrap()
                );
                assert!(!subaddress_matches_tx_out(&bob, CHANGE_SUBADDRESS_INDEX, output).unwrap());
                assert!(
                    !subaddress_matches_tx_out(&charlie, DEFAULT_SUBADDRESS_INDEX, change).unwrap()
                );
                assert!(
                    !subaddress_matches_tx_out(&charlie, DEFAULT_SUBADDRESS_INDEX, output).unwrap()
                );

                // The 1st output should belong to the correct recipient and have correct amount
                // and have correct memo
                {
                    let ss = get_tx_out_shared_secret(
                        bob.view_private_key(),
                        &RistrettoPublic::try_from(&output.public_key).unwrap(),
                    );
                    let (amount, _) = output.masked_amount.get_value(&ss).unwrap();
                    assert_eq!(amount.value, value - change_value - Mob::MINIMUM_FEE);
                    assert_eq!(amount.token_id, token_id);

                    if block_version.e_memo_feature_is_supported() {
                        let memo = output.e_memo.unwrap().decrypt(&ss);
                        match MemoType::try_from(&memo).expect("Couldn't decrypt memo") {
                            MemoType::AuthenticatedSender(memo) => {
                                assert_eq!(
                                    memo.sender_address_hash(),
                                    ShortAddressHash::from(&charlie_addr),
                                    "lookup based on address hash failed"
                                );
                                assert!(
                                    bool::from(memo.validate(
                                        &charlie_addr,
                                        &bob.subaddress_view_private(DEFAULT_SUBADDRESS_INDEX),
                                        &output.public_key,
                                    )),
                                    "hmac validation failed"
                                );
                            }
                            _ => {
                                panic!("unexpected memo type")
                            }
                        }
                    }
                }

                // The 2nd output should belong to the correct recipient and have correct amount
                // and have correct memo
                {
                    let ss = get_tx_out_shared_secret(
                        alice.view_private_key(),
                        &RistrettoPublic::try_from(&change.public_key).unwrap(),
                    );
                    let (amount, _) = change.masked_amount.get_value(&ss).unwrap();
                    assert_eq!(amount.value, change_value);
                    assert_eq!(amount.token_id, token_id);

                    if block_version.e_memo_feature_is_supported() {
                        let memo = change.e_memo.unwrap().decrypt(&ss);
                        match MemoType::try_from(&memo).expect("Couldn't decrypt memo") {
                            MemoType::Destination(memo) => {
                                assert_eq!(
                                    memo.get_address_hash(),
                                    &ShortAddressHash::from(&bob_address),
                                    "lookup based on address hash failed"
                                );
                                assert_eq!(memo.get_num_recipients(), 1);
                                assert_eq!(memo.get_fee(), Mob::MINIMUM_FEE);
                                assert_eq!(
                                    memo.get_total_outlay(),
                                    value - change_value,
                                    "outlay should be amount sent to recipient + fee"
                                );
                            }
                            _ => {
                                panic!("unexpected memo type")
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    // TransactionBuilder with RTHMemoBuilder expected failures due to modification
    // after change output
    fn transaction_builder_rth_memo_expected_failures() {
        let mut rng: StdRng = SeedableRng::from_seed([1u8; 32]);

        for (block_version, token_id) in get_block_version_token_id_pairs() {
            if !block_version.e_memo_feature_is_supported() {
                continue;
            }

            let sender = AccountKey::random_with_fog(&mut rng);
            let sender_change_dest = ReservedDestination::from(&sender);
            let recipient = AccountKey::random_with_fog(&mut rng);
            let recipient_address = recipient.default_subaddress();
            let ingest_private_key = RistrettoPrivate::from_random(&mut rng);
            let value = 1475 * MILLIMOB_TO_PICOMOB;
            let change_value = 128 * MILLIMOB_TO_PICOMOB;

            let fog_resolver = MockFogResolver(btreemap! {
                                recipient_address
                        .fog_report_url()
                        .unwrap()
                        .to_string()
                =>
                    FullyValidatedFogPubkey {
                        pubkey: RistrettoPublic::from(&ingest_private_key),
                        pubkey_expiry: 1000,
                    },
            });

            // Test that changing things after the change output causes an error as expected
            {
                let mut memo_builder = RTHMemoBuilder::default();
                memo_builder.set_sender_credential(SenderMemoCredential::from(&sender));
                memo_builder.enable_destination_memo();

                let mut transaction_builder = TransactionBuilder::new(
                    block_version,
                    Amount::new(Mob::MINIMUM_FEE, token_id),
                    fog_resolver.clone(),
                    memo_builder,
                )
                .unwrap();

                transaction_builder.set_tombstone_block(2000);

                let input_credentials = get_input_credentials(
                    block_version,
                    Amount { value, token_id },
                    &sender,
                    &fog_resolver,
                    &mut rng,
                );
                transaction_builder.add_input(input_credentials);

                let (_txout, _confirmation) = transaction_builder
                    .add_output(
                        Amount::new(value - change_value - Mob::MINIMUM_FEE, token_id),
                        &recipient_address,
                        &mut rng,
                    )
                    .unwrap();

                transaction_builder
                    .add_change_output(
                        Amount::new(change_value, token_id),
                        &sender_change_dest,
                        &mut rng,
                    )
                    .unwrap();

                assert!(
                    transaction_builder.set_fee(Mob::MINIMUM_FEE * 4).is_err(),
                    "setting fee after change output should be rejected"
                );

                assert!(
                    transaction_builder
                        .add_output(
                            Amount::new(Mob::MINIMUM_FEE, token_id),
                            &recipient_address,
                            &mut rng
                        )
                        .is_err(),
                    "Adding another output after change output should be rejected"
                );

                assert!(
                    transaction_builder
                        .add_change_output(
                            Amount::new(change_value, token_id),
                            &sender_change_dest,
                            &mut rng
                        )
                        .is_err(),
                    "Adding a second change output should be rejected"
                );

                transaction_builder.build(&mut rng).unwrap();
            }
        }
    }

    #[test]
    #[ignore]
    // `build` should return an error if the inputs contain rings of different
    // sizes.
    fn test_inputs_with_different_ring_sizes() {
        unimplemented!()
    }

    #[test]
    // `build` should return an error if the sum of inputs does not equal the sum of
    // outputs and the fee.
    fn test_inputs_do_not_equal_outputs() {
        let mut rng: StdRng = SeedableRng::from_seed([1u8; 32]);

        for (block_version, token_id) in get_block_version_token_id_pairs() {
            let fpr = MockFogResolver::default();
            let alice = AccountKey::random(&mut rng);
            let bob = AccountKey::random(&mut rng);
            let value = 1475;
            let amount = Amount { value, token_id };

            // Mint an initial collection of outputs, including one belonging to Alice.
            let (ring, real_index) = get_ring(block_version, amount, 3, &alice, &fpr, &mut rng);
            let real_output = ring[real_index].clone();

            let onetime_private_key = recover_onetime_private_key(
                &RistrettoPublic::try_from(&real_output.public_key).unwrap(),
                alice.view_private_key(),
                &alice.subaddress_spend_private(DEFAULT_SUBADDRESS_INDEX),
            );

            let membership_proofs: Vec<TxOutMembershipProof> = ring
                .iter()
                .map(|_tx_out| {
                    // TransactionBuilder does not validate membership proofs, but does require one
                    // for each ring member.
                    TxOutMembershipProof::default()
                })
                .collect();

            let input_credentials = InputCredentials::new(
                ring,
                membership_proofs,
                real_index,
                onetime_private_key,
                *alice.view_private_key(),
            )
            .unwrap();

            let mut transaction_builder = TransactionBuilder::new(
                block_version,
                Amount::new(Mob::MINIMUM_FEE, token_id),
                fpr,
                EmptyMemoBuilder::default(),
            )
            .unwrap();
            transaction_builder.add_input(input_credentials);

            let wrong_value = 999;
            transaction_builder
                .add_output(
                    Amount::new(wrong_value, token_id),
                    &bob.default_subaddress(),
                    &mut rng,
                )
                .unwrap();

            let result = transaction_builder.build(&mut rng);
            // Signing should fail if value is not conserved.
            match result {
                Err(TxBuilderError::RingSignatureFailed(_)) => {} // Expected.
                _ => panic!("Unexpected result {:?}", result),
            }
        }
    }

    #[test]
    // `build` should succeed with MAX_INPUTS and MAX_OUTPUTS.
    fn test_max_transaction_size() {
        let mut rng: StdRng = SeedableRng::from_seed([18u8; 32]);

        for (block_version, token_id) in get_block_version_token_id_pairs() {
            let fpr = MockFogResolver::default();
            let sender = AccountKey::random(&mut rng);
            let recipient = AccountKey::random(&mut rng);
            let tx = get_transaction(
                block_version,
                token_id,
                MAX_INPUTS as usize,
                MAX_OUTPUTS as usize,
                &sender,
                &recipient,
                fpr,
                &mut rng,
            )
            .unwrap();
            assert_eq!(tx.prefix.inputs.len(), MAX_INPUTS as usize);
            assert_eq!(tx.prefix.outputs.len(), MAX_OUTPUTS as usize);
        }
    }

    #[test]
    // Ring elements should be sorted by tx_out.public_key
    fn test_ring_elements_are_sorted() {
        let mut rng: StdRng = SeedableRng::from_seed([97u8; 32]);

        for (block_version, token_id) in get_block_version_token_id_pairs() {
            let fpr = MockFogResolver::default();
            let sender = AccountKey::random(&mut rng);
            let recipient = AccountKey::random(&mut rng);
            let num_inputs = 3;
            let num_outputs = 11;
            let tx = get_transaction(
                block_version,
                token_id,
                num_inputs,
                num_outputs,
                &sender,
                &recipient,
                fpr,
                &mut rng,
            )
            .unwrap();

            for tx_in in &tx.prefix.inputs {
                assert!(tx_in
                    .ring
                    .windows(2)
                    .all(|w| w[0].public_key < w[1].public_key));
            }
        }
    }

    #[test]
    // Transaction outputs should be sorted by public key.
    fn test_outputs_are_sorted() {
        let mut rng: StdRng = SeedableRng::from_seed([92u8; 32]);

        for (block_version, token_id) in get_block_version_token_id_pairs() {
            let fpr = MockFogResolver::default();
            let sender = AccountKey::random(&mut rng);
            let recipient = AccountKey::random(&mut rng);
            let num_inputs = 3;
            let num_outputs = 11;
            let tx = get_transaction(
                block_version,
                token_id,
                num_inputs,
                num_outputs,
                &sender,
                &recipient,
                fpr,
                &mut rng,
            )
            .unwrap();

            let outputs = tx.prefix.outputs;
            let mut expected_outputs = outputs.clone();
            expected_outputs.sort_by(|a, b| a.public_key.cmp(&b.public_key));
            assert_eq!(outputs, expected_outputs);
        }
    }

    #[test]
    // Transaction inputs should be sorted by the public key of the first ring
    // element.
    fn test_inputs_are_sorted() {
        let mut rng: StdRng = SeedableRng::from_seed([92u8; 32]);

        for (block_version, token_id) in get_block_version_token_id_pairs() {
            let fpr = MockFogResolver::default();
            let sender = AccountKey::random(&mut rng);
            let recipient = AccountKey::random(&mut rng);
            let num_inputs = 3;
            let num_outputs = 11;
            let tx = get_transaction(
                block_version,
                token_id,
                num_inputs,
                num_outputs,
                &sender,
                &recipient,
                fpr,
                &mut rng,
            )
            .unwrap();

            let inputs = tx.prefix.inputs;
            let mut expected_inputs = inputs.clone();
            expected_inputs.sort_by(|a, b| a.ring[0].public_key.cmp(&b.ring[0].public_key));
            assert_eq!(inputs, expected_inputs);
        }
    }

    #[test]
    // Test that sending money to a burn address works, and that view key scanning
    // reveals the amount correctly.
    fn test_burn_address() {
        let mut rng: StdRng = SeedableRng::from_seed([18u8; 32]);

        let block_version = BlockVersion::MAX;

        let fog_resolver = MockFogResolver::default();
        let sender = AccountKey::random(&mut rng);
        let sender_change_dest = ReservedDestination::from(&sender);
        let recipient = burn_address();

        let value = 1475 * MILLIMOB_TO_PICOMOB;
        let change_value = 128 * MILLIMOB_TO_PICOMOB;

        let token_id = Mob::ID;

        for _ in 0..3 {
            let mut memo_builder = RTHMemoBuilder::default();
            memo_builder.set_sender_credential(SenderMemoCredential::from(&sender));
            memo_builder.enable_destination_memo();

            let mut transaction_builder = TransactionBuilder::new(
                block_version,
                Amount::new(Mob::MINIMUM_FEE, token_id),
                fog_resolver.clone(),
                memo_builder,
            )
            .unwrap();

            let input_credentials = get_input_credentials(
                block_version,
                Amount { value, token_id },
                &sender,
                &fog_resolver,
                &mut rng,
            );
            transaction_builder.add_input(input_credentials);

            let (burn_tx_out, _confirmation) = transaction_builder
                .add_output(
                    Amount::new(value - change_value - Mob::MINIMUM_FEE, token_id),
                    &recipient,
                    &mut rng,
                )
                .unwrap();

            transaction_builder
                .add_change_output(
                    Amount::new(change_value, token_id),
                    &sender_change_dest,
                    &mut rng,
                )
                .unwrap();

            let tx = transaction_builder.build(&mut rng).unwrap();

            assert_eq!(tx.prefix.outputs.len(), 2);
            let idx = tx
                .prefix
                .outputs
                .iter()
                .position(|tx_out| tx_out.public_key == burn_tx_out.public_key)
                .unwrap();
            let change_idx = 1 - idx;

            let change_tx_out = &tx.prefix.outputs[change_idx];

            // Test that sender's change subaddress owns the change, and not the burn tx out
            assert!(
                !subaddress_matches_tx_out(&sender, CHANGE_SUBADDRESS_INDEX, &burn_tx_out).unwrap()
            );
            assert!(
                subaddress_matches_tx_out(&sender, CHANGE_SUBADDRESS_INDEX, change_tx_out).unwrap()
            );

            // Test that view key matching works with the burn tx out with burn address view
            // key
            let (amount, _) = burn_tx_out
                .view_key_match(&burn_address_view_private())
                .unwrap();
            assert_eq!(amount.value, value - change_value - Mob::MINIMUM_FEE);

            assert!(change_tx_out
                .view_key_match(&burn_address_view_private())
                .is_err());

            // Test that view key matching works with the change tx out with sender's view
            // key
            let (amount, _) = change_tx_out
                .view_key_match(sender.view_private_key())
                .unwrap();
            assert_eq!(amount.value, change_value);

            assert!(burn_tx_out
                .view_key_match(sender.view_private_key())
                .is_err());
        }
    }

    #[test]
    // Transaction builder with Burn Redemption memo builder
    fn test_transaction_builder_burn_redemption_memos() {
        let mut rng: StdRng = SeedableRng::from_seed([1u8; 32]);
        let block_version = BlockVersion::MAX;
        let token_id = TokenId::from(5);
        let fog_resolver = MockFogResolver::default();
        let sender = AccountKey::random(&mut rng);
        let change_destination = ReservedDestination::from(&sender);

        // Adding an output that is not to the burn address is not allowed.
        {
            let memo_builder = BurnRedemptionMemoBuilder::new([2u8; 64]);

            let mut transaction_builder = TransactionBuilder::new(
                block_version,
                Amount::new(10, token_id),
                fog_resolver.clone(),
                memo_builder,
            )
            .unwrap();

            let recipient = AccountKey::random(&mut rng);
            let result = transaction_builder.add_output(
                Amount::new(100, token_id),
                &recipient.default_subaddress(),
                &mut rng,
            );
            assert_matches!(
                result,
                Err(TxBuilderError::NewTx(NewTxError::Memo(
                    NewMemoError::InvalidRecipient
                )))
            );
        }

        // Adding two burn outputs is not allowed.
        {
            let memo_builder = BurnRedemptionMemoBuilder::new([2u8; 64]);

            let mut transaction_builder = TransactionBuilder::new(
                block_version,
                Amount::new(10, token_id),
                fog_resolver.clone(),
                memo_builder,
            )
            .unwrap();

            transaction_builder
                .add_output(Amount::new(100, token_id), &burn_address(), &mut rng)
                .unwrap();

            let result = transaction_builder.add_output(
                Amount::new(100, token_id),
                &burn_address(),
                &mut rng,
            );
            assert_matches!(
                result,
                Err(TxBuilderError::NewTx(NewTxError::Memo(
                    NewMemoError::MultipleOutputs
                )))
            );
        }

        // Adding a change output before a burn output is not allowed.
        {
            let mut memo_builder = BurnRedemptionMemoBuilder::new([2u8; 64]);
            memo_builder.enable_destination_memo();

            let mut transaction_builder = TransactionBuilder::new(
                block_version,
                Amount::new(10, token_id),
                fog_resolver.clone(),
                memo_builder,
            )
            .unwrap();

            let result = transaction_builder.add_change_output(
                Amount::new(10, token_id),
                &change_destination,
                &mut rng,
            );

            assert_matches!(
                result,
                Err(TxBuilderError::NewTx(NewTxError::Memo(
                    NewMemoError::MissingOutput
                )))
            );
        }

        // Setting fee after change output has been written is not allowed.
        {
            let mut memo_builder = BurnRedemptionMemoBuilder::new([3u8; 64]);
            memo_builder.enable_destination_memo();

            let mut transaction_builder = TransactionBuilder::new(
                block_version,
                Amount::new(10, token_id),
                fog_resolver.clone(),
                memo_builder,
            )
            .unwrap();

            transaction_builder.set_fee(3).unwrap();

            let input_credentials = get_input_credentials(
                block_version,
                Amount::new(113, token_id),
                &AccountKey::random(&mut rng),
                &fog_resolver,
                &mut rng,
            );
            transaction_builder.add_input(input_credentials);

            let (_burn_tx_out, _confirmation) = transaction_builder
                .add_output(Amount::new(100, token_id), &burn_address(), &mut rng)
                .unwrap();

            transaction_builder
                .add_change_output(Amount::new(10, token_id), &change_destination, &mut rng)
                .unwrap();

            let result = transaction_builder.set_fee(1235);
            assert_matches!(
                result,
                Err(TxBuilderError::Memo(NewMemoError::FeeAfterChange))
            );
        }

        // Change in a different token is not allowed.
        {
            let mut memo_builder = BurnRedemptionMemoBuilder::new([3u8; 64]);
            memo_builder.enable_destination_memo();

            let mut transaction_builder = TransactionBuilder::new(
                block_version,
                Amount::new(10, Mob::ID),
                fog_resolver.clone(),
                memo_builder,
            )
            .unwrap();

            let (_burn_tx_out, _confirmation) = transaction_builder
                .add_output(Amount::new(100, token_id), &burn_address(), &mut rng)
                .unwrap();

            let result = transaction_builder.add_change_output(
                Amount::new(10, token_id),
                &change_destination,
                &mut rng,
            );

            assert_matches!(
                result,
                Err(TxBuilderError::NewTx(NewTxError::Memo(
                    NewMemoError::MixedTokenIds
                )))
            );
        }

        // Happy flow without change
        {
            let mut memo_builder = BurnRedemptionMemoBuilder::new([2u8; 64]);
            memo_builder.enable_destination_memo();

            let mut transaction_builder = TransactionBuilder::new(
                block_version,
                Amount::new(10, token_id),
                fog_resolver.clone(),
                memo_builder,
            )
            .unwrap();

            transaction_builder.set_fee(3).unwrap();

            let input_credentials = get_input_credentials(
                block_version,
                Amount::new(113, token_id),
                &AccountKey::random(&mut rng),
                &fog_resolver,
                &mut rng,
            );
            transaction_builder.add_input(input_credentials);

            let (burn_output, _confirmation) = transaction_builder
                .add_output(Amount::new(110, token_id), &burn_address(), &mut rng)
                .unwrap();

            let tx = transaction_builder.build(&mut rng).expect("build tx");

            assert_eq!(tx.prefix.outputs.len(), 1);
            assert_eq!(burn_output, tx.prefix.outputs[0]);

            // Test that view key matching works with the burn tx out with burn address view
            // key
            let (amount, _) = burn_output
                .view_key_match(&burn_address_view_private())
                .unwrap();
            assert_eq!(amount, Amount::new(110, token_id));

            // Burn output should have a burn redemption memo
            let ss = get_tx_out_shared_secret(
                &burn_address_view_private(),
                &RistrettoPublic::try_from(&burn_output.public_key).unwrap(),
            );
            let memo = burn_output.e_memo.unwrap().decrypt(&ss);
            match MemoType::try_from(&memo).expect("Couldn't decrypt memo") {
                MemoType::BurnRedemption(memo) => {
                    assert_eq!(memo.memo_data(), &[2u8; 64],);
                }
                _ => {
                    panic!("unexpected memo type")
                }
            }
        }

        // Happy flow with change
        {
            let mut memo_builder = BurnRedemptionMemoBuilder::new([3u8; 64]);
            memo_builder.enable_destination_memo();

            let mut transaction_builder = TransactionBuilder::new(
                block_version,
                Amount::new(10, token_id),
                fog_resolver.clone(),
                memo_builder,
            )
            .unwrap();

            transaction_builder.set_fee(3).unwrap();

            let input_credentials = get_input_credentials(
                block_version,
                Amount::new(113, token_id),
                &AccountKey::random(&mut rng),
                &fog_resolver,
                &mut rng,
            );
            transaction_builder.add_input(input_credentials);

            let (burn_tx_out, _confirmation) = transaction_builder
                .add_output(Amount::new(100, token_id), &burn_address(), &mut rng)
                .unwrap();

            transaction_builder
                .add_change_output(Amount::new(10, token_id), &change_destination, &mut rng)
                .unwrap();

            let tx = transaction_builder.build(&mut rng).expect("build tx");

            assert_eq!(tx.prefix.outputs.len(), 2);

            let burn_output = tx
                .prefix
                .outputs
                .iter()
                .find(|tx_out| tx_out.public_key == burn_tx_out.public_key)
                .expect("Didn't find recipient's output");
            let change_output = tx
                .prefix
                .outputs
                .iter()
                .find(|tx_out| {
                    subaddress_matches_tx_out(&sender, CHANGE_SUBADDRESS_INDEX, tx_out).unwrap()
                })
                .expect("Didn't find sender's output");

            // Test that view key matching works with the burn tx out with burn address view
            // key
            let (amount, _) = burn_output
                .view_key_match(&burn_address_view_private())
                .unwrap();
            assert_eq!(amount, Amount::new(100, token_id));

            assert!(change_output
                .view_key_match(&burn_address_view_private())
                .is_err());

            // Test that view key matching works with the change tx out with sender's view
            // key
            let (amount, _) = change_output
                .view_key_match(sender.view_private_key())
                .unwrap();
            assert_eq!(amount, Amount::new(10, token_id));

            assert!(burn_output
                .view_key_match(sender.view_private_key())
                .is_err());

            // Burn output should have a burn redemption memo
            let ss = get_tx_out_shared_secret(
                &burn_address_view_private(),
                &RistrettoPublic::try_from(&burn_output.public_key).unwrap(),
            );
            let memo = burn_output.e_memo.unwrap().decrypt(&ss);
            match MemoType::try_from(&memo).expect("Couldn't decrypt memo") {
                MemoType::BurnRedemption(memo) => {
                    assert_eq!(memo.memo_data(), &[3u8; 64],);
                }
                _ => {
                    panic!("unexpected memo type")
                }
            }

            // Change output should have a destination memo
            let ss = get_tx_out_shared_secret(
                sender.view_private_key(),
                &RistrettoPublic::try_from(&change_output.public_key).unwrap(),
            );
            let memo = change_output.e_memo.unwrap().decrypt(&ss);
            match MemoType::try_from(&memo).expect("Couldn't decrypt memo") {
                MemoType::Destination(memo) => {
                    assert_eq!(
                        memo.get_address_hash(),
                        &ShortAddressHash::from(&burn_address()),
                        "lookup based on address hash failed"
                    );
                    assert_eq!(memo.get_num_recipients(), 1);
                    assert_eq!(memo.get_fee(), 3);
                    assert_eq!(
                        memo.get_total_outlay(),
                        103,
                        "outlay should be amount sent to recipient + fee"
                    );
                }
                _ => {
                    panic!("unexpected memo type")
                }
            }
        }
    }

    #[test]
    // Test that sending mixed transactions works
    //
    // This test uses inputs of two different token IDs, paying the fee and creating
    // change with TokenID1, and "passing through" the second token ID with its
    // full amount as output.
    fn test_mixed_transactions() {
        let mut rng: StdRng = SeedableRng::from_seed([18u8; 32]);

        let fog_resolver = MockFogResolver::default();
        let sender = AccountKey::random(&mut rng);
        let sender_change_dest = ReservedDestination::from(&sender);
        let recipient = AccountKey::random(&mut rng);
        let recipient_addr = recipient.default_subaddress();

        let amount1 = Amount::new(1475 * MILLIMOB_TO_PICOMOB, Mob::ID);
        let change_amount = Amount::new(128 * MILLIMOB_TO_PICOMOB, Mob::ID);
        let amount2 = Amount::new(999999, 2.into());

        let tx_out1_right_amount = Amount::new(
            amount1.value - change_amount.value - Mob::MINIMUM_FEE,
            Mob::ID,
        );

        for block_version in 3..=*BlockVersion::MAX {
            let block_version = BlockVersion::try_from(block_version).unwrap();
            let memo_builder = EmptyMemoBuilder::default();

            let mut transaction_builder = TransactionBuilder::new(
                block_version,
                Amount::new(Mob::MINIMUM_FEE, Mob::ID),
                fog_resolver.clone(),
                memo_builder,
            )
            .unwrap();

            let input_credentials =
                get_input_credentials(block_version, amount1, &sender, &fog_resolver, &mut rng);
            transaction_builder.add_input(input_credentials);

            let input_credentials =
                get_input_credentials(block_version, amount2, &sender, &fog_resolver, &mut rng);
            transaction_builder.add_input(input_credentials);

            let (tx_out1, _confirmation) = transaction_builder
                .add_output(tx_out1_right_amount, &recipient_addr, &mut rng)
                .unwrap();

            let (tx_out2, _confirmation) = transaction_builder
                .add_output(amount2, &recipient_addr, &mut rng)
                .unwrap();

            transaction_builder
                .add_change_output(change_amount, &sender_change_dest, &mut rng)
                .unwrap();

            let tx = transaction_builder.build(&mut rng).unwrap();

            assert_eq!(tx.prefix.outputs.len(), 3);
            let idx1 = tx
                .prefix
                .outputs
                .iter()
                .position(|tx_out| tx_out.public_key == tx_out1.public_key)
                .unwrap();
            let idx2 = tx
                .prefix
                .outputs
                .iter()
                .position(|tx_out| tx_out.public_key == tx_out2.public_key)
                .unwrap();
            let change_idx = (0..3).find(|x| *x != idx1 && *x != idx2).unwrap();

            let change_tx_out = &tx.prefix.outputs[change_idx];

            // Test that sender's change subaddress owns the change, and not the other tx
            // outs
            assert!(
                !subaddress_matches_tx_out(&sender, CHANGE_SUBADDRESS_INDEX, &tx_out1).unwrap()
            );
            assert!(
                !subaddress_matches_tx_out(&sender, CHANGE_SUBADDRESS_INDEX, &tx_out2).unwrap()
            );
            assert!(
                subaddress_matches_tx_out(&sender, CHANGE_SUBADDRESS_INDEX, change_tx_out).unwrap()
            );

            // Test that recipients's default subaddress owns the correct output, and not
            // the other tx outs
            assert!(
                subaddress_matches_tx_out(&recipient, DEFAULT_SUBADDRESS_INDEX, &tx_out1).unwrap()
            );
            assert!(
                subaddress_matches_tx_out(&recipient, DEFAULT_SUBADDRESS_INDEX, &tx_out2).unwrap()
            );
            assert!(!subaddress_matches_tx_out(
                &recipient,
                DEFAULT_SUBADDRESS_INDEX,
                change_tx_out
            )
            .unwrap());

            // Test that view key matching works with the two tx outs
            let (amount, _) = tx_out1
                .view_key_match(recipient.view_private_key())
                .unwrap();
            assert_eq!(
                amount.value,
                amount1.value - change_amount.value - Mob::MINIMUM_FEE
            );
            assert_eq!(amount.token_id, Mob::ID);

            let (amount, _) = tx_out2
                .view_key_match(recipient.view_private_key())
                .unwrap();
            assert_eq!(amount, amount2);

            assert!(change_tx_out
                .view_key_match(recipient.view_private_key())
                .is_err());

            // Test that view key matching works with the change tx out with sender's view
            // key
            let (amount, _) = change_tx_out
                .view_key_match(sender.view_private_key())
                .unwrap();
            assert_eq!(amount.value, change_amount.value);

            assert!(tx_out1.view_key_match(sender.view_private_key()).is_err());

            assert!(tx_out2.view_key_match(sender.view_private_key()).is_err());
        }
    }

    #[test]
    // Test mixed transactions expected failures (imbalanced transactions)
    fn test_mixed_transactions_expected_failure_imbalanced_transactions() {
        let mut rng: StdRng = SeedableRng::from_seed([18u8; 32]);

        let fog_resolver = MockFogResolver::default();
        let sender = AccountKey::random(&mut rng);
        let sender_change_dest = ReservedDestination::from(&sender);
        let recipient = AccountKey::random(&mut rng);
        let recipient_addr = recipient.default_subaddress();

        let amount1 = Amount::new(1475 * MILLIMOB_TO_PICOMOB, Mob::ID);
        let change_amount = Amount::new(128 * MILLIMOB_TO_PICOMOB, Mob::ID);
        let amount2 = Amount::new(999999, 2.into());

        let tx_out1_right_amount = Amount::new(
            amount1.value - change_amount.value - Mob::MINIMUM_FEE,
            Mob::ID,
        );

        // Builds a transaction using a particular amount in place of tx_out1, returning
        // result of `.build()`
        let mut test_fn = |block_version, tx_out1_amount| -> Result<_, _> {
            let memo_builder = EmptyMemoBuilder::default();

            let mut transaction_builder = TransactionBuilder::new(
                block_version,
                Amount::new(Mob::MINIMUM_FEE, Mob::ID),
                fog_resolver.clone(),
                memo_builder,
            )
            .unwrap();

            let input_credentials =
                get_input_credentials(block_version, amount1, &sender, &fog_resolver, &mut rng);
            transaction_builder.add_input(input_credentials);

            let input_credentials =
                get_input_credentials(block_version, amount2, &sender, &fog_resolver, &mut rng);
            transaction_builder.add_input(input_credentials);

            let (_tx_out1, _confirmation) = transaction_builder
                .add_output(tx_out1_amount, &recipient_addr, &mut rng)
                .unwrap();

            let (_tx_out2, _confirmation) = transaction_builder
                .add_output(amount2, &recipient_addr, &mut rng)
                .unwrap();

            transaction_builder
                .add_change_output(change_amount, &sender_change_dest, &mut rng)
                .unwrap();

            transaction_builder.build(&mut rng)
        };

        for block_version in 3..=*BlockVersion::MAX {
            let block_version = BlockVersion::try_from(block_version).unwrap();

            assert!(test_fn(block_version, tx_out1_right_amount).is_ok());

            let mut tx_out1_wrong_amount = tx_out1_right_amount;
            tx_out1_wrong_amount.value -= 1;
            assert!(test_fn(block_version, tx_out1_wrong_amount).is_err());

            tx_out1_wrong_amount.value += 2;
            assert!(test_fn(block_version, tx_out1_wrong_amount).is_err());

            tx_out1_wrong_amount.token_id = 99.into();
            assert!(test_fn(block_version, tx_out1_wrong_amount).is_err());

            tx_out1_wrong_amount.value -= 1;
            assert!(test_fn(block_version, tx_out1_wrong_amount).is_err());
        }
    }
}
