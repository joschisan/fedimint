pub mod audit;
pub mod bitcoin;
pub mod general;
pub mod invite;
pub mod latency;
pub mod modules;

use axum::Router;
use axum::body::Body;
use axum::extract::{Form, State};
use axum::http::header;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum_extra::extract::cookie::CookieJar;
use fedimint_ui_common::assets::WithStaticRoutesExt;
use fedimint_ui_common::auth::UserAuth;
use fedimint_ui_common::{
    CONNECTIVITY_CHECK_ROUTE, LOGIN_ROUTE, LoginInput, ROOT_ROUTE, UiState,
    connectivity_check_handler, dashboard_layout, login_form, login_submit_response,
    single_card_layout,
};
use maud::html;

use crate::dashboard::modules::{lnv2, mintv2, walletv2};
use crate::{DOWNLOAD_BACKUP_ROUTE, DynDashboardApi};

async fn login_form_handler() -> impl IntoResponse {
    Html(single_card_layout("Enter Password", login_form(None)).into_string())
}

async fn login_submit(
    State(state): State<UiState<DynDashboardApi>>,
    jar: CookieJar,
    Form(input): Form<LoginInput>,
) -> impl IntoResponse {
    login_submit_response(
        state.api.auth().await,
        state.auth_cookie_name,
        state.auth_cookie_value,
        jar,
        input,
    )
}

async fn download_backup(
    State(state): State<UiState<DynDashboardApi>>,
    user_auth: UserAuth,
) -> impl IntoResponse {
    let backup = state
        .api
        .download_guardian_config_backup(&user_auth.guardian_auth_token)
        .await;
    let filename = "guardian-backup.tar";

    Response::builder()
        .header(header::CONTENT_TYPE, "application/x-tar")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .body(Body::from(backup.tar_archive_bytes))
        .expect("Failed to build response")
}

async fn dashboard_view(
    State(state): State<UiState<DynDashboardApi>>,
    _auth: UserAuth,
) -> impl IntoResponse {
    let guardian_names = state.api.guardian_names().await;
    let federation_name = state.api.federation_name().await;
    let session_count = state.api.session_count().await;
    let fedimintd_version = state.api.fedimintd_version().await;
    let consensus_ord_latency = state.api.consensus_ord_latency().await;
    let p2p_connection_status = state.api.p2p_connection_status().await;
    let invite_code = state.api.federation_invite_code().await;
    let audit_summary = state.api.federation_audit().await;
    let bitcoin_rpc_url = state.api.bitcoin_rpc_url().await;
    let bitcoin_rpc_status = state.api.bitcoin_rpc_status().await;

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
                (lnv2::render(state.api.lightning()).await)
            }
        }

        (walletv2::render(state.api.wallet()).await)

        div class="row gy-4 mt-2" {
            div class="col-12" {
                (mintv2::render(state.api.mint()).await)
            }
        }

        div class="row gy-4 mt-2" {
            div class="col-lg-6" {
                div class="card h-100" {
                    div class="card-header dashboard-header" { "Guardian Backup" }
                    div class="card-body" {
                        div class="alert alert-warning mb-3" {
                            "You only need to download this backup once. Use it to restore your guardian if your server fails. Store this file securely since it contains your guardians private key for the onchain threshold signature protecting your funds."
                        }
                        a href="/download-backup" class="btn btn-primary" {
                            "Download"
                        }
                    }
                }
            }
        }
    };

    Html(dashboard_layout(content, &fedimintd_version).into_string()).into_response()
}

pub fn router(api: DynDashboardApi) -> Router {
    Router::new()
        .route(ROOT_ROUTE, get(dashboard_view))
        .route(LOGIN_ROUTE, get(login_form_handler).post(login_submit))
        .route(DOWNLOAD_BACKUP_ROUTE, get(download_backup))
        .route(
            CONNECTIVITY_CHECK_ROUTE,
            get(connectivity_check_handler::<DynDashboardApi>),
        )
        .route(lnv2::LNV2_ADD_ROUTE, post(lnv2::post_add))
        .route(lnv2::LNV2_REMOVE_ROUTE, post(lnv2::post_remove))
        .with_static_routes()
        .with_state(UiState::new(api))
}
