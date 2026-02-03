mod bitcoin;
mod connect_fed;
mod federation;
mod general;
mod lightning;
mod mnemonic;
mod payment_summary;
mod setup;

use std::collections::BTreeMap;
use std::fmt::Display;
use std::sync::Arc;

use ::bitcoin::{Address, Txid};
use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Form, Router};
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::{Cookie, SameSite};
use fedimint_core::bitcoin::Network;
use fedimint_core::config::FederationId;
use fedimint_core::invite_code::InviteCode;
use fedimint_core::secp256k1::serde::Deserialize;
use fedimint_core::task::TaskGroup;
use fedimint_gateway_common::{
    ChainSource, CloseChannelsWithPeerRequest, CloseChannelsWithPeerResponse, ConnectFedPayload,
    CreateInvoiceForOperatorPayload, CreateOfferPayload, CreateOfferResponse,
    DepositAddressPayload, FederationInfo, GatewayBalances, GatewayInfo, LeaveFedPayload,
    LightningMode, ListTransactionsPayload, ListTransactionsResponse, MnemonicResponse,
    OpenChannelRequest, PayInvoiceForOperatorPayload, PayOfferPayload, PayOfferResponse,
    PaymentLogPayload, PaymentLogResponse, PaymentSummaryPayload, PaymentSummaryResponse,
    ReceiveEcashPayload, ReceiveEcashResponse, SendOnchainRequest, SetFeesPayload,
    SetMnemonicPayload, SpendEcashPayload, SpendEcashResponse, WithdrawPayload,
    WithdrawPreviewPayload, WithdrawPreviewResponse, WithdrawResponse,
};
use fedimint_ln_common::contracts::Preimage;
use fedimint_logging::LOG_GATEWAY_UI;
use fedimint_ui_common::assets::WithStaticRoutesExt;
use fedimint_ui_common::auth::UserAuth;
use fedimint_ui_common::{
    LOGIN_ROUTE, LoginInput, ROOT_ROUTE, UiState, dashboard_layout, login_form_response,
    login_layout,
};
use lightning_invoice::Bolt11Invoice;
use maud::html;
use tracing::debug;

use crate::connect_fed::connect_federation_handler;
use crate::federation::{
    deposit_address_handler, leave_federation_handler, receive_ecash_handler, set_fees_handler,
    spend_ecash_handler, withdraw_confirm_handler, withdraw_preview_handler,
};
use crate::lightning::{
    channels_fragment_handler, close_channel_handler, create_bolt11_invoice_handler,
    create_receive_invoice_handler, detect_payment_type_handler, generate_receive_address_handler,
    open_channel_handler, pay_bolt11_invoice_handler, pay_unified_handler,
    payments_fragment_handler, send_onchain_handler, transactions_fragment_handler,
    wallet_fragment_handler,
};
use crate::mnemonic::{mnemonic_iframe_handler, mnemonic_reveal_handler};
use crate::payment_summary::payment_log_fragment_handler;
use crate::setup::{create_wallet_handler, recover_wallet_form, recover_wallet_handler};
pub type DynGatewayApi<E> = Arc<dyn IAdminGateway<Error = E> + Send + Sync + 'static>;

