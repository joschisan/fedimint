use std::sync::Arc;

use axum::Router;
use axum::extract::{Multipart, State};
use axum::response::{Html, IntoResponse, Redirect};
use axum::routing::{get, post};
use axum_extra::extract::Form;
use axum_extra::extract::cookie::CookieJar;
use maud::{Markup, PreEscaped, html};
use picomint_core::module::ApiAuth;
use qrcode::QrCode;
use serde::Deserialize;

use crate::config::ServerConfig;
use crate::config::setup::SetupApi;
use crate::ui::assets::WithStaticRoutesExt;
use crate::ui::auth::UserAuth;
use crate::ui::{
    LOGIN_ROUTE, LoginInput, ROOT_ROUTE, UiState, copiable_text, login_form, login_submit_response,
    single_card_layout,
};

// Setup route constants
pub const FEDERATION_SETUP_ROUTE: &str = "/federation_setup";
pub const ADD_SETUP_CODE_ROUTE: &str = "/add_setup_code";
pub const RESET_SETUP_CODES_ROUTE: &str = "/reset_setup_codes";
pub const START_DKG_ROUTE: &str = "/start_dkg";
pub const RESTORE_CONFIG_ROUTE: &str = "/restore_config";
pub const RECOVER_PAGE_ROUTE: &str = "/recover";

#[derive(Debug, Deserialize)]
pub(crate) struct SetupInput {
    pub name: String,
    #[serde(default)]
    pub is_lead: bool,
    pub federation_name: String,
    #[serde(default)]
    pub federation_size: String,
}

#[derive(Debug, Deserialize)]
pub(crate) struct PeerInfoInput {
    pub peer_info: String,
}

fn peer_list_section(
    connected_peers: &[String],
    federation_size: Option<u32>,
    cfg_federation_name: &Option<String>,
    error: Option<&str>,
) -> Markup {
    let total_guardians = connected_peers.len() + 1;
    let can_start_dkg = federation_size
        .is_some_and(|expected| total_guardians == expected as usize);

    html! {
        div id="peer-list-section" {
            @if let Some(expected) = federation_size {
                p { (format!("{total_guardians} of {expected} guardians connected.")) }
            } @else {
                p { "Add setup code for every other guardian." }
            }

            @if !connected_peers.is_empty() {
                ul class="list-group mb-2" {
                    @for peer in connected_peers {
                        li class="list-group-item" { (peer) }
                    }
                }

                form id="reset-form" method="post" action=(RESET_SETUP_CODES_ROUTE) class="d-none" {}
                div class="text-center mb-4" {
                    button type="button" class="btn btn-link text-danger text-decoration-none p-0" onclick="if(confirm('Are you sure you want to reset all guardians?')){document.getElementById('reset-form').submit();}" {
                        "Reset Guardians"
                    }
                }
            }

            @if can_start_dkg {
                @let has_settings = cfg_federation_name.is_some() || federation_size.is_some();

                form id="start-dkg-form" hx-post=(START_DKG_ROUTE) hx-target="#peer-list-section" hx-swap="outerHTML" {
                    @if let Some(error) = error {
                        div class="alert alert-danger mb-3" { (error) }
                    }
                    button type="submit" class="btn btn-warning w-100 py-2" { "Confirm" }
                }

                @if has_settings {
                    p class="text-muted mt-3 mb-0" style="font-size: 0.85rem;" {
                        @if let Some(name) = cfg_federation_name {
                            (name) " federation has been configured"
                        } @else {
                            "The federation has been configured"
                        }
                        "."
                    }
                }
            } @else {
                form id="add-setup-code-form" hx-post=(ADD_SETUP_CODE_ROUTE) hx-target="#peer-list-section" hx-swap="outerHTML" {
                    div class="mb-3" {
                        div class="input-group" {
                            input type="text" class="form-control" id="peer_info" name="peer_info"
                                placeholder="Paste Setup Code" required;
                            button type="button" class="btn btn-outline-secondary" onclick="startQrScanner()" title="Scan QR Code" {
                                i class="bi bi-qr-code-scan" {}
                            }
                        }
                    }

                    @if let Some(error) = error {
                        div class="alert alert-danger mb-3" { (error) }
                    }
                    button type="submit" class="btn btn-primary w-100 py-2" { "Add Guardian" }
                }
            }
        }
    }
}

