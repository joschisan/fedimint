use maud::{Markup, html};

use super::BACKUP_CONFIG_ROUTE;

/// Card with a download link for the full server config (including
/// private keys). The user can later restore the guardian from this file.
pub fn render() -> Markup {
    html! {
        div class="card h-100" {
            div class="card-header dashboard-header" { "Download Config" }
            div class="card-body d-flex flex-column" {
                div class="alert alert-info" {
                    "Download the server config — including the private keys — and store it somewhere safe. "
                    "You can completely recover the guardian from this file."
                }

                a href=(BACKUP_CONFIG_ROUTE)
                    download="config.json"
                    class="btn btn-outline-primary w-100 py-2 mt-auto" {
                    "Download Config"
                }
            }
        }
    }
}
