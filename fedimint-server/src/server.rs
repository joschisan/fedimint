//! Concrete `Server` container for the fixed module set.
//!
//! Replaces the dynamic `ServerModuleRegistry` in `ConsensusEngine` once
//! transaction processing, API routing, and DKG are rewired to the static
//! wire enums in [`crate::wire`].

#![allow(dead_code)]

use fedimint_lnv2_server::Lightning as LnServer;
use fedimint_mintv2_server::Mint as MintServer;
use fedimint_walletv2_server::Wallet as WalletServer;

pub mod instance_id {
    use fedimint_core::core::ModuleInstanceId;

    pub const MINT: ModuleInstanceId = 0;
    pub const LN: ModuleInstanceId = 1;
    pub const WALLET: ModuleInstanceId = 2;
}

pub struct Server {
    pub mint: MintServer,
    pub ln: LnServer,
    pub wallet: WalletServer,
}