pub(crate) const OPEN_CHANNEL_ROUTE: &str = "/ui/channels/open";
pub(crate) const CLOSE_CHANNEL_ROUTE: &str = "/ui/channels/close";
pub(crate) const CHANNEL_FRAGMENT_ROUTE: &str = "/ui/channels/fragment";
pub(crate) const LEAVE_FEDERATION_ROUTE: &str = "/ui/federations/{id}/leave";
pub(crate) const CONNECT_FEDERATION_ROUTE: &str = "/ui/federations/join";
pub(crate) const SET_FEES_ROUTE: &str = "/ui/federation/set-fees";
pub(crate) const SEND_ONCHAIN_ROUTE: &str = "/ui/wallet/send";
pub(crate) const WALLET_FRAGMENT_ROUTE: &str = "/ui/wallet/fragment";
pub(crate) const LN_ONCHAIN_ADDRESS_ROUTE: &str = "/ui/wallet/receive";
pub(crate) const DEPOSIT_ADDRESS_ROUTE: &str = "/ui/federations/deposit-address";
pub(crate) const PAYMENTS_FRAGMENT_ROUTE: &str = "/ui/payments/fragment";
pub(crate) const CREATE_BOLT11_INVOICE_ROUTE: &str = "/ui/payments/receive/bolt11";
pub(crate) const CREATE_RECEIVE_INVOICE_ROUTE: &str = "/ui/payments/receive";
pub(crate) const PAY_BOLT11_INVOICE_ROUTE: &str = "/ui/payments/send/bolt11";
pub(crate) const PAY_UNIFIED_ROUTE: &str = "/ui/payments/send";
pub(crate) const DETECT_PAYMENT_TYPE_ROUTE: &str = "/ui/payments/detect";
pub(crate) const TRANSACTIONS_FRAGMENT_ROUTE: &str = "/ui/transactions/fragment";
pub(crate) const RECEIVE_ECASH_ROUTE: &str = "/ui/federations/receive";
pub(crate) const STOP_GATEWAY_ROUTE: &str = "/ui/stop";
pub(crate) const WITHDRAW_PREVIEW_ROUTE: &str = "/ui/federations/withdraw-preview";
pub(crate) const WITHDRAW_CONFIRM_ROUTE: &str = "/ui/federations/withdraw-confirm";
pub(crate) const SPEND_ECASH_ROUTE: &str = "/ui/federations/spend";
pub(crate) const PAYMENT_LOG_ROUTE: &str = "/ui/payment-log";
pub(crate) const CREATE_WALLET_ROUTE: &str = "/ui/wallet/create";
pub(crate) const RECOVER_WALLET_ROUTE: &str = "/ui/wallet/recover";
pub(crate) const MNEMONIC_IFRAME_ROUTE: &str = "/ui/mnemonic/iframe";
pub(crate) const EXPORT_INVITE_CODES_ROUTE: &str = "/ui/export-invite-codes";
pub(crate) const IMPORT_INVITE_CODES_ROUTE: &str = "/ui/federations/import";

#[derive(Default, Deserialize)]
pub struct DashboardQuery {
    pub success: Option<String>,
    pub ui_error: Option<String>,
    pub show_export_reminder: Option<bool>,
}

fn redirect_success(msg: String) -> impl IntoResponse {
    let encoded: String = url::form_urlencoded::byte_serialize(msg.as_bytes()).collect();
    Redirect::to(&format!("/?success={}", encoded))
}

pub(crate) fn redirect_success_with_export_reminder(msg: String) -> impl IntoResponse {
    let encoded: String = url::form_urlencoded::byte_serialize(msg.as_bytes()).collect();
    Redirect::to(&format!("/?success={}&show_export_reminder=true", encoded))
}

fn redirect_error(msg: String) -> impl IntoResponse {
    let encoded: String = url::form_urlencoded::byte_serialize(msg.as_bytes()).collect();
    Redirect::to(&format!("/?ui_error={}", encoded))
}

pub fn is_allowed_setup_route(path: &str) -> bool {
    path == ROOT_ROUTE
        || path == LOGIN_ROUTE
        || path.starts_with("/assets/")
        || path == CREATE_WALLET_ROUTE
        || path == RECOVER_WALLET_ROUTE
}

#[async_trait]
pub trait IAdminGateway {
    type Error;

    async fn handle_get_info(&self) -> Result<GatewayInfo, Self::Error>;

    async fn handle_list_channels_msg(
        &self,
    ) -> Result<Vec<fedimint_gateway_common::ChannelInfo>, Self::Error>;

    async fn handle_payment_summary_msg(
        &self,
        PaymentSummaryPayload {
            start_millis,
            end_millis,
        }: PaymentSummaryPayload,
    ) -> Result<PaymentSummaryResponse, Self::Error>;

    async fn handle_leave_federation(
        &self,
        payload: LeaveFedPayload,
    ) -> Result<FederationInfo, Self::Error>;

    async fn handle_connect_federation(
        &self,
        payload: ConnectFedPayload,
    ) -> Result<FederationInfo, Self::Error>;

