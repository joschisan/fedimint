use std::collections::BTreeMap;

use bitcoin::Network;
use fedimint_core::core::ModuleKind;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::envs::BitcoinRpcConfig;
use fedimint_core::module::serde_json;
use fedimint_core::util::SafeUrl;
use fedimint_core::{plugin_types_trait_impl_config, Amount, PeerId};
use secp256k1::{PublicKey, SecretKey};
use serde::{Deserialize, Serialize};

use crate::envs::FM_PORT_ESPLORA_ENV;
use crate::{descriptor, WalletCommonInit};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletGenParams {
    pub local: WalletGenParamsLocal,
    pub consensus: WalletGenParamsConsensus,
}

impl WalletGenParams {
    pub fn regtest(bitcoin_rpc: BitcoinRpcConfig) -> WalletGenParams {
        WalletGenParams {
            local: WalletGenParamsLocal { bitcoin_rpc },
            consensus: WalletGenParamsConsensus {
                network: Network::Regtest,
                client_default_bitcoin_rpc: BitcoinRpcConfig {
                    kind: "esplora".to_string(),
                    url: SafeUrl::parse(&format!(
                        "http://127.0.0.1:{}/",
                        std::env::var(FM_PORT_ESPLORA_ENV).unwrap_or(String::from("50002"))
                    ))
                    .expect("Failed to parse default esplora server"),
                },
                fee_consensus: FeeConsensus::default(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletGenParamsLocal {
    pub bitcoin_rpc: BitcoinRpcConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletGenParamsConsensus {
    pub network: Network,
    pub client_default_bitcoin_rpc: BitcoinRpcConfig,
    pub fee_consensus: FeeConsensus,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalletConfig {
    pub local: WalletConfigLocal,
    pub private: WalletConfigPrivate,
    pub consensus: WalletConfigConsensus,
}

#[derive(Clone, Debug, Serialize, Deserialize, Decodable, Encodable)]
pub struct WalletConfigLocal {
    pub bitcoin_rpc: BitcoinRpcConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalletConfigPrivate {
    pub bitcoin_sk: SecretKey,
}

#[derive(Clone, Debug, Serialize, Deserialize, Encodable, Decodable)]
pub struct WalletConfigConsensus {
    /// The public keys for the bitcoin multisig
    pub bitcoin_pks: BTreeMap<PeerId, PublicKey>,
    /// Fees taken by the guardians to process wallet inputs and outputs
    pub fee_consensus: FeeConsensus,
    /// Bitcoin network (e.g. testnet, bitcoin)
    pub network: Network,
    /// Confirmations required for a peg in to be accepted by federation
    pub finality_delay: u64,
    /// Total weight of a bitcoin transaction to pegout without the pegout
    /// script
    pub pegout_weight: u64,
    /// Total weight of a bitcoin transaction to pegin
    pub pegin_weight: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize, Encodable, Decodable)]
pub struct WalletClientConfig {
    /// The public keys for the bitcoin multisig
    pub bitcoin_pks: BTreeMap<PeerId, PublicKey>,
    /// Fees taken by the guardians to process wallet inputs and outputs
    pub fee_consensus: FeeConsensus,
    /// Bitcoin network (e.g. testnet, bitcoin)
    pub network: Network,
    /// Confirmations required for a peg in to be accepted by federation
    pub finality_delay: u64,
}

impl std::fmt::Display for WalletClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "WalletClientConfig {}",
            serde_json::to_string(self).map_err(|_e| std::fmt::Error)?
        )
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Encodable, Decodable)]
pub struct FeeConsensus {
    pub base: Amount,
    pub parts_per_million: u64,
}

impl FeeConsensus {
    pub fn fee(&self, amount: Amount) -> Amount {
        Amount::from_msats(self.fee_msats(amount.msats))
    }

    fn fee_msats(&self, msats: u64) -> u64 {
        self.base.msats
            + msats
                .saturating_mul(self.parts_per_million)
                .saturating_div(1_000_000)
    }
}

impl Default for FeeConsensus {
    fn default() -> Self {
        Self {
            base: Amount::from_sats(1000),
            parts_per_million: 1_000,
        }
    }
}

impl WalletConfig {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        bitcoin_rpc: BitcoinRpcConfig,
        bitcoin_sk: SecretKey,
        bitcoin_pks: BTreeMap<PeerId, PublicKey>,
        fee_consensus: FeeConsensus,
        network: Network,
    ) -> Self {
        const DUMMY_TWEAK: [u8; 32] = [0; 32];

        let change_descriptor = descriptor(&bitcoin_pks, &DUMMY_TWEAK.consensus_hash());

        let change_input_weight = change_descriptor
            .max_weight_to_satisfy()
            .expect("is satisfyable") 
            + 128  // TxOutHash
            + 16  // TxOutIndex
            + 16; // sequence

        let change_output_weight = change_descriptor.script_pubkey().len() * 4 + 1 + 32;

        let pegout_weight = 16  // version
            + 12  // up to 2**16-1 inputs
            + 12  // up to 2**16-1 outputs
            + 16 // lock time https://github.com/fedimint/fedimint/issues/4590
            + change_input_weight
            + change_output_weight
            + 32 + 1;

        let pegin_weight = 16  // version
            + 12  // up to 2**16-1 inputs
            + 12  // up to 2**16-1 outputs
            + 16 // lock time https://github.com/fedimint/fedimint/issues/4590
            + change_input_weight
            + change_input_weight
            + change_output_weight;

        Self {
            local: WalletConfigLocal { bitcoin_rpc },
            private: WalletConfigPrivate { bitcoin_sk },
            consensus: WalletConfigConsensus {
                network,
                bitcoin_pks,
                fee_consensus,
                finality_delay: 12,
                pegout_weight: pegout_weight as u64,
                pegin_weight: pegin_weight as u64,
            },
        }
    }
}

plugin_types_trait_impl_config!(
    WalletCommonInit,
    WalletGenParams,
    WalletGenParamsLocal,
    WalletGenParamsConsensus,
    WalletConfig,
    WalletConfigLocal,
    WalletConfigPrivate,
    WalletConfigConsensus,
    WalletClientConfig
);