fn restore_form_content(error: Option<&str>) -> Markup {
    html! {
        form id="restore-form"
            hx-post=(RESTORE_CONFIG_ROUTE)
            hx-encoding="multipart/form-data"
            hx-target="#restore-form"
            hx-swap="outerHTML"
        {
            div class="alert alert-info mb-3" {
                "Upload your saved server config to recover."
            }

            div class="form-group mb-3" {
                input type="file" class="form-control" id="config_file" name="config_file"
                    accept="application/json" required;
            }

            @if let Some(error) = error {
                div class="alert alert-danger mb-3" { (error) }
            }

            button type="submit" class="btn btn-outline-primary w-100 py-2" { "Recover from Config" }
        }
    }
}

fn setup_form_content(error: Option<&str>) -> Markup {
    html! {
        form id="setup-form" hx-post=(ROOT_ROUTE) hx-target="#setup-form" hx-swap="outerHTML" {
            style {
                r#"
                .toggle-content {
                    display: none;
                }

                .toggle-control:checked ~ .toggle-content {
                    display: block;
                }
                "#
            }

            div class="form-group mb-4" {
                input type="text" class="form-control" id="name" name="name" placeholder="Your Guardian Name" required;
            }

            div class="alert alert-warning mb-3" style="font-size: 0.875rem;" {
                "Exactly one guardian must set the global config."
            }

            div class="form-group mb-4" {
                input type="checkbox" class="form-check-input toggle-control" id="is_lead" name="is_lead" value="true";

                label class="form-check-label ms-2" for="is_lead" {
                    "Set the global config"
                }

                div class="toggle-content mt-3" {
                    input type="text" class="form-control" id="federation_name" name="federation_name" placeholder="Federation Name";

                    div class="form-group mt-3" {
                        label class="form-label d-block" {
                            "Total number of guardians (including you)"
                        }
                        @for size in [4u32, 7, 10, 13, 16, 19] {
                            div class="form-check form-check-inline" {
                                input type="radio" class="form-check-input"
                                    id=(format!("federation_size_{size}"))
                                    name="federation_size"
                                    value=(size.to_string())
                                    required;
                                label class="form-check-label" for=(format!("federation_size_{size}")) {
                                    (size.to_string())
                                }
                            }
                        }
                    }
                }
            }

            @if let Some(error) = error {
                div class="alert alert-danger mb-3" { (error) }
            }
            button type="submit" class="btn btn-primary w-100 py-2" { "Confirm" }

            div class="text-center mt-3" {
                a href=(RECOVER_PAGE_ROUTE) class="text-decoration-none" {
                    "Recover from Config"
                }
            }
        }
    }
}

// GET handler for the /setup route (display the setup form)
async fn setup_form(State(state): State<UiState<Arc<SetupApi>>>) -> impl IntoResponse {
    if state.api.setup_code().await.is_some() {
        return Redirect::to(FEDERATION_SETUP_ROUTE).into_response();
    }

    Html(single_card_layout("Guardian Setup", setup_form_content(None)).into_string())
        .into_response()
}

// GET handler for the /recover route (dedicated page for restoring from a
// previously-saved server config).
async fn recover_page(State(state): State<UiState<Arc<SetupApi>>>) -> impl IntoResponse {
    if state.api.setup_code().await.is_some() {
        return Redirect::to(FEDERATION_SETUP_ROUTE).into_response();
    }

    Html(single_card_layout("Recover from Config", restore_form_content(None)).into_string())
        .into_response()
}

