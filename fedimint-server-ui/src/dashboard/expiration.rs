use axum::extract::{Form, State};
use axum::response::{Html, IntoResponse, Redirect};
use chrono::{Datelike, Months, Utc};
use fedimint_core::admin_client::ExpirationStatus;
use fedimint_core::invite_code::InviteCode;
use fedimint_server_core::dashboard_ui::DynDashboardApi;
use fedimint_ui_common::auth::UserAuth;
use fedimint_ui_common::{ROOT_ROUTE, UiState, dashboard_layout};
use maud::{Markup, html};
use serde::Deserialize;

use crate::{CLEAR_EXPIRATION_ROUTE, SET_EXPIRATION_ROUTE};

#[derive(Debug, Deserialize)]
pub struct ExpirationForm {
    pub expiration_date: String,
    pub successor_invite_code: Option<String>,
}

pub fn render(status: Option<&ExpirationStatus>) -> Markup {
    html! {
        div class="card h-100" {
            div class="card-header dashboard-header" { "Federation Expiration" }
            div class="card-body" {
                @if let Some(status) = status {
                    div class="alert alert-info" {
                        strong { "Expiration Announced" }
                        @if let Some(date) = chrono::DateTime::from_timestamp(status.date as i64, 0) {
                            strong { " - " (date.format("%B %-d, %Y")) }
                        }
                        @if let Some(ref successor) = status.successor {
                            p class="mb-0 mt-2" { (successor.to_string()) }
                        }
                    }
                    form method="post" action=(CLEAR_EXPIRATION_ROUTE) {
                        button type="submit" class="btn btn-primary" {
                            "Clear Expiration Announcement"
                        }
                    }
                } @else {
                    div class="alert alert-warning" {
                        "All guardians have to enter the exact same values for an expiration status."
                    }
                    form method="post" action=(SET_EXPIRATION_ROUTE) {
                        div class="form-group mb-3" {
                            select class="form-select" id="expiration_date" name="expiration_date" required {
                                option value="" selected disabled { "Select Expiration Date" }
                                @let now = Utc::now();
                                @for i in 1..=12u32 {
                                    @let last_day = now.date_naive()
                                        .with_day(1).expect("day 1 is always valid")
                                        .checked_add_months(Months::new(i + 1))
                                        .expect("adding months to current date can't overflow")
                                        .pred_opt()
                                        .expect("predecessor of first of month is always valid");
                                    @let timestamp = last_day
                                        .and_hms_opt(0, 0, 0).expect("midnight is always valid")
                                        .and_utc()
                                        .timestamp();
                                    option value=(timestamp) {
                                        (last_day.format("%B %-d, %Y"))
                                    }
                                }
                            }
                        }
                        div class="form-group mb-3" {
                            input
                                type="text"
                                class="form-control"
                                id="successor_invite_code"
                                name="successor_invite_code"
                                placeholder="Enter Optional Invite Code";
                        }
                        button type="submit" class="btn btn-primary" {
                            "Announce Expiration"
                        }
                    }
                }
            }
        }
    }
}

pub async fn post_set_expiration(
    State(state): State<UiState<DynDashboardApi>>,
    _auth: UserAuth,
    Form(form): Form<ExpirationForm>,
) -> impl IntoResponse {
    let date = form
        .expiration_date
        .parse::<u64>()
        .expect("date values are generated server-side");

    let invite_input = form.successor_invite_code.filter(|s| !s.trim().is_empty());

    let successor = match &invite_input {
        Some(s) => match s.parse::<InviteCode>() {
            Ok(code) => Some(code),
            Err(_) => {
                let content = html! {
                    div class="alert alert-danger" { "Invalid invite code format" }
                    div class="button-container" {
                        a href="/" class="btn btn-primary" { "Return to Dashboard" }
                    }
                };
                return Html(dashboard_layout(content, "Invalid Invite Code", None).into_string())
                    .into_response();
            }
        },
        None => None,
    };

    state
        .api
        .set_expiration_status(Some(ExpirationStatus { date, successor }))
        .await;

    Redirect::to(ROOT_ROUTE).into_response()
}

pub async fn post_clear_expiration(
    State(state): State<UiState<DynDashboardApi>>,
    _auth: UserAuth,
) -> impl IntoResponse {
    state.api.set_expiration_status(None).await;

    Redirect::to(ROOT_ROUTE)
}
