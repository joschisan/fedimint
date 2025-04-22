
use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;
use axum::routing::post;
use axum::{Json, Router, middleware};
use fedimint_core::module::ApiAuth;
use fedimint_server_core::setup_ui::DynSetupApi;
use serde::{Deserialize, Serialize};

pub const INIT_SETUP_ROUTE: &str = "/init_setup";
pub const ADD_SETUP_CODE_ROUTE: &str = "/add_setup_code";
pub const START_DKG_ROUTE: &str = "/start_dkg";
pub const RESET_SETUP_CODES_ROUTE: &str = "/reset_setup_codes";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InitSetupRequest {
    pub auth: ApiAuth,
    pub name: String,
    pub federation_name: Option<String>,
}

async fn init_setup(
    State(state): State<DynSetupApi>,
    Json(request): Json<InitSetupRequest>,
) -> Result<String, StatusCode> {
    state
        .init_setup(request.auth, request.name, request.federation_name)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)
}

async fn add_setup_code(
    State(state): State<DynSetupApi>,
    Json(code): Json<String>,
) -> Result<String, StatusCode> {
    state
        .add_setup_code(code)
        .await
        .map_err(|_| StatusCode::BAD_REQUEST)
}

async fn start_dkg(State(state): State<DynSetupApi>) -> Result<(), StatusCode> {
    state.start_dkg().await.map_err(|_| StatusCode::BAD_REQUEST)
}

async fn reset_setup_codes(State(state): State<DynSetupApi>) {
    state.reset_setup_codes().await;
}

fn extract_auth(request: &Request<Body>) -> Option<String> {
    request
        .headers()
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(std::string::ToString::to_string)
}

async fn auth_middleware(
    State(state): State<DynSetupApi>,
    request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if let Some(api_auth) = state.auth().await {
        if api_auth.0 != extract_auth(&request).ok_or(StatusCode::UNAUTHORIZED)? {
            return Err(StatusCode::UNAUTHORIZED);
        }
    }

    Ok(next.run(request).await)
}

pub fn router(setup_api: DynSetupApi) -> Router {
    Router::new()
        .route(INIT_SETUP_ROUTE, post(init_setup))
        .route(ADD_SETUP_CODE_ROUTE, post(add_setup_code))
        .route(START_DKG_ROUTE, post(start_dkg))
        .route(RESET_SETUP_CODES_ROUTE, post(reset_setup_codes))
        .layer(middleware::from_fn_with_state(
            setup_api.clone(),
            auth_middleware,
        ))
        .with_state(setup_api)
}
