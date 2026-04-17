pub mod audit;
pub mod bitcoin;
pub mod general;
pub mod invite;
pub mod latency;
pub mod modules;

use std::sync::Arc;

use axum::Router;
use axum::extract::{Form, State};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, post};
use axum_extra::extract::cookie::CookieJar;
use maud::html;
use picomint_core::config::META_FEDERATION_NAME_KEY;

use crate::consensus::api::ConsensusApi;
use crate::ui::assets::WithStaticRoutesExt;
use crate::ui::auth::UserAuth;
use crate::ui::dashboard::modules::{ln, mint, wallet};
use crate::ui::{
    LOGIN_ROUTE, LoginInput, ROOT_ROUTE, UiState, dashboard_layout, login_form,
    login_submit_response, single_card_layout,
};

async fn login_form_handler() -> impl IntoResponse {
    Html(single_card_layout("Enter Password", login_form(None)).into_string())
}

async fn login_submit(
    State(state): State<UiState<Arc<ConsensusApi>>>,
    jar: CookieJar,
    Form(input): Form<LoginInput>,
) -> impl IntoResponse {
    login_submit_response(
        state.api.auth.clone(),
        state.auth_cookie_name,
        state.auth_cookie_value,
        jar,
        input,
    )
}

async fn dashboard_view(
    State(state): State<UiState<Arc<ConsensusApi>>>,
    _auth: UserAuth,
) -> impl IntoResponse {
    let api = &*state.api;

    let guardian_names: std::collections::BTreeMap<_, _> = api
        .cfg
        .consensus
        .iroh_endpoints
        .iter()
        .map(|(peer_id, endpoint)| (*peer_id, endpoint.name.clone()))
        .collect();
    let federation_name = api
        .cfg
        .consensus
        .meta
        .get(META_FEDERATION_NAME_KEY)
        .cloned()
        .expect("Federation name must be set");
    let session_count = api.session_count().await;
    let picomintd_version = api.code_version_str.clone();
    let consensus_ord_latency = *api.ord_latency_receiver.borrow();
    let p2p_connection_status: std::collections::BTreeMap<_, _> = api
        .p2p_status_receivers
        .iter()
        .map(|(peer, receiver)| (*peer, receiver.borrow().clone()))
        .collect();
    let invite_code = api.cfg.get_invite_code().to_string();
    let audit_summary = api.federation_audit().await;
    let bitcoin_rpc_url = api.bitcoin_rpc_connection.url();
    let bitcoin_rpc_status = api.bitcoin_rpc_connection.status();

    let content = html! {
        div class="row gy-4" {
            div class="col-md-6" {
                (general::render(&federation_name, session_count, &guardian_names))
            }

            div class="col-md-6" {
                (invite::render(&invite_code, session_count))
            }
        }

        div class="row gy-4 mt-2" {
            div class="col-lg-6" {
                (audit::render(&audit_summary))
            }

            div class="col-lg-6" {
                (latency::render(consensus_ord_latency, &p2p_connection_status))
            }
        }

        div class="row gy-4 mt-2" {
            div class="col-12" {
                (bitcoin::render(bitcoin_rpc_url, &bitcoin_rpc_status))
            }
        }

        div class="row gy-4 mt-2" {
            div class="col-12" {
                (ln::render(&api.server.ln).await)
            }
        }

        (wallet::render(&api.server.wallet).await)

        div class="row gy-4 mt-2" {
            div class="col-12" {
                (mint::render(&api.server.mint).await)
            }
        }
    };

    Html(dashboard_layout(content, &picomintd_version).into_string()).into_response()
}

pub fn router(api: Arc<ConsensusApi>) -> Router {
    Router::new()
        .route(ROOT_ROUTE, get(dashboard_view))
        .route(LOGIN_ROUTE, get(login_form_handler).post(login_submit))
        .route(ln::LN_ADD_ROUTE, post(ln::post_add))
        .route(ln::LN_REMOVE_ROUTE, post(ln::post_remove))
        .with_static_routes()
        .with_state(UiState::new(api))
}
