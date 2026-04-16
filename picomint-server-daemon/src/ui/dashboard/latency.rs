use std::collections::BTreeMap;
use std::time::Duration;

use maud::{Markup, html};
use picomint_core::PeerId;
use picomint_server_core::P2PConnectionStatus;

pub fn render(
    consensus_ord_latency: Option<Duration>,
    p2p_connection_status: &BTreeMap<PeerId, Option<P2PConnectionStatus>>,
) -> Markup {
    html! {
        div class="card h-100" id="consensus-latency" {
            div class="card-header dashboard-header" { "System Latency" }
            div class="card-body" {
                @if let Some(duration) = consensus_ord_latency {
                    div class=(format!("alert {}", if duration.as_millis() < 1000 {
                        "alert-success"
                    } else if duration.as_millis() < 2000 {
                        "alert-warning"
                    } else {
                        "alert-danger"
                    })) {
                        "Consensus Latency: " strong {
                            (format!("{} ms", duration.as_millis()))
                        }
                    }
                }
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
