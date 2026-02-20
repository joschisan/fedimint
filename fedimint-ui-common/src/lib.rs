pub mod assets;
pub mod auth;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use axum::extract::State;
use axum::response::{Html, IntoResponse};
use axum_extra::extract::CookieJar;
use fedimint_core::hex::ToHex;
use fedimint_core::secp256k1::rand::{Rng, thread_rng};
use maud::{DOCTYPE, Markup, html};
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio::time::timeout;

pub const ROOT_ROUTE: &str = "/";
pub const LOGIN_ROUTE: &str = "/login";
pub const CONNECTIVITY_CHECK_ROUTE: &str = "/ui/connectivity-check";

/// Generic state for both setup and dashboard UIs
#[derive(Clone)]
pub struct UiState<T> {
    pub api: T,
    pub auth_cookie_name: String,
    pub auth_cookie_value: String,
}

impl<T> UiState<T> {
    pub fn new(api: T) -> Self {
        Self {
            api,
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
        link rel="stylesheet" type="text/css" href="/assets/style.css";
        link rel="icon" type="image/png" href="/assets/logo.png";

        // Note: this needs to be included in the header, so that web-page does not
        // get in a state where htmx is not yet loaded. `deref` helps with blocking the load.
        // Learned the hard way. --dpc
        script defer src="/assets/htmx.org-2.0.4.min.js" {}

        title { (title) }
    }
}

#[derive(Debug, Deserialize)]
pub struct LoginInput {
    pub password: String,
}

pub fn login_layout(title: &str, content: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html {
            head {
                (common_head(title))
            }
            body {
                div class="container" {
                    div class="row justify-content-center" {
                        div class="col-md-8 col-lg-5 narrow-container" {
                            header class="text-center" {
                                h1 class="header-title" { (title) }
                            }

                            div class="card" {
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

pub fn login_form_response(title: &str) -> impl IntoResponse {
    let content = html! {
        form method="post" action=(LOGIN_ROUTE) {
            div class="form-group mb-4" {
                input type="password" class="form-control" id="password" name="password" placeholder="Your password" required;
            }
            div class="button-container" {
                button type="submit" class="btn btn-primary setup-btn" { "Log In" }
            }
        }
    };

    Html(login_layout(title, content).into_string()).into_response()
}

pub fn dashboard_layout(content: Markup, title: &str, version: Option<&str>) -> Markup {
    html! {
        (DOCTYPE)
        html {
            head {
                (common_head(title))
            }
            body {
                div class="container" {
                    header class="text-center mb-4" {
                        h1 class="header-title mb-1" { (title) }
                        @if let Some(version) = version {
                            div {
                                small class="text-muted" { "v" (version) }
                            }
                        }
                    }

                    (content)
                }
                (connectivity_widget())
                script src="/assets/bootstrap.bundle.min.js" integrity="sha384-C6RzsynM9kWDrMNeT87bh95OGNyZPhcTNXj1NW7RuBCsyN/o0jlpcV8Qyq46cDfL" crossorigin="anonymous" {}
            }
        }
    }
}

/// Fixed-position div that loads the connectivity status fragment via htmx.
pub fn connectivity_widget() -> Markup {
    html! {
        div
            style="position: fixed; bottom: 1rem; right: 1rem; z-index: 1050;"
            hx-get=(CONNECTIVITY_CHECK_ROUTE)
            hx-trigger="load, every 30s"
            hx-swap="innerHTML"
        {}
    }
}

async fn check_tcp_connect(addr: SocketAddr) -> bool {
    timeout(Duration::from_secs(3), TcpStream::connect(addr))
        .await
        .is_ok_and(|r| r.is_ok())
}

/// Handler that checks internet connectivity by attempting TCP connections
/// to well-known anycast IPs and returns an HTML fragment.
/// Manually checks auth cookie to avoid `UserAuth` extractor's redirect,
/// which would cause htmx to swap the entire login page into the widget.
pub async fn connectivity_check_handler<Api: Send + Sync + 'static>(
    State(state): State<UiState<Api>>,
    jar: CookieJar,
) -> Html<String> {
    // Check auth manually — return empty fragment if not authenticated
    let authenticated = jar
        .get(&state.auth_cookie_name)
        .is_some_and(|c| c.value() == state.auth_cookie_value);

    if !authenticated {
        return Html(String::new());
    }

    let check_1 = check_tcp_connect(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)), 443));
    let check_2 = check_tcp_connect(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 53));

    let (r1, r2) = tokio::join!(check_1, check_2);
    let is_connected = r1 || r2;

    let markup = if is_connected {
        html! {
            span class="badge bg-success" style="font-size: 0.75rem;" {
                "Internet connection OK"
            }
        }
    } else {
        html! {
            span class="badge bg-danger" style="font-size: 0.75rem;" {
                "Internet connection unavailable"
            }
        }
    };

    Html(markup.into_string())
}