// POST handler for the /setup route (process the setup form)
async fn setup_submit(
    State(state): State<UiState<Arc<SetupApi>>>,
    Form(input): Form<SetupInput>,
) -> impl IntoResponse {
    // Only use these settings if is_lead is true
    let federation_name = if input.is_lead {
        Some(input.federation_name)
    } else {
        None
    };

    let federation_size = if input.is_lead {
        let s = input.federation_size.trim();
        if s.is_empty() {
            None
        } else {
            match s.parse::<u32>() {
                Ok(size) => Some(size),
                Err(_) => {
                    return Html(setup_form_content(Some("Invalid federation size")).into_string())
                        .into_response();
                }
            }
        }
    } else {
        None
    };

    match state
        .api
        .set_local_parameters(input.name, federation_name, federation_size)
        .await
    {
        Ok(_) => (
            [("HX-Redirect", FEDERATION_SETUP_ROUTE)],
            Html(String::new()),
        )
            .into_response(),
        Err(e) => Html(setup_form_content(Some(&e.to_string())).into_string()).into_response(),
    }
}

// GET handler for the /login route (display the login form)
async fn login_form_handler(State(state): State<UiState<Arc<SetupApi>>>) -> impl IntoResponse {
    if state.api.setup_code().await.is_none() {
        return Redirect::to(ROOT_ROUTE).into_response();
    }

    Html(single_card_layout("Enter Password", login_form(None)).into_string()).into_response()
}

// POST handler for the /login route (authenticate and set session cookie)
async fn login_submit(
    State(state): State<UiState<Arc<SetupApi>>>,
    jar: CookieJar,
    Form(input): Form<LoginInput>,
) -> impl IntoResponse {
    login_submit_response(
        state.auth.clone(),
        state.auth_cookie_name,
        state.auth_cookie_value,
        jar,
        input,
    )
    .into_response()
}