    async fn handle_set_fees_msg(&self, payload: SetFeesPayload) -> Result<(), Self::Error>;

    async fn handle_mnemonic_msg(&self) -> Result<MnemonicResponse, Self::Error>;

    async fn handle_open_channel_msg(
        &self,
        payload: OpenChannelRequest,
    ) -> Result<Txid, Self::Error>;

    async fn handle_close_channels_with_peer_msg(
        &self,
        payload: CloseChannelsWithPeerRequest,
    ) -> Result<CloseChannelsWithPeerResponse, Self::Error>;

    async fn handle_get_balances_msg(&self) -> Result<GatewayBalances, Self::Error>;

    async fn handle_send_onchain_msg(
        &self,
        payload: SendOnchainRequest,
    ) -> Result<Txid, Self::Error>;

    async fn handle_get_ln_onchain_address_msg(&self) -> Result<Address, Self::Error>;

    async fn handle_deposit_address_msg(
        &self,
        payload: DepositAddressPayload,
    ) -> Result<Address, Self::Error>;

    async fn handle_receive_ecash_msg(
        &self,
        payload: ReceiveEcashPayload,
    ) -> Result<ReceiveEcashResponse, Self::Error>;

    async fn handle_create_invoice_for_operator_msg(
        &self,
        payload: CreateInvoiceForOperatorPayload,
    ) -> Result<Bolt11Invoice, Self::Error>;

    async fn handle_pay_invoice_for_operator_msg(
        &self,
        payload: PayInvoiceForOperatorPayload,
    ) -> Result<Preimage, Self::Error>;

    async fn handle_list_transactions_msg(
        &self,
        payload: ListTransactionsPayload,
    ) -> Result<ListTransactionsResponse, Self::Error>;

    async fn handle_spend_ecash_msg(
        &self,
        payload: SpendEcashPayload,
    ) -> Result<SpendEcashResponse, Self::Error>;

    async fn handle_shutdown_msg(&self, task_group: TaskGroup) -> Result<(), Self::Error>;

    fn get_task_group(&self) -> TaskGroup;

    async fn handle_withdraw_msg(
        &self,
        payload: WithdrawPayload,
    ) -> Result<WithdrawResponse, Self::Error>;

    async fn handle_withdraw_preview_msg(
        &self,
        payload: WithdrawPreviewPayload,
    ) -> Result<WithdrawPreviewResponse, Self::Error>;

    async fn handle_payment_log_msg(
        &self,
        payload: PaymentLogPayload,
    ) -> Result<PaymentLogResponse, Self::Error>;

    async fn handle_export_invite_codes(&self) -> BTreeMap<FederationId, Vec<InviteCode>>;

    fn get_password_hash(&self) -> String;

    fn gatewayd_version(&self) -> String;

    async fn get_chain_source(&self) -> (ChainSource, Network);

    fn lightning_mode(&self) -> LightningMode;

    async fn is_configured(&self) -> bool;

    async fn handle_set_mnemonic_msg(&self, payload: SetMnemonicPayload)
    -> Result<(), Self::Error>;

    async fn handle_create_offer_for_operator_msg(
        &self,
        payload: CreateOfferPayload,
    ) -> Result<CreateOfferResponse, Self::Error>;

    async fn handle_pay_offer_for_operator_msg(
        &self,
        payload: PayOfferPayload,
    ) -> Result<PayOfferResponse, Self::Error>;
}

async fn login_form<E>(State(_state): State<UiState<DynGatewayApi<E>>>) -> impl IntoResponse {
    login_form_response("Fedimint Gateway Login")
}

// Dashboard login submit handler
async fn login_submit<E>(
    State(state): State<UiState<DynGatewayApi<E>>>,
    jar: CookieJar,
    Form(input): Form<LoginInput>,
) -> impl IntoResponse {
    if let Ok(verify) = bcrypt::verify(input.password, &state.api.get_password_hash())
        && verify
    {
        let mut cookie = Cookie::new(state.auth_cookie_name.clone(), state.auth_cookie_value);
        cookie.set_path(ROOT_ROUTE);

        cookie.set_http_only(true);
        cookie.set_same_site(Some(SameSite::Lax));

        let jar = jar.add(cookie);
        return (jar, Redirect::to(ROOT_ROUTE)).into_response();
    }

    let content = html! {
        div class="alert alert-danger" { "The password is invalid" }
        div class="button-container" {
            a href=(LOGIN_ROUTE) class="btn btn-primary setup-btn" { "Return to Login" }
        }
    };

    Html(login_layout("Login Failed", content).into_string()).into_response()
}

