use std::fmt::Display;

use axum::Form;
use axum::extract::State;
use axum::response::IntoResponse;
use fedimint_gateway_common::ConnectFedPayload;
use fedimint_ui_common::UiState;
use fedimint_ui_common::auth::UserAuth;
use maud::{Markup, PreEscaped, html};

use crate::{
    CONNECT_FEDERATION_ROUTE, DynGatewayApi, IMPORT_INVITE_CODES_ROUTE, redirect_error,
    redirect_success_with_export_reminder,
};

pub fn render(gateway_state: &str) -> Markup {
    let is_running = gateway_state == "Running";

    html!(
        div class="card h-100" {
            div class="card-header dashboard-header" { "Connect a new Federation" }
            div class="card-body" {
                form method="post" action=(CONNECT_FEDERATION_ROUTE)
                    onsubmit="var btn = this.querySelector('button[type=submit]'); \
                              var isRecover = this.querySelector('#recover-checkbox').checked; \
                              btn.disabled = true; \
                              btn.innerHTML = '<span class=\"spinner-border spinner-border-sm\" role=\"status\"></span> ' + (isRecover ? 'Recovering...' : 'Connecting...');"
                {
                    div class="mb-3" {
                        label class="form-label" { "Invite Code" }
                        input type="text" class="form-control" name="invite_code" required;
                    }
                    div class="mb-3 form-check" {
                        input type="checkbox" class="form-check-input" name="recover" value="true" id="recover-checkbox";
                        label class="form-check-label" for="recover-checkbox" { "Recover" }
                    }
                    div class="d-flex gap-2" {
                        button type="submit" class="btn btn-primary" { "Submit" }
                        @if is_running {
                            button type="button"
                                class="btn btn-warning"
                                onclick="showImportModal();"
                            {
                                "Import from File"
                            }
                        } @else {
                            button type="button"
                                class="btn btn-warning"
                                disabled
                                data-bs-toggle="tooltip"
                                data-bs-placement="top"
                                title="Gateway must be in Running state to import and recover federations"
                            {
                                "Import from File"
                            }
                        }
                    }
                }
            }
        }

        // Import Modal
        div class="modal fade" id="importModal" tabindex="-1" aria-labelledby="importModalLabel" aria-hidden="true" {
            div class="modal-dialog modal-dialog-centered" {
                div class="modal-content" {
                    div class="modal-header" {
                        h5 class="modal-title" id="importModalLabel" { "Recover Federations from Backup" }
                        button type="button" class="btn-close" onclick="hideImportModal();" aria-label="Close" {}
                    }
                    div class="modal-body" {
                        form id="importForm"
                            method="post"
                            action=(IMPORT_INVITE_CODES_ROUTE)
                            enctype="multipart/form-data"
                        {
                            div class="mb-3" {
                                label class="form-label" for="inviteCodesFile" {
                                    "Select Invite Codes Backup File (JSON)"
                                }
                                input
                                    type="file"
                                    class="form-control"
                                    id="inviteCodesFile"
                                    name="invite_codes"
                                    accept=".json,application/json"
                                    required;
                                div class="form-text" {
                                    "Upload the gateway-invite-codes.json file exported from this or another gateway."
                                }
                            }
                            div id="importStatus" class="d-none" {
                                div class="d-flex align-items-center" {
                                    div class="spinner-border spinner-border-sm me-2" role="status" {
                                        span class="visually-hidden" { "Processing..." }
                                    }
                                    span id="importStatusText" { "Processing..." }
                                }
                            }
                        }
                    }
                    div class="modal-footer" {
                        button type="button" class="btn btn-secondary" onclick="hideImportModal();" { "Cancel" }
                        button
                            type="button"
                            id="importSubmitBtn"
                            class="btn btn-warning"
                            onclick="submitImportForm();"
                        {
                            "Recover Federations"
                        }
                    }
                }
            }
        }

        // JavaScript for modal control
        script {
            (PreEscaped(r#"
            var importModal = null;
            function showImportModal() {
                var modalEl = document.getElementById('importModal');
                if (!importModal) {
                    importModal = new bootstrap.Modal(modalEl);
                }
                // Reset form state
                document.getElementById('importForm').reset();
                document.getElementById('importStatus').classList.add('d-none');
                document.getElementById('importSubmitBtn').disabled = false;
                document.getElementById('importSubmitBtn').innerHTML = 'Recover Federations';
                importModal.show();
            }
            function hideImportModal() {
                if (importModal) {
                    importModal.hide();
                }
            }
            function submitImportForm() {
                var form = document.getElementById('importForm');
                var fileInput = document.getElementById('inviteCodesFile');
                var statusDiv = document.getElementById('importStatus');
                var statusText = document.getElementById('importStatusText');
                var submitBtn = document.getElementById('importSubmitBtn');
                // Validate file
                if (!fileInput.files || fileInput.files.length === 0) {
                    alert('Please select a file to upload.');
                    return;
                }
                var file = fileInput.files[0];
                // Validate file type
                if (!file.name.endsWith('.json') && file.type !== 'application/json') {
                    alert('Please select a valid JSON file.');
                    return;
                }
                // Validate file size (1MB max)
                var maxSize = 1024 * 1024;
                if (file.size > maxSize) {
                    alert('File too large. Maximum size is 1MB.');
                    return;
                }
                // Show processing state
                statusDiv.classList.remove('d-none');
                statusText.textContent = 'Recovering federations, please wait... This may take a while.';
                submitBtn.disabled = true;
                submitBtn.innerHTML = '<span class="spinner-border spinner-border-sm" role="status"></span> Recovering...';
                // Submit the form
                form.submit();
            }
            "#))
        }
    )
}

pub async fn connect_federation_handler<E: Display>(
    State(state): State<UiState<DynGatewayApi<E>>>,
    _auth: UserAuth,
    Form(payload): Form<ConnectFedPayload>,
) -> impl IntoResponse {
    match state.api.handle_connect_federation(payload).await {
        Ok(info) => {
            // Redirect back to dashboard on success
            redirect_success_with_export_reminder(format!(
                "Successfully joined {}.",
                info.federation_name
                    .unwrap_or("Unnamed Federation".to_string())
            ))
            .into_response()
        }
        Err(err) => redirect_error(format!("Failed to join federation: {err}")).into_response(),
    }
}
