use maud::{Markup, PreEscaped, html};
use qrcode::QrCode;

use crate::ui::copiable_text;

// Card with invite code text and copy button
pub fn render(invite_code: &str, session_count: u64) -> Markup {
    html! {
        div class="card h-100" {
            div class="card-header dashboard-header" { "Invite Code" }
            div class="card-body" {
                @if session_count == 0 {
                    div class="alert alert-warning" {
                        "The invite code will be available once the federation has completed its first consensus session."
                    }
                } @else {
                    @let qr_svg = QrCode::new(invite_code)
                        .expect("Failed to generate QR code")
                        .render::<qrcode::render::svg::Color>()
                        .build();

                    p { "Share this with users to onboard them to your federation." }

                    div class="mb-3" {
                        div class="border rounded p-2 bg-white" style="width: 100%;" {
                            div style="width: 100%; height: auto; overflow: hidden;" {
                                (PreEscaped(format!(r#"<div style="width: 100%; height: auto;">{}</div>"#, qr_svg.replace("width=", "data-width=").replace("height=", "data-height=").replace("<svg", r#"<svg style="width: 100%; height: auto; display: block;""#))))
                            }
                        }
                    }

                    div class="mb-3" {
                        (copiable_text(invite_code))
                    }
                }
            }
        }
    }
}
