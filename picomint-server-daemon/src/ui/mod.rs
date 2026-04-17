//! Server-side admin web UI.
//!
//! The UI runs in two phases:
//!
//! - Setup UI (before the federation is configured). Served by
//!   [`setup::router`] which takes an `Arc<SetupApi>` directly.
//! - Dashboard UI (once the federation is running). Served by
//!   [`dashboard::router`] which takes an `Arc<ConsensusApi>` and reaches
//!   straight into the three typed module instances (`mint`, `ln`, `wallet`)
//!   hanging off it.
//!
//! Previously this lived in two sibling crates (`picomint-server-ui` and
//! `picomint-ui-common`) and exposed a trait-indirection (`IDashboardApi` /
//! `ISetupApi` / `DynDashboardApi` / `DynSetupApi`) purely so those crates
//! could avoid a dep cycle back into the daemon. That cycle is gone now that
//! UI lives inside the daemon; we use the concrete types directly.

pub mod assets;
pub mod auth;
pub mod dashboard;
pub mod setup;

use axum::response::{Html, IntoResponse};
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::{Cookie, SameSite};
use maud::{DOCTYPE, Markup, PreEscaped, html};
use picomint_core::hex::ToHex;
use picomint_core::module::ApiAuth;
use picomint_core::secp256k1::rand::{Rng, thread_rng};
use serde::Deserialize;

// Common route constants shared between setup and dashboard.
pub const ROOT_ROUTE: &str = "/";
pub const LOGIN_ROUTE: &str = "/login";

/// Generic state wrapper for the setup and dashboard axum routers. Holds the
/// concrete API handle, the admin password (verified by `UserAuth`), and a
/// random per-process auth cookie pair used to gate authenticated routes.
#[derive(Clone)]
pub struct UiState<T> {
    pub api: T,
    pub auth: ApiAuth,
    pub auth_cookie_name: String,
    pub auth_cookie_value: String,
}

impl<T> UiState<T> {
    pub fn new(api: T, auth: ApiAuth) -> Self {
        Self {
            api,
            auth,
            auth_cookie_name: thread_rng().r#gen::<[u8; 4]>().encode_hex(),
            auth_cookie_value: thread_rng().r#gen::<[u8; 32]>().encode_hex(),
        }
    }
}

pub fn common_head(title: &str) -> Markup {
    html! {
        meta charset="utf-8";
        meta name="viewport" content="width=device-width, initial-scale=1.0";
        link rel="stylesheet" href="/assets/bootstrap.min.css" integrity="sha384-T3c6CoIi6uLrA9TneNEoa7RxnatzjcDSCmG1MXxSR1GAsXEV/Dwwykc2MPK8M2HN" crossorigin="anonymous";
        link rel="stylesheet" href="/assets/bootstrap-icons.min.css";
        link rel="stylesheet" type="text/css" href="/assets/style.css";
        link rel="icon" type="image/png" href="/assets/logo.png";

        // Note: this needs to be included in the header, so that web-page does not
        // get in a state where htmx is not yet loaded. `defer` helps with blocking the load.
        // Learned the hard way. --dpc
        script defer src="/assets/htmx.org-2.0.4.min.js" {}

        title { (title) }

        script {
            (PreEscaped(r#"
            function copyText(text, btn) {
                if (navigator.clipboard) {
                    navigator.clipboard.writeText(text).then(function() {
                        showCopied(btn);
                    });
                } else {
                    var ta = document.createElement('textarea');
                    ta.value = text;
                    ta.style.position = 'fixed';
                    ta.style.opacity = '0';
                    document.body.appendChild(ta);
                    ta.select();
                    document.execCommand('copy');
                    document.body.removeChild(ta);
                    showCopied(btn);
                }
            }
            function showCopied(btn) {
                if (!btn) return;
                btn.classList.add('copied');
                var icon = btn.innerHTML;
                btn.innerHTML = '<i class="bi bi-check-lg"></i>';
                setTimeout(function() {
                    btn.innerHTML = icon;
                    btn.classList.remove('copied');
                }, 2000);
            }
            "#))
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct LoginInput {
    pub password: String,
}

pub fn single_card_layout(header: &str, content: Markup) -> Markup {
    card_layout("col-md-8 col-lg-5 narrow-container", header, content)
}

fn card_layout(col_class: &str, header: &str, content: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html {
            head {
                (common_head("Picomint"))
            }
            body class="d-flex align-items-center min-vh-100" {
                div class="container" {
                    div class="row justify-content-center" {
                        div class=(col_class) {
                            div class="card" {
                                div class="card-header dashboard-header" { (header) }
                                div class="card-body" {
                                    (content)
                                }
                            }
                        }
                    }
                }
                script src="/assets/bootstrap.bundle.min.js" integrity="sha384-C6RzsynM9kWDrMNeT87bh95OGNyZPhcTNXj1NW7RuBCsyN/o0jlpcV8Qyq46cDfL" crossorigin="anonymous" {}
            }
        }
    }
}

/// Renders a readonly input with a copy-to-clipboard button using
/// Bootstrap's input-group pattern.
pub fn copiable_text(text: &str) -> Markup {
    html! {
        div class="input-group" {
            input type="text" class="form-control form-control-sm font-monospace"
                value=(text) readonly;
            button type="button" class="btn btn-outline-secondary"
                onclick=(format!("copyText('{}', this)", text)) {
                i class="bi bi-clipboard" {}
            }
        }
    }
}

pub fn login_form(error: Option<&str>) -> Markup {
    html! {
        form id="login-form" hx-post=(LOGIN_ROUTE) hx-target="#login-form" hx-swap="outerHTML" {
            div class="form-group mb-3" {
                input type="password" class="form-control" id="password" name="password" placeholder="Your Password" required autofocus;
            }
            @if let Some(error) = error {
                div class="alert alert-danger mb-3" { (error) }
            }
            button type="submit" class="btn btn-primary w-100 py-2" { "Continue" }
        }
    }
}

pub fn login_submit_response(
    auth: ApiAuth,
    auth_cookie_name: String,
    auth_cookie_value: String,
    jar: CookieJar,
    input: LoginInput,
) -> impl IntoResponse {
    if auth.verify(&input.password) {
        let mut cookie = Cookie::new(auth_cookie_name, auth_cookie_value);

        cookie.set_http_only(true);
        cookie.set_same_site(Some(SameSite::Lax));

        return (jar.add(cookie), [("HX-Redirect", "/")]).into_response();
    }

    Html(login_form(Some("The password is invalid")).into_string()).into_response()
}

pub fn dashboard_layout(content: Markup, version: &str) -> Markup {
    html! {
        (DOCTYPE)
        html {
            head {
                (common_head("Picomint"))
            }
            body {
                div class="container" {
                    (content)

                    div class="text-center mt-4 mb-3" {
                        span class="text-muted" { "Version " (version) }
                    }
                }
                script src="/assets/bootstrap.bundle.min.js" integrity="sha384-C6RzsynM9kWDrMNeT87bh95OGNyZPhcTNXj1NW7RuBCsyN/o0jlpcV8Qyq46cDfL" crossorigin="anonymous" {}
            }
        }
    }
}
