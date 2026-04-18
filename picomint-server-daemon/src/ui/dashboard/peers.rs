use std::collections::BTreeMap;

use maud::{Markup, html};
use picomint_core::PeerId;
use crate::p2p::P2PConnectionStatus;

pub fn render(p2p_connection_status: &BTreeMap<PeerId, Option<P2PConnectionStatus>>) -> Markup {
    html! {
        div class="card h-100" id="peer-connections" {
            div class="card-header dashboard-header" { "Peer Connections" }
            div class="card-body" {
                @if p2p_connection_status.is_empty() {
                    p { "No peer connections available." }
                } @else {
                    table class="table table-striped" {
                        thead {
                            tr {
                                th { "ID" }
                                th { "Status" }
                                th { "Round Trip" }
                            }
                        }
                        tbody {
                            @for (peer_id, status) in p2p_connection_status {
                                tr {
                                    td { (peer_id.to_string()) }
                                    td {
                                        @match status {
                                            Some(_) => {
                                                span class="badge bg-success" { "Connected" }
                                            }
                                            None => {
                                                span class="badge bg-danger" { "Disconnected" }
                                            }
                                        }
                                    }
                                    td {
                                        @match status.as_ref().and_then(|s| s.rtt) {
                                            Some(duration) => {
                                                (format!("{} ms", duration.as_millis()))
                                            }
                                            None => {
                                                span class="text-muted" { "N/A" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
