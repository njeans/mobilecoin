// Copyright (c) 2018-2022 The MobileCoin Foundation

use crate::{ring_signature::CurveScalar, tokens::Mob, Amount, MaskedAmount, Token};
use curve25519_dalek::scalar::Scalar;
use mc_crypto_keys::{RistrettoPrivate, RistrettoPublic};
use proptest::prelude::*;

/// Generates an arbitrary Scalar.
pub fn arbitrary_scalar() -> impl Strategy<Value = Scalar> {
    any::<[u8; 32]>().prop_map(Scalar::from_bytes_mod_order)
}

/// Generates an arbitrary CurveScalar.
pub fn arbitrary_curve_scalar() -> impl Strategy<Value = CurveScalar> {
    arbitrary_scalar().prop_map(CurveScalar::from)
}

/// Generates an arbitrary RistrettoPrivate key.
pub fn arbitrary_ristretto_private() -> impl Strategy<Value = RistrettoPrivate> {
    arbitrary_scalar().prop_map(RistrettoPrivate::from)
}

/// Generates an arbitrary RistrettoPublic key.
pub fn arbitrary_ristretto_public() -> impl Strategy<Value = RistrettoPublic> {
    arbitrary_ristretto_private().prop_map(|private_key| RistrettoPublic::from(&private_key))
}

prop_compose! {
    /// Generates an arbitrary masked_amount with value in [0,max_value].
    /// Of token_id = 0
    pub fn arbitrary_masked_amount(max_value: u64, shared_secret: RistrettoPublic)
                (value in 0..=max_value) -> MaskedAmount {
            let amount = Amount {
                value,
                token_id: Mob::ID,
            };
            MaskedAmount::new(amount, &shared_secret).unwrap()
    }
}
