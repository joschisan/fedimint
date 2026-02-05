use std::fmt::Display;

use axum::extract::State;
use axum::http::{HeaderMap, header};
use axum::response::IntoResponse;
use axum::{Form, Json};
use fedimint_gateway_common::ConnectFedPayload;
use fedimint_ui_common::UiState;
use fedimint_ui_common::auth::UserAuth;
use maud::{Markup, PreEscaped, html};
use serde::Serialize;

use crate::{
    CONNECT_FEDERATION_ROUTE, DynGatewayApi, redirect_error, redirect_success_with_export_reminder,
};

#[derive(Serialize)]
struct ConnectFedResponse {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    federation_info: Option<fedimint_gateway_common::FederationInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

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
                                "Recover from File"
                            }
                        } @else {
                            button type="button"
                                class="btn btn-warning"
                                disabled
                                data-bs-toggle="tooltip"
                                data-bs-placement="top"
                                title="Gateway must be in Running state to import and recover federations"
                            {
                                "Recover from File"
                            }
                        }
                    }
                }
            }
        }

        // Import Modal
        div class="modal fade" id="importModal" tabindex="-1" aria-labelledby="importModalLabel" aria-hidden="true" {
            div class="modal-dialog modal-dialog-centered modal-lg" {
                div class="modal-content" {
                    div class="modal-header" {
                        h5 class="modal-title" id="importModalLabel" { "Recover Federations" }
                        button type="button" id="modalCloseBtn" class="btn-close" onclick="hideImportModal();" aria-label="Close" {}
                    }
                    div class="modal-body" {
                        // File selection view
                        div id="fileSelectionView" {
                            div class="alert alert-warning" role="alert" {
                                strong { "Important: " }
                                "The recovery process may take some time per federation. Please be patient and do not close this window until the process is complete."
                            }
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
                        }

                        // Progress view (hidden initially)
                        div id="progressView" class="d-none" {
                            div class="alert alert-info" role="alert" {
                                "Recovery is in progress. This may take some time. Please wait..."
                            }
                            div class="mb-3" {
                                div class="d-flex justify-content-between mb-2" {
                                    span id="progressText" { "Processing 0 of 0 federations..." }
                                    span id="progressPercent" { "0%" }
                                }
                                div class="progress" {
                                    div
                                        id="progressBar"
                                        class="progress-bar progress-bar-striped progress-bar-animated"
                                        role="progressbar"
                                        style="width: 0%"
                                        aria-valuenow="0"
                                        aria-valuemin="0"
                                        aria-valuemax="100"
                                    {}
                                }
                            }
                            div id="federationStatusList" class="list-group" {}
                        }

                        // Results view (hidden initially)
                        div id="resultsView" class="d-none" {
                            div id="resultsSummary" class="alert" {}
                            div id="resultsDetails" class="mb-3" {}
                        }
                    }
                    div class="modal-footer" {
                        button type="button" id="cancelBtn" class="btn btn-secondary" onclick="hideImportModal();" { "Cancel" }
                        button
                            type="button"
                            id="startRecoveryBtn"
                            class="btn btn-warning"
                            onclick="startRecovery();"
                        {
                            "Recover Federations"
                        }
                        button
                            type="button"
                            id="closeResultsBtn"
                            class="btn btn-primary d-none"
                            onclick="hideImportModal();"
                        {
                            "Close"
                        }
                    }
                }
            }
        }

        // JavaScript for modal control and recovery
        script {
            (PreEscaped(r#"
            var importModal = null;
            var recoveryData = null;
            var recoveryResults = {
                recovered: [],
                skipped: [],
                failed: []
            };

            function showImportModal() {
                var modalEl = document.getElementById('importModal');
                if (!importModal) {
                    importModal = new bootstrap.Modal(modalEl, {
                        backdrop: 'static',
                        keyboard: false
                    });
                }
                resetModalState();
                importModal.show();
            }

            function hideImportModal() {
                // Check if we're showing results (recovery completed)
                var resultsView = document.getElementById('resultsView');
                var shouldRefresh = resultsView && !resultsView.classList.contains('d-none');

                if (importModal) {
                    importModal.hide();
                }

                // Refresh the page to show updated federation list if recovery completed
                if (shouldRefresh) {
                    window.location.reload();
                }
            }

            function resetModalState() {
                document.getElementById('inviteCodesFile').value = '';
                document.getElementById('fileSelectionView').classList.remove('d-none');
                document.getElementById('progressView').classList.add('d-none');
                document.getElementById('resultsView').classList.add('d-none');
                document.getElementById('cancelBtn').classList.remove('d-none');
                document.getElementById('startRecoveryBtn').classList.remove('d-none');
                document.getElementById('closeResultsBtn').classList.add('d-none');
                document.getElementById('modalCloseBtn').classList.remove('d-none');
                document.getElementById('federationStatusList').innerHTML = '';
                document.getElementById('resultsSummary').innerHTML = '';
                document.getElementById('resultsDetails').innerHTML = '';
                recoveryData = null;
                recoveryResults = { recovered: [], skipped: [], failed: [] };
            }

            async function startRecovery() {
                var fileInput = document.getElementById('inviteCodesFile');

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

                // Read and parse file
                try {
                    var content = await file.text();
                    recoveryData = JSON.parse(content);
                } catch (e) {
                    alert('Failed to parse JSON file: ' + e.message);
                    return;
                }

                // Validate data structure
                var federationIds = Object.keys(recoveryData);
                if (federationIds.length === 0) {
                    alert('No federations found in the uploaded file.');
                    return;
                }

                // Switch to progress view and disable modal dismissal
                document.getElementById('fileSelectionView').classList.add('d-none');
                document.getElementById('progressView').classList.remove('d-none');
                document.getElementById('startRecoveryBtn').classList.add('d-none');
                document.getElementById('cancelBtn').classList.add('d-none');
                document.getElementById('modalCloseBtn').classList.add('d-none');

                // Initialize progress
                updateProgress(0, federationIds.length);

                // Create status list items
                var statusList = document.getElementById('federationStatusList');
                federationIds.forEach(function(fedId) {
                    var item = document.createElement('div');
                    item.className = 'list-group-item d-flex justify-content-between align-items-center';
                    item.id = 'fed-status-' + fedId;
                    item.innerHTML = '<span>' + fedId + '</span><span class="badge bg-secondary">Pending</span>';
                    statusList.appendChild(item);
                });

                // Process federations sequentially
                for (var i = 0; i < federationIds.length; i++) {
                    var fedId = federationIds[i];
                    updateProgress(i, federationIds.length);
                    await processFederation(fedId, recoveryData[fedId]);
                }

                // Complete
                updateProgress(federationIds.length, federationIds.length);
                showResults();
            }

            function updateProgress(current, total) {
                var percent = total > 0 ? Math.round((current / total) * 100) : 0;
                document.getElementById('progressText').textContent = 'Processing ' + current + ' of ' + total + ' federations...';
                document.getElementById('progressPercent').textContent = percent + '%';
                var progressBar = document.getElementById('progressBar');
                progressBar.style.width = percent + '%';
                progressBar.setAttribute('aria-valuenow', percent);
            }

            async function processFederation(federationId, inviteCodes) {
                var statusEl = document.getElementById('fed-status-' + federationId);
                if (statusEl) {
                    statusEl.querySelector('.badge').className = 'badge bg-info';
                    statusEl.querySelector('.badge').textContent = 'Processing...';
                }

                // Check if there are invite codes
                if (!inviteCodes || inviteCodes.length === 0) {
                    recoveryResults.failed.push({ id: federationId, error: 'No invite codes available' });
                    updateFederationStatus(federationId, 'failed', 'No invite codes');
                    return;
                }

                // Create URL-encoded form data
                var formData = new URLSearchParams();
                formData.append('invite_code', inviteCodes[0]);
                formData.append('recover', 'true');

                try {
                    var response = await fetch('/ui/federations/join', {
                        method: 'POST',
                        headers: {
                            'Accept': 'application/json',
                            'Content-Type': 'application/x-www-form-urlencoded'
                        },
                        body: formData.toString()
                    });

                    // Check if response is JSON
                    var contentType = response.headers.get('content-type');
                    if (!contentType || !contentType.includes('application/json')) {
                        // Not JSON - likely an error page or redirect
                        var text = await response.text();
                        console.error('Non-JSON response:', text.substring(0, 200));
                        recoveryResults.failed.push({
                            id: federationId,
                            error: 'Server returned non-JSON response (status: ' + response.status + ')'
                        });
                        updateFederationStatus(federationId, 'failed', 'Server error (status: ' + response.status + ')');
                        return;
                    }

                    var result = await response.json();

                    if (result.status === 'success') {
                        var fedName = result.federation_info && result.federation_info.federation_name ?
                                     result.federation_info.federation_name : federationId;
                        recoveryResults.recovered.push({ id: federationId, name: fedName });
                        updateFederationStatus(federationId, 'success', 'Recovered');
                    } else {
                        recoveryResults.failed.push({ id: federationId, error: result.error || 'Unknown error' });
                        updateFederationStatus(federationId, 'failed', result.error || 'Failed');
                    }
                } catch (e) {
                    console.error('Error processing federation:', federationId, e);
                    recoveryResults.failed.push({ id: federationId, error: e.message });
                    updateFederationStatus(federationId, 'failed', 'Network error');
                }
            }

            function updateFederationStatus(fedId, status, message) {
                var statusEl = document.getElementById('fed-status-' + fedId);
                if (statusEl) {
                    var badge = statusEl.querySelector('.badge');
                    if (status === 'success') {
                        badge.className = 'badge bg-success';
                    } else if (status === 'skipped') {
                        badge.className = 'badge bg-warning text-dark';
                    } else if (status === 'failed') {
                        badge.className = 'badge bg-danger';
                    }
                    badge.textContent = message;
                }
            }

            function showResults() {
                document.getElementById('progressView').classList.add('d-none');
                document.getElementById('resultsView').classList.remove('d-none');
                document.getElementById('closeResultsBtn').classList.remove('d-none');
                document.getElementById('modalCloseBtn').classList.remove('d-none');

                var total = recoveryResults.recovered.length + recoveryResults.skipped.length + recoveryResults.failed.length;
                var summaryEl = document.getElementById('resultsSummary');
                var detailsEl = document.getElementById('resultsDetails');

                // Set summary
                if (recoveryResults.failed.length === 0) {
                    summaryEl.className = 'alert alert-success';
                    summaryEl.innerHTML = '<strong>Success!</strong> All federations have been recovered.';
                } else if (recoveryResults.recovered.length === 0) {
                    summaryEl.className = 'alert alert-danger';
                    summaryEl.innerHTML = '<strong>Error!</strong> No federations could be recovered. Please try again later.';
                } else {
                    summaryEl.className = 'alert alert-warning';
                    summaryEl.innerHTML = '<strong>Partial Success!</strong> Some federations were recovered, but there were failures. Please try again later.';
                }

                // Build details
                var details = [];
                if (recoveryResults.recovered.length > 0) {
                    details.push('<p><strong>Recovered (' + recoveryResults.recovered.length + '):</strong> ' +
                        recoveryResults.recovered.map(function(r) { return r.name || r.id; }).join(', ') + '</p>');
                }
                if (recoveryResults.skipped.length > 0) {
                    details.push('<p><strong>Skipped - Already Joined (' + recoveryResults.skipped.length + '):</strong> ' +
                        recoveryResults.skipped.join(', ') + '</p>');
                }
                if (recoveryResults.failed.length > 0) {
                    var failedList = recoveryResults.failed.map(function(f) {
                        return f.id + ' (' + (f.error || 'Unknown error') + ')';
                    }).join(', ');
                    details.push('<p><strong>Failed (' + recoveryResults.failed.length + '):</strong> ' + failedList + '</p>');
                    details.push('<p class="text-muted mt-2">Please try again later to recover the failed federations.</p>');
                }

                detailsEl.innerHTML = details.join('');
            }
            "#))
        }
    )
}

pub async fn connect_federation_handler<E: Display>(
    headers: HeaderMap,
    State(state): State<UiState<DynGatewayApi<E>>>,
    _auth: UserAuth,
    Form(payload): Form<ConnectFedPayload>,
) -> impl IntoResponse {
    let accepts_json = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("application/json"))
        .unwrap_or(false);

    match state.api.handle_connect_federation(payload).await {
        Ok(info) => {
            if accepts_json {
                Json(ConnectFedResponse {
                    status: "success".to_string(),
                    federation_info: Some(info),
                    error: None,
                })
                .into_response()
            } else {
                redirect_success_with_export_reminder(format!(
                    "Successfully joined {}.",
                    info.federation_name
                        .unwrap_or("Unnamed Federation".to_string())
                ))
                .into_response()
            }
        }
        Err(err) => {
            let error_msg = format!("Failed to join federation: {err}");
            if accepts_json {
                Json(ConnectFedResponse {
                    status: "error".to_string(),
                    federation_info: None,
                    error: Some(error_msg),
                })
                .into_response()
            } else {
                redirect_error(error_msg).into_response()
            }
        }
    }
}