// GET handler for the /federation-setup route (main federation management page)
async fn federation_setup(
    State(state): State<UiState<Arc<SetupApi>>>,
    _auth: UserAuth,
) -> impl IntoResponse {
    let our_connection_info = state
        .api
        .setup_code()
        .await
        .expect("Successful authentication ensures that the local parameters have been set");

    let connected_peers = state.api.connected_peers().await;
    let federation_size = state.api.federation_size().await;
    let cfg_federation_name = state.api.cfg_federation_name().await;

    let content = html! {
        p { "Share this with your fellow guardians." }

        @let qr_svg = QrCode::new(&our_connection_info)
            .expect("Failed to generate QR code")
            .render::<qrcode::render::svg::Color>()
            .build();

        div class="text-center mb-3" {
            div class="border rounded p-2 bg-white d-inline-block" style="width: 250px; max-width: 100%;" {
                div style="width: 100%; height: auto; overflow: hidden;" {
                    (PreEscaped(format!(r#"<div style="width: 100%; height: auto;">{}</div>"#,
                        qr_svg.replace("width=", "data-width=")
                              .replace("height=", "data-height=")
                              .replace("<svg", r#"<svg style="width: 100%; height: auto; display: block;""#))))
                }
            }
        }

        div class="mb-4" {
            (copiable_text(&our_connection_info))
        }

        (peer_list_section(&connected_peers, federation_size, &cfg_federation_name, None))

        // QR Scanner Modal
        div class="modal fade" id="qrScannerModal" tabindex="-1" aria-labelledby="qrScannerModalLabel" aria-hidden="true" {
            div class="modal-dialog modal-dialog-centered" {
                div class="modal-content" {
                    div class="modal-header" {
                        h5 class="modal-title" id="qrScannerModalLabel" { "Scan Setup Code" }
                        button type="button" class="btn-close" data-bs-dismiss="modal" aria-label="Close" {}
                    }
                    div class="modal-body" {
                        div id="qr-reader" style="width: 100%;" {}
                        div id="qr-reader-error" class="alert alert-danger mt-3 d-none" {}
                    }
                    div class="modal-footer" {
                        button type="button" class="btn btn-secondary" data-bs-dismiss="modal" { "Cancel" }
                    }
                }
            }
        }

        script src="/assets/html5-qrcode.min.js" {}

        script {
            (PreEscaped(r#"
            var html5QrCode = null;
            var qrScannerModal = null;

            function startQrScanner() {
                if (typeof window.picomintQrScannerOverride === 'function') {
                    window.picomintQrScannerOverride(function(result) {
                        if (result) {
                            document.getElementById('peer_info').value = result;
                        }
                    });
                    return;
                }

                var modalEl = document.getElementById('qrScannerModal');
                qrScannerModal = new bootstrap.Modal(modalEl);

                var errorEl = document.getElementById('qr-reader-error');
                errorEl.classList.add('d-none');
                errorEl.textContent = '';

                qrScannerModal.show();

                modalEl.addEventListener('shown.bs.modal', function onShown() {
                    modalEl.removeEventListener('shown.bs.modal', onShown);
                    initializeScanner();
                });

                modalEl.addEventListener('hidden.bs.modal', function onHidden() {
                    modalEl.removeEventListener('hidden.bs.modal', onHidden);
                    stopQrScanner();
                });
            }

            function initializeScanner() {
                html5QrCode = new Html5Qrcode("qr-reader");

                var config = {
                    fps: 10,
                    qrbox: { width: 250, height: 250 },
                    aspectRatio: 1.0
                };

                html5QrCode.start(
                    { facingMode: "environment" },
                    config,
                    function(decodedText, decodedResult) {
                        document.getElementById('peer_info').value = decodedText;
                        qrScannerModal.hide();
                    },
                    function(errorMessage) {
                    }
                ).catch(function(err) {
                    var errorEl = document.getElementById('qr-reader-error');
                    errorEl.textContent = 'Unable to access camera: ' + err;
                    errorEl.classList.remove('d-none');
                });
            }

            function stopQrScanner() {
                if (html5QrCode && html5QrCode.isScanning) {
                    html5QrCode.stop().catch(function(err) {
                        console.error('Error stopping scanner:', err);
                    });
                }
            }
            "#))
        }
    };

    Html(single_card_layout("Federation Setup", content).into_string()).into_response()
}

async fn post_add_setup_code(
    State(state): State<UiState<Arc<SetupApi>>>,
    _auth: UserAuth,
    Form(input): Form<PeerInfoInput>,
) -> impl IntoResponse {
    let error = state.api.add_peer_setup_code(input.peer_info).await.err();

    let connected_peers = state.api.connected_peers().await;
    let federation_size = state.api.federation_size().await;
    let cfg_federation_name = state.api.cfg_federation_name().await;

    Html(
        peer_list_section(
            &connected_peers,
            federation_size,
            &cfg_federation_name,
            error.as_ref().map(std::string::ToString::to_string).as_deref(),
        )
        .into_string(),
    )
    .into_response()
}

async fn post_start_dkg(
    State(state): State<UiState<Arc<SetupApi>>>,
    _auth: UserAuth,
) -> impl IntoResponse {
    let our_connection_info = state.api.setup_code().await;

    match state.api.start_dkg().await {
        Ok(()) => {
            let content = html! {
                @if let Some(ref info) = our_connection_info {
                    p { "Share with guardians who still need it." }
                    div class="mb-4" {
                        (copiable_text(info))
                    }
                }

                div class="alert alert-info mb-3" {
                    "All guardians need to confirm their settings. Once completed you will be redirected to the Dashboard."
                }

                div
                    hx-get=(ROOT_ROUTE)
                    hx-trigger="every 2s"
                    hx-swap="none"
                    hx-on--after-request={
                        "if (event.detail.xhr.status === 200) { window.location.href = '" (ROOT_ROUTE) "'; }"
                    }
                    style="display: none;"
                {}

                div class="text-center mt-4" {
                    div class="spinner-border text-primary" role="status" {
                        span class="visually-hidden" { "Loading..." }
                    }
                    p class="mt-2 text-muted" { "Waiting for federation setup to complete..." }
                }
            };

            (
                [("HX-Retarget", "body"), ("HX-Reswap", "innerHTML")],
                Html(single_card_layout("DKG Started", content).into_string()),
            )
                .into_response()
        }
        Err(e) => {
            let connected_peers = state.api.connected_peers().await;
            let federation_size = state.api.federation_size().await;
            let cfg_federation_name = state.api.cfg_federation_name().await;

            Html(
                peer_list_section(
                    &connected_peers,
                    federation_size,
                    &cfg_federation_name,
                    Some(&e.to_string()),
                )
                .into_string(),
            )
            .into_response()
        }
    }
}

async fn post_restore_config(
    State(state): State<UiState<Arc<SetupApi>>>,
    mut multipart: Multipart,
) -> impl IntoResponse {
    let bytes = match multipart.next_field().await {
        Ok(Some(field)) => match field.bytes().await {
            Ok(b) => b,
            Err(e) => {
                return Html(
                    restore_form_content(Some(&format!("Read failed: {e}"))).into_string(),
                )
                .into_response();
            }
        },
        Ok(None) => {
            return Html(restore_form_content(Some("No file uploaded")).into_string())
                .into_response();
        }
        Err(e) => {
            return Html(restore_form_content(Some(&format!("Upload failed: {e}"))).into_string())
                .into_response();
        }
    };

    let cfg: ServerConfig = match serde_json::from_slice(&bytes) {
        Ok(c) => c,
        Err(e) => {
            return Html(
                restore_form_content(Some(&format!("Invalid config JSON: {e}"))).into_string(),
            )
            .into_response();
        }
    };

    if let Err(e) = state.api.restore_config(cfg).await {
        return Html(restore_form_content(Some(&e.to_string())).into_string()).into_response();
    }

    let waiting = html! {
        div class="alert alert-info mb-3" {
            "Config restored. The guardian is rejoining the federation — you'll be redirected once it's back online."
        }

        div
            hx-get=(ROOT_ROUTE)
            hx-trigger="every 2s"
            hx-swap="none"
            hx-on--after-request={
                "if (event.detail.xhr.status === 200) { window.location.href = '" (ROOT_ROUTE) "'; }"
            }
            style="display: none;"
        {}

        div class="text-center mt-4" {
            div class="spinner-border text-primary" role="status" {
                span class="visually-hidden" { "Loading..." }
            }
            p class="mt-2 text-muted" { "Waiting for guardian to come online..." }
        }
    };

    (
        [("HX-Retarget", "body"), ("HX-Reswap", "innerHTML")],
        Html(single_card_layout("Restoring Config", waiting).into_string()),
    )
        .into_response()
}

async fn post_reset_setup_codes(
    State(state): State<UiState<Arc<SetupApi>>>,
    _auth: UserAuth,
) -> impl IntoResponse {
    state.api.reset_setup_codes().await;

    Redirect::to(FEDERATION_SETUP_ROUTE).into_response()
}

pub fn router(api: Arc<SetupApi>, auth: ApiAuth) -> Router {
    Router::new()
        .route(ROOT_ROUTE, get(setup_form).post(setup_submit))
        .route(LOGIN_ROUTE, get(login_form_handler).post(login_submit))
        .route(FEDERATION_SETUP_ROUTE, get(federation_setup))
        .route(ADD_SETUP_CODE_ROUTE, post(post_add_setup_code))
        .route(RESET_SETUP_CODES_ROUTE, post(post_reset_setup_codes))
        .route(START_DKG_ROUTE, post(post_start_dkg))
        .route(RESTORE_CONFIG_ROUTE, post(post_restore_config))
        .route(RECOVER_PAGE_ROUTE, get(recover_page))
        .with_static_routes()
        .with_state(UiState::new(api, auth))
}
