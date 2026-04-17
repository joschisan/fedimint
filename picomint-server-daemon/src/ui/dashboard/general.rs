use std::collections::BTreeMap;

use maud::{Markup, html};
use picomint_core::PeerId;

use super::BACKUP_CONFIG_ROUTE;

/// Renders the Guardian info card with federation name, session count and
/// guardian list
pub fn render(
    federation_name: &str,
    session_count: u64,
    guardian_names: &BTreeMap<PeerId, String>,
) -> Markup {
    html! {
        div class="card h-100" {
            div class="card-header dashboard-header" { (federation_name) }
            div class="card-body" {
                div id="session-count" class="alert alert-info" {
                    "Session Count: " strong { (session_count) }
                }

                table class="table table-sm mb-0" {
                    thead {
                        tr {
                            th { "Guardian ID" }
                            th { "Guardian Name" }
                        }
                    }
                    tbody {
                        @for (guardian_id, name) in guardian_names {
                            tr {
                                td { (guardian_id.to_string()) }
                                td { (name) }
                            }
                        }
                    }
                }

                div class="alert alert-info mt-4" {
                    "Download the server config — including the private keys — and store it somewhere safe. "
                    "You can completely recover the guardian from this file."
                }

                a href=(BACKUP_CONFIG_ROUTE)
                    download="config.json"
                    class="btn btn-outline-primary w-100 py-2" {
                    "Download Config"
                }
            }
        }
    }
}