async fn dashboard_view<E>(
    State(state): State<UiState<DynGatewayApi<E>>>,
    _auth: UserAuth,
    Query(msg): Query<DashboardQuery>,
) -> impl IntoResponse
where
    E: std::fmt::Display,
{
    // If gateway is not configured, show setup view instead of dashboard
    if !state.api.is_configured().await {
        return setup::setup_view(State(state), Query(msg))
            .await
            .into_response();
    }

    let gatewayd_version = state.api.gatewayd_version();
    debug!(target: LOG_GATEWAY_UI, "Getting gateway info...");
    let gateway_info = match state.api.handle_get_info().await {
        Ok(info) => info,
        Err(err) => {
            let content = html! {
                div class="alert alert-danger mt-4" {
                    strong { "Failed to fetch gateway info: " }
                    (err.to_string())
                }
            };
            return Html(
                dashboard_layout(content, "Fedimint Gateway UI", Some(&gatewayd_version))
                    .into_string(),
            )
            .into_response();
        }
    };

    let content = html! {

       (federation::scripts())

        @if let Some(success) = msg.success {
            div class="alert alert-success mt-2 d-flex justify-content-between align-items-center" {
                span {
                    (success)
                    @if msg.show_export_reminder.unwrap_or(false) {
                        " "
                        a href=(EXPORT_INVITE_CODES_ROUTE) { "Export your invite codes for backup." }
                    }
                }
                a href=(ROOT_ROUTE)
                class="ms-3 text-decoration-none text-dark fw-bold"
                style="font-size: 1.5rem; line-height: 1; cursor: pointer;"
                { "×" }
            }
        }
        @if let Some(error) = msg.ui_error {
            div class="alert alert-danger mt-2 d-flex justify-content-between align-items-center" {
                span { (error) }
                a href=(ROOT_ROUTE)
                class="ms-3 text-decoration-none text-dark fw-bold"
                style="font-size: 1.5rem; line-height: 1; cursor: pointer;"
                { "×" }
            }
        }

        div class="row mt-4" {
            div class="col-md-12 text-end" {
                a href=(EXPORT_INVITE_CODES_ROUTE) class="btn btn-outline-primary me-2" {
                    "Export Invite Codes"
                }
                form action=(STOP_GATEWAY_ROUTE) method="post" style="display: inline;" {
                    button class="btn btn-outline-danger" type="submit"
                        onclick="return confirm('Are you sure you want to safely stop the gateway? The gateway will wait for outstanding payments and then shutdown.');"
                    {
                        "Safely Stop Gateway"
                    }
                }
            }
        }

        div class="row gy-4" {
            div class="col-md-6" {
                (general::render(&gateway_info))
            }
            div class="col-md-6" {
                (payment_summary::render(&state.api, &gateway_info.federations).await)
            }
        }

        div class="row gy-4 mt-2" {
            div class="col-md-6" {
                (bitcoin::render(&state.api).await)
            }
            div class="col-md-6" {
                (mnemonic::render())
            }
        }

        div class="row gy-4 mt-2" {
            div class="col-md-12" {
                (lightning::render(&gateway_info, &state.api).await)
            }
        }

        div class="row gy-4 mt-2" {
            div class="col-md-12" {
                (connect_fed::render(&gateway_info.gateway_state))
            }
        }

        @for fed in gateway_info.federations {
            (federation::render(&fed))
        }
    };

    Html(dashboard_layout(content, "Fedimint Gateway UI", Some(&gatewayd_version)).into_string())
        .into_response()
}

