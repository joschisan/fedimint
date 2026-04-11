use clap::Subcommand;
use fedimint_core::config::FederationId;
use fedimint_core::fedimint_build_code_version_env;
use fedimint_core::util::SafeUrl;
use fedimint_eventlog::{EventKind, EventLogId};
use fedimint_gateway_client::{
    get_balances, get_info, get_invite_codes, get_mnemonic, join_federation, payment_log, stop,
};
use fedimint_gateway_common::{ConnectFedPayload, PaymentLogPayload};
use fedimint_gateway_common::GatewayApi;

use crate::{CliOutput, CliOutputResult};

/// General federation management commands including info, connecting, or
/// leaving a federation.
#[derive(Subcommand)]
pub enum GeneralCommands {
    /// Display the version hash of the CLI.
    VersionHash,
    /// Display high-level information about the gateway.
    Info,
    /// Get the total on-chain, lightning, and eCash balances of the gateway.
    GetBalances,
    /// Register the gateway with a federation.
    Join {
        /// Invite code to connect to the federation
        invite_code: String,
        /// Activate usage of Tor (or not) as the connector for the federation
        /// client
        #[cfg(feature = "tor")]
        use_tor: Option<bool>,
        /// Indicates if the client should be recovered from a mnemonic
        #[clap(long)]
        recover: Option<bool>,
    },
    /// Prints the seed phrase for the gateway
    Seed,
    /// Safely stop the gateway
    Stop,
    /// List the fedimint transactions that the gateway has processed
    PaymentLog {
        #[clap(long)]
        end_position: Option<EventLogId>,

        #[clap(long, default_value_t = 25)]
        pagination_size: usize,

        #[clap(long)]
        federation_id: FederationId,

        #[clap(long)]
        event_kinds: Vec<EventKind>,
    },
    /// Create a bcrypt hash of a password, for use in gateway deployment
    CreatePasswordHash {
        password: String,

        /// The bcrypt cost factor to use when hashing the password
        #[clap(long)]
        cost: Option<u32>,
    },
    /// List all invite codes of each federation the gateway has joined
    InviteCodes,
}

impl GeneralCommands {
    #[allow(clippy::too_many_lines)]
    pub async fn handle(self, client: &GatewayApi, base_url: &SafeUrl) -> CliOutputResult {
        match self {
            Self::VersionHash => {
                // Keep version-hash as raw string output for backward compatibility
                println!("{}", fedimint_build_code_version_env!());
                Ok(CliOutput::Empty)
            }
            Self::Info => {
                let response = get_info(client, base_url).await?;
                Ok(CliOutput::Info(response))
            }
            Self::GetBalances => {
                let response = get_balances(client, base_url).await?;
                Ok(CliOutput::Balances(response))
            }
            Self::Join {
                invite_code,
                #[cfg(feature = "tor")]
                use_tor,
                recover,
            } => {
                let response = join_federation(
                    client,
                    base_url,
                    ConnectFedPayload {
                        invite_code,
                        #[cfg(feature = "tor")]
                        use_tor,
                        #[cfg(not(feature = "tor"))]
                        use_tor: None,
                        recover,
                    },
                )
                .await?;

                Ok(CliOutput::Federation(response))
            }
            Self::Seed => {
                let response = get_mnemonic(client, base_url).await?;
                Ok(CliOutput::Mnemonic(response))
            }
            Self::Stop => {
                stop(client, base_url).await?;
                Ok(CliOutput::Empty)
            }
            Self::PaymentLog {
                end_position,
                pagination_size,
                federation_id,
                event_kinds,
            } => {
                let payment_log = payment_log(
                    client,
                    base_url,
                    PaymentLogPayload {
                        end_position,
                        pagination_size,
                        federation_id,
                        event_kinds,
                    },
                )
                .await?;
                Ok(CliOutput::PaymentLog(payment_log))
            }
            Self::CreatePasswordHash { password, cost } => {
                let hash = bcrypt::hash(password, cost.unwrap_or(bcrypt::DEFAULT_COST))
                    .expect("Unable to create bcrypt hash");
                Ok(CliOutput::PasswordHash(hash))
            }
            Self::InviteCodes => {
                let invite_codes = get_invite_codes(client, base_url).await?;
                Ok(CliOutput::InviteCodes(invite_codes))
            }
        }
    }
}
