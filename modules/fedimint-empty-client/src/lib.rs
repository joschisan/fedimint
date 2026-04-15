#![deny(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]

use fedimint_client_module::module::init::{ClientModuleInit, ClientModuleInitArgs};
use fedimint_client_module::module::{ClientContext, ClientModule};
use fedimint_core::module::{ModuleCommon, ModuleInit};
use fedimint_core::{Amount, apply, async_trait_maybe_send};
pub use fedimint_empty_common as common;
use fedimint_empty_common::config::EmptyClientConfig;
use fedimint_empty_common::{EmptyCommonInit, EmptyModuleTypes};
use fedimint_redb::{Database, WriteTxRef};

pub mod api;

#[derive(Debug)]
pub struct EmptyClientModule {
    #[allow(dead_code)]
    cfg: EmptyClientConfig,
    #[allow(dead_code)]
    client_ctx: ClientContext<Self>,
    #[allow(dead_code)]
    db: Database,
}

#[apply(async_trait_maybe_send!)]
impl ClientModule for EmptyClientModule {
    type Init = EmptyClientInit;
    type Common = EmptyModuleTypes;

    fn input_fee(
        &self,
        _amount: Amount,
        _input: &<Self::Common as ModuleCommon>::Input,
    ) -> Option<Amount> {
        unreachable!()
    }

    fn output_fee(
        &self,
        _amount: Amount,
        _output: &<Self::Common as ModuleCommon>::Output,
    ) -> Option<Amount> {
        unreachable!()
    }

    async fn get_balance(&self, _dbtx: &WriteTxRef<'_>) -> Amount {
        Amount::ZERO
    }
}

#[derive(Debug, Clone)]
pub struct EmptyClientInit;

// TODO: Boilerplate-code
impl ModuleInit for EmptyClientInit {
    type Common = EmptyCommonInit;
}

/// Generates the client module
#[apply(async_trait_maybe_send!)]
impl ClientModuleInit for EmptyClientInit {
    type Module = EmptyClientModule;

    async fn init(&self, args: &ClientModuleInitArgs<Self>) -> anyhow::Result<Self::Module> {
        Ok(EmptyClientModule {
            cfg: args.cfg().clone(),
            client_ctx: args.context(),
            db: args.db().clone(),
        })
    }
}
