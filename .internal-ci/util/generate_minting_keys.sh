#!/bin/bash
# Copyright (c) 2018-2022 The MobileCoin Foundation
#
# Generate minting keys
# See for notes on what keys do what:
# https://www.notion.so/mobilecoin/Consensus-tokens-config-and-Minting-keys-45def9fb96ff4c41ba1ec513934c45a2

set -e

location=$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )

BASE_PATH="${BASE_PATH:-.tmp/seeds/minting}"
mkdir -p "${BASE_PATH}"

# Token 1 governor keys
# This key pair is used to validate MintConfigTxs
if [[ ! -f "${BASE_PATH}/minter1_governor.private.pem" ]]
then
    "${location}/generate_ed25519_keys.sh" \
        --public-out "${BASE_PATH}/minter1_governor.public.pem" \
        --private-out "${BASE_PATH}/minter1_governor.private.pem"
fi

# Token 1 signer keys
# This key pair is used to validate MintTX
if [[ ! -f "${BASE_PATH}/token_signer.private.pem" ]]
then
    "${location}/generate_ed25519_keys.sh" \
        --public-out "${BASE_PATH}/token1_signer.public.pem" \
        --private-out "${BASE_PATH}/token1_signer.private.pem"
fi
