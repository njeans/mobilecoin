// Copyright (c) 2018-2022 The MobileCoin Foundation

//! Entrypoint for the consensus mint client.

use clap::Parser;
use grpcio::{ChannelBuilder, EnvBuilder};
use mc_common::logger::{create_app_logger, o};
use mc_consensus_api::{
    consensus_client_grpc::ConsensusClientApiClient, consensus_common_grpc::BlockchainApiClient,
    empty::Empty,
};
use mc_consensus_enclave_api::GovernorsSigner;
use mc_consensus_mint_client::{Commands, Config};
use mc_crypto_keys::Ed25519Pair;
use mc_crypto_multisig::MultiSig;
use mc_transaction_core::{
    constants::MAX_TOMBSTONE_BLOCKS,
    mint::{MintConfigTx, MintTx},
};
use mc_util_grpc::ConnectionUriGrpcioChannel;
use protobuf::ProtobufEnum;
use serde::de::DeserializeOwned;
use std::{fs, path::PathBuf, process::exit, sync::Arc};

fn main() {
    let (logger, _global_logger_guard) = create_app_logger(o!());
    let config = Config::parse();

    match config.command {
        Commands::GenerateAndSubmitMintConfigTx { node, params } => {
            let env = Arc::new(EnvBuilder::new().name_prefix("mint-client-grpc").build());
            let ch = ChannelBuilder::default_channel_builder(env).connect_to_uri(&node, &logger);
            let client_api = ConsensusClientApiClient::new(ch.clone());
            let blockchain_api = BlockchainApiClient::new(ch);

            let tx = params
                .try_into_mint_config_tx(|| {
                    let last_block_info = blockchain_api
                        .get_last_block_info(&Empty::new())
                        .expect("get last block info");
                    last_block_info.index + MAX_TOMBSTONE_BLOCKS - 1
                })
                .expect("failed creating tx");

            let resp = client_api
                .propose_mint_config_tx(&(&tx).into())
                .expect("propose tx");
            println!("response: {:?}", resp);

            // Relying on the success result code being 0, we terminate ourselves in a way
            // that allows whoever started this binary to easily determine if submitting the
            // transaction succeeded.
            exit(resp.get_result().get_code().value());
        }

        Commands::GenerateMintConfigTx { out, params } => {
            let tx = params
                .try_into_mint_config_tx(|| panic!("missing tombstone block"))
                .expect("failed creating tx");

            let json = serde_json::to_string_pretty(&tx).expect("failed serializing tx");

            fs::write(out, json).expect("failed writing output file");
        }

        Commands::HashMintConfigTx { params } => {
            let tx_prefix = params
                .try_into_mint_config_tx_prefix(|| panic!("missing tombstone block"))
                .expect("failed creating tx prefix");

            // Print the nonce, since if we generated it randomlly then there is no way to
            // reconstruct the tx prefix that is being hashed without it.
            println!("Nonce: {}", hex::encode(&tx_prefix.nonce));

            let hash = tx_prefix.hash();
            println!("Hash: {}", hex::encode(hash));
        }

        Commands::SubmitMintConfigTx { node, tx_filenames } => {
            // Load all txs.
            let txs: Vec<MintConfigTx> = load_json_files(&tx_filenames);

            // All tx prefixes should be the same.
            if !txs.windows(2).all(|pair| pair[0].prefix == pair[1].prefix) {
                panic!("All txs must have the same prefix");
            }

            // Collect all signatures.
            let mut signatures = txs
                .iter()
                .flat_map(|tx| tx.signature.signatures())
                .cloned()
                .collect::<Vec<_>>();
            signatures.sort();
            signatures.dedup();

            let merged_tx = MintConfigTx {
                prefix: txs[0].prefix.clone(),
                signature: MultiSig::new(signatures),
            };

            let env = Arc::new(EnvBuilder::new().name_prefix("mint-client-grpc").build());
            let ch = ChannelBuilder::default_channel_builder(env).connect_to_uri(&node, &logger);
            let client_api = ConsensusClientApiClient::new(ch);

            let resp = client_api
                .propose_mint_config_tx(&(&merged_tx).into())
                .expect("propose tx");
            println!("response: {:?}", resp);

            // Relying on the success result code being 0, we terminate ourselves in a way
            // that allows whoever started this binary to easily determine if submitting the
            // transaction succeeded.
            exit(resp.get_result().get_code().value());
        }

        Commands::GenerateAndSubmitMintTx { node, params } => {
            let env = Arc::new(EnvBuilder::new().name_prefix("mint-client-grpc").build());
            let ch = ChannelBuilder::default_channel_builder(env).connect_to_uri(&node, &logger);
            let client_api = ConsensusClientApiClient::new(ch.clone());
            let blockchain_api = BlockchainApiClient::new(ch);

            let tx = params
                .try_into_mint_tx(|| {
                    let last_block_info = blockchain_api
                        .get_last_block_info(&Empty::new())
                        .expect("get last block info");
                    last_block_info.index + MAX_TOMBSTONE_BLOCKS - 1
                })
                .expect("failed creating tx");
            let resp = client_api
                .propose_mint_tx(&(&tx).into())
                .expect("propose tx");
            println!("response: {:?}", resp);

            // Relying on the success result code being 0, we terminate ourselves in a way
            // that allows whoever started this binary to easily determine if submitting the
            // transaction succeeded.
            exit(resp.get_result().get_code().value());
        }

        Commands::GenerateMintTx { out, params } => {
            let tx = params
                .try_into_mint_tx(|| panic!("missing tombstone block"))
                .expect("failed creating tx");

            let json = serde_json::to_string_pretty(&tx).expect("failed serializing tx");

            fs::write(out, json).expect("failed writing output file");
        }

        Commands::HashMintTx { params } => {
            let tx_prefix = params
                .try_into_mint_tx_prefix(|| panic!("missing tombstone block"))
                .expect("failed creating tx prefix");

            // Print the nonce, since if we generated it randomlly then there is no way to
            // reconstruct the tx prefix that is being hashed without it.
            println!("Nonce: {}", hex::encode(&tx_prefix.nonce));

            let hash = tx_prefix.hash();
            println!("Hash: {}", hex::encode(hash));
        }

        Commands::SubmitMintTx { node, tx_filenames } => {
            // Load all txs.
            let txs: Vec<MintTx> = load_json_files(&tx_filenames);

            // All tx prefixes should be the same.
            if !txs.windows(2).all(|pair| pair[0].prefix == pair[1].prefix) {
                panic!("All txs must have the same prefix");
            }

            // Collect all signatures.
            let mut signatures = txs
                .iter()
                .flat_map(|tx| tx.signature.signatures())
                .cloned()
                .collect::<Vec<_>>();
            signatures.sort();
            signatures.dedup();

            let merged_tx = MintTx {
                prefix: txs[0].prefix.clone(),
                signature: MultiSig::new(signatures),
            };

            let env = Arc::new(EnvBuilder::new().name_prefix("mint-client-grpc").build());
            let ch = ChannelBuilder::default_channel_builder(env).connect_to_uri(&node, &logger);
            let client_api = ConsensusClientApiClient::new(ch);

            let resp = client_api
                .propose_mint_tx(&(&merged_tx).into())
                .expect("propose tx");
            println!("response: {:?}", resp);

            // Relying on the success result code being 0, we terminate ourselves in a way
            // that allows whoever started this binary to easily determine if submitting the
            // transaction succeeded.
            exit(resp.get_result().get_code().value());
        }

        Commands::SignGovernors {
            signing_key,
            mut tokens,
            output_toml,
            output_json,
        } => {
            let governors_map = tokens
                .token_id_to_governors()
                .expect("governors configuration error");
            let signature = Ed25519Pair::from(signing_key)
                .sign_governors_map(&governors_map)
                .expect("failed signing governors map");
            println!("Signature: {}", hex::encode(signature.as_ref()));
            println!("Put this signature in the governors configuration file in the key \"governors_signature\".");

            tokens.governors_signature = Some(signature);

            if let Some(path) = output_toml {
                let toml_str = toml::to_string_pretty(&tokens).expect("failed serializing toml");
                fs::write(path, toml_str).expect("failed writing output file");
            }

            if let Some(path) = output_json {
                let json_str =
                    serde_json::to_string_pretty(&tokens).expect("failed serializing json");
                fs::write(path, json_str).expect("failed writing output file");
            }
        }
    }
}

fn load_json_files<T: DeserializeOwned>(filenames: &[PathBuf]) -> Vec<T> {
    filenames
        .iter()
        .map(|filename| {
            let json = fs::read_to_string(filename)
                .unwrap_or_else(|err| panic!("Failed reading file {:?}: {}", filename, err));
            serde_json::from_str(&json)
                .unwrap_or_else(|err| panic!("Failed parsing tx from file {:?}: {}", filename, err))
        })
        .collect()
}
