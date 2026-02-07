use axum::body::Body;
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use fedimint_server_core::dashboard_ui::DynDashboardApi;
use fedimint_ui_common::UiState;
use fedimint_ui_common::auth::UserAuth;
use maud::{Markup, html};

use crate::DOWNLOAD_BACKUP_ROUTE;

pub fn render() -> Markup {
    html! {
        div class="card h-100" {
            div class="card-header dashboard-header" { "Guardian Configuration Backup" }
            div class="card-body" {
                div class="alert alert-warning" {
                    "This is a static backup, you only need to download it once. You can use it to restore your guardian if your server fails. Store this file securely since anyone with it and your password can run your guardian node."
                }
                a href=(DOWNLOAD_BACKUP_ROUTE) class="btn btn-primary" {
                    "Download Guardian Backup"
                }
            }
        }
    }
}

pub async fn download(
    State(state): State<UiState<DynDashboardApi>>,
    user_auth: UserAuth,
) -> impl IntoResponse {
    let api_auth = state.api.auth().await;

    let backup = state
        .api
        .download_guardian_config_backup(&api_auth.0, &user_auth.guardian_auth_token)
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