async fn stop_gateway_handler<E>(
    State(state): State<UiState<DynGatewayApi<E>>>,
    _auth: UserAuth,
) -> impl IntoResponse
where
    E: std::fmt::Display,
{
    match state
        .api
        .handle_shutdown_msg(state.api.get_task_group())
        .await
    {
        Ok(_) => redirect_success("Gateway is safely shutting down...".to_string()).into_response(),
        Err(err) => redirect_error(format!("Failed to stop gateway: {err}")).into_response(),
    }
}

async fn export_invite_codes_handler<E>(
    State(state): State<UiState<DynGatewayApi<E>>>,
    _auth: UserAuth,
) -> impl IntoResponse
where
    E: std::fmt::Display,
{
    let invite_codes = state.api.handle_export_invite_codes().await;
    let json = match serde_json::to_string_pretty(&invite_codes) {
        Ok(json) => json,
        Err(err) => {
            return Response::builder()
                .status(500)
                .body(Body::from(format!(
                    "Failed to serialize invite codes: {err}"
                )))
                .expect("Failed to build error response");
        }
    };
    let filename = "gateway-invite-codes.json";

    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .body(Body::from(json))
        .expect("Failed to build response")
}

async fn import_invite_codes_handler<E>(
    State(state): State<UiState<DynGatewayApi<E>>>,
    _auth: UserAuth,
    mut multipart: axum::extract::Multipart,
) -> impl IntoResponse
where
    E: std::fmt::Display,
{
    const MAX_FILE_SIZE: usize = 1024 * 1024; // 1MB

    // Check gateway is in Running state
    let gateway_info = match state.api.handle_get_info().await {
        Ok(info) => info,
        Err(err) => {
            return redirect_error(format!("Failed to get gateway info: {err}")).into_response();
        }
    };

    if gateway_info.gateway_state != "Running" {
        return redirect_error(
            "Gateway must be in Running state to recover federations. Please wait for the gateway to finish starting up.".to_string(),
        )
        .into_response();
    }

    // Extract file from multipart
    let field = match multipart.next_field().await {
        Ok(Some(field)) => field,
        Ok(None) => {
            return redirect_error("No file uploaded".to_string()).into_response();
        }
        Err(err) => {
            return redirect_error(format!("Failed to read uploaded file: {err}")).into_response();
        }
    };

    // Get file data
    let data = match field.bytes().await {
        Ok(data) => data,
        Err(err) => {
            return redirect_error(format!("Failed to read file data: {err}")).into_response();
        }
    };

    // Check file size
    if data.len() > MAX_FILE_SIZE {
        return redirect_error(format!(
            "File too large. Maximum size is {}MB.",
            MAX_FILE_SIZE / (1024 * 1024)
        ))
        .into_response();
    }

    // Parse JSON
    let invite_codes: BTreeMap<FederationId, Vec<InviteCode>> = match serde_json::from_slice(&data)
    {
        Ok(codes) => codes,
        Err(err) => {
            return redirect_error(format!(
                "Failed to parse JSON file. Expected format: BTreeMap<FederationId, Vec<InviteCode>>. Error: {err}"
            ))
            .into_response();
        }
    };

    if invite_codes.is_empty() {
        return redirect_error("No federations found in the uploaded file".to_string())
            .into_response();
    }

    // Process each federation
    let mut recovered = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();

    for (federation_id, codes) in invite_codes {
        if codes.is_empty() {
            failed.push((federation_id, "No invite codes available".to_string()));
            continue;
        }

        // Check if federation already exists
        let already_joined = gateway_info
            .federations
            .iter()
            .any(|f| f.federation_id == federation_id);

        if already_joined {
            skipped.push(federation_id);
            continue;
        }

        // Try to join with the first invite code and recover=true
        let invite_code_str = codes[0].to_string();
        let payload = ConnectFedPayload {
            invite_code: invite_code_str,
            use_tor: None,
            recover: Some(true),
        };

        match state.api.handle_connect_federation(payload).await {
            Ok(info) => {
                recovered.push((federation_id, info.federation_name));
            }
            Err(err) => {
                failed.push((federation_id, err.to_string()));
            }
        }
    }

    // Build result message
    let total = recovered.len() + skipped.len() + failed.len();
    let mut msg_parts = vec![format!(
        "Processed {} of {} federations.",
        recovered.len() + failed.len(),
        total
    )];

    if !recovered.is_empty() {
        let names: Vec<String> = recovered
            .iter()
            .map(|(id, name)| {
                name.as_ref()
                    .map(|n| format!("{} ({})", n, id))
                    .unwrap_or_else(|| id.to_string())
            })
            .collect();
        msg_parts.push(format!("Recovered: {}", names.join(", ")));
    }

    if !skipped.is_empty() {
        let ids: Vec<String> = skipped.iter().map(|id| id.to_string()).collect();
        msg_parts.push(format!("Skipped (already joined): {}", ids.join(", ")));
    }

    if !failed.is_empty() {
        let failures: Vec<String> = failed
            .iter()
            .map(|(id, err)| format!("{} (error: {})", id, err))
            .collect();
        msg_parts.push(format!("Failed: {}", failures.join(", ")));
    }

    let message = msg_parts.join(" ");

    if failed.is_empty() {
        redirect_success_with_export_reminder(message).into_response()
    } else {
        redirect_error(message).into_response()
    }
}

