use maud::{Markup, html};
use picomint_core::module::audit::AuditSummary;

pub fn render(audit_summary: &AuditSummary) -> Markup {
    html! {
        div class="card h-100" {
            div class="card-header dashboard-header" { "Audit Summary" }
            div class="card-body" {
                div class="mb-3" {
                    div class="alert alert-info" {
                        "Total Net Assets: " strong { (format!("{} msat", audit_summary.net_assets)) }
                    }
                }

                table class="table table-striped" {
                    thead {
                        tr {
                            th { "Module Kind" }
                            th { "Net Assets (msat)" }
                        }
                    }
                    tbody {
                        @for (kind, module_summary) in audit_summary.module_summaries.iter() {
                            tr {
                                td { (kind) }
                                td { (module_summary.net_assets) }
                            }
                        }
                    }
                }
            }
        }
    }
}