pub fn router<E: Display + Send + Sync + std::fmt::Debug + 'static>(
    api: DynGatewayApi<E>,
) -> Router {
    let app = Router::new()
        .route(ROOT_ROUTE, get(dashboard_view))
        .route(LOGIN_ROUTE, get(login_form).post(login_submit))
        .route(OPEN_CHANNEL_ROUTE, post(open_channel_handler))
        .route(CLOSE_CHANNEL_ROUTE, post(close_channel_handler))
        .route(CHANNEL_FRAGMENT_ROUTE, get(channels_fragment_handler))
        .route(WALLET_FRAGMENT_ROUTE, get(wallet_fragment_handler))
        .route(LEAVE_FEDERATION_ROUTE, post(leave_federation_handler))
        .route(CONNECT_FEDERATION_ROUTE, post(connect_federation_handler))
        .route(SET_FEES_ROUTE, post(set_fees_handler))
        .route(SEND_ONCHAIN_ROUTE, post(send_onchain_handler))
        .route(
            LN_ONCHAIN_ADDRESS_ROUTE,
            get(generate_receive_address_handler),
        )
        .route(DEPOSIT_ADDRESS_ROUTE, post(deposit_address_handler))
        .route(SPEND_ECASH_ROUTE, post(spend_ecash_handler))
        .route(RECEIVE_ECASH_ROUTE, post(receive_ecash_handler))
        .route(PAYMENTS_FRAGMENT_ROUTE, get(payments_fragment_handler))
        .route(
            CREATE_BOLT11_INVOICE_ROUTE,
            post(create_bolt11_invoice_handler),
        )
        .route(
            CREATE_RECEIVE_INVOICE_ROUTE,
            post(create_receive_invoice_handler),
        )
        .route(PAY_BOLT11_INVOICE_ROUTE, post(pay_bolt11_invoice_handler))
        .route(PAY_UNIFIED_ROUTE, post(pay_unified_handler))
        .route(DETECT_PAYMENT_TYPE_ROUTE, post(detect_payment_type_handler))
        .route(
            TRANSACTIONS_FRAGMENT_ROUTE,
            get(transactions_fragment_handler),
        )
        .route(STOP_GATEWAY_ROUTE, post(stop_gateway_handler))
        .route(EXPORT_INVITE_CODES_ROUTE, get(export_invite_codes_handler))
        .route(IMPORT_INVITE_CODES_ROUTE, post(import_invite_codes_handler))
        .route(WITHDRAW_PREVIEW_ROUTE, post(withdraw_preview_handler))
        .route(WITHDRAW_CONFIRM_ROUTE, post(withdraw_confirm_handler))
        .route(PAYMENT_LOG_ROUTE, get(payment_log_fragment_handler))
        .route(CREATE_WALLET_ROUTE, post(create_wallet_handler))
        .route(
            RECOVER_WALLET_ROUTE,
            get(recover_wallet_form).post(recover_wallet_handler),
        )
        .route(
            MNEMONIC_IFRAME_ROUTE,
            get(mnemonic_iframe_handler).post(mnemonic_reveal_handler),
        )
        .with_static_routes();

    app.with_state(UiState::new(api))
}
