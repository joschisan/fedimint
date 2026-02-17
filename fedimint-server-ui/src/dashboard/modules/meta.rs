use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::LazyLock;

use axum::extract::{Form, FromRequest, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use chrono::NaiveDateTime;
use fedimint_core::PeerId;
use fedimint_core::module::serde_json::{self, Value};
use fedimint_meta_server::Meta;
use fedimint_server_core::dashboard_ui::{DashboardApiModuleExt, DynDashboardApi};
use fedimint_ui_common::UiState;
use fedimint_ui_common::auth::UserAuth;
use maud::{Markup, html};
use serde::Serialize;
use thiserror::Error;
use tracing::{debug, warn};

use crate::LOG_UI;

// Meta route constants
pub const META_SUBMIT_ROUTE: &str = "/meta/submit";
pub const META_SET_ROUTE: &str = "/meta/set";
pub const META_RESET_ROUTE: &str = "/meta/reset";
pub const META_DELETE_ROUTE: &str = "/meta/delete";
pub const META_VALUE_INPUT_ROUTE: &str = "/meta/value-input";

/// The type of value expected for a well-known meta key.
enum KeyType {
    String,
    Url,
    Amount,
    DateTime,
    Json,
}

/// Schema describing a well-known meta key.
struct KeySchema {
    description: &'static str,
    value_type: KeyType,
}

// <https://fedibtc.github.io/fedi-docs/docs/fedi/meta_fields/federation-metadata-configurations>
// NOTE: If you're updating this, please update `docs/meta_fields/README.md`
static WELL_KNOWN_KEYS: LazyLock<BTreeMap<&'static str, KeySchema>> = LazyLock::new(|| {
    BTreeMap::from([
        (
            "welcome_message",
            KeySchema {
                description: "A welcome message for new users joining the federation",
                value_type: KeyType::String,
            },
        ),
        (
            "federation_expiry_timestamp",
            KeySchema {
                description: "The date and time after which the federation will shut down",
                value_type: KeyType::DateTime,
            },
        ),
        (
            "federation_name",
            KeySchema {
                description: "The human-readable name of the federation",
                value_type: KeyType::String,
            },
        ),
        (
            "federation_successor",
            KeySchema {
                description: "An invite code to a successor federation for user migration",
                value_type: KeyType::String,
            },
        ),
        (
            "meta_override_url",
            KeySchema {
                description: "A URL to a file containing overrides for meta fields",
                value_type: KeyType::Url,
            },
        ),
        (
            "vetted_gateways",
            KeySchema {
                description: "A list of gateway identifiers vetted by the federation",
                value_type: KeyType::Json,
            },
        ),
        (
            "recurringd_api",
            KeySchema {
                description: "The API URL of a recurringd instance for creating LNURLs",
                value_type: KeyType::Url,
            },
        ),
        (
            "lnaddress_api",
            KeySchema {
                description: "The API URL of a Lightning Address Server for serving LNURLs",
                value_type: KeyType::Url,
            },
        ),
        (
            "fedi:pinned_message",
            KeySchema {
                description: "",
                value_type: KeyType::String,
            },
        ),
        (
            "fedi:federation_icon_url",
            KeySchema {
                description: "",
                value_type: KeyType::Url,
            },
        ),
        (
            "fedi:tos_url",
            KeySchema {
                description: "",
                value_type: KeyType::Url,
            },
        ),
        (
            "fedi:default_currency",
            KeySchema {
                description: "",
                value_type: KeyType::String,
            },
        ),
        (
            "fedi:invite_codes_disabled",
            KeySchema {
                description: "",
                value_type: KeyType::String,
            },
        ),
        (
            "fedi:new_members_disabled",
            KeySchema {
                description: "",
                value_type: KeyType::String,
            },
        ),
        (
            "fedi:max_invoice_msats",
            KeySchema {
                description: "",
                value_type: KeyType::Amount,
            },
        ),
        (
            "fedi:max_balance_msats",
            KeySchema {
                description: "",
                value_type: KeyType::Amount,
            },
        ),
        (
            "fedi:max_stable_balance_msats",
            KeySchema {
                description: "",
                value_type: KeyType::Amount,
            },
        ),
        (
            "fedi:fedimods",
            KeySchema {
                description: "",
                value_type: KeyType::Json,
            },
        ),
        (
            "fedi:default_group_chats",
            KeySchema {
                description: "",
                value_type: KeyType::Json,
            },
        ),
        (
            "fedi:offline_wallet_disabled",
            KeySchema {
                description: "",
                value_type: KeyType::String,
            },
        ),
    ])
});

// Function to render the Meta module UI section
pub async fn render(meta: &Meta) -> Markup {
    // Get current consensus value
    let consensus_value = meta.handle_get_consensus_request_ui().await.ok().flatten();
    // Get current revision number
    let revision = meta
        .handle_get_consensus_revision_request_ui()
        .await
        .ok()
        .unwrap_or(0);
    // Get current submissions from all peers
    let submissions = meta
        .handle_get_submissions_request_ui()
        .await
        .ok()
        .unwrap_or_default();

    let current_meta_keys = if let Some(o) = submissions
        .get(&meta.our_peer_id)
        .cloned()
        .or_else(|| consensus_value.clone())
        .and_then(|v| v.as_object().cloned())
    {
        o
    } else {
        serde_json::Map::new()
    };

    html! {
        div class="card h-100" {
            div class="card-header dashboard-header" { "Meta Configuration" }
            div class="card-body" {
                div class="mb-4" {
                    h5 { "Current Consensus (Revision: " (revision) ")" }
                    @if let Some(value) = &consensus_value {
                        pre class="bg-light p-3 user-select-all" {
                            code {
                                (serde_json::to_string_pretty(value).unwrap_or_else(|_| "Invalid JSON".to_string()))
                            }
                        }
                    } @else {
                        div class="alert alert-secondary" { "No consensus value has been established yet." }
                    }
                    div class="mb-4" {
                        (render_meta_edit_form(current_meta_keys, false, MetaEditForm::default()))
                    }

                    (render_submissions_form(meta.our_peer_id, &submissions))
                }
            }
        }
    }
}

fn render_submissions_form(our_id: PeerId, submissions: &BTreeMap<PeerId, Value>) -> Markup {
    let mut submissions_by_value: HashMap<String, BTreeSet<PeerId>> = HashMap::new();

    for (peer_id, value) in submissions {
        let value_str =
            serde_json::to_string_pretty(value).unwrap_or_else(|_| "Invalid JSON".to_string());
        submissions_by_value
            .entry(value_str)
            .or_default()
            .insert(*peer_id);
    }

    html! {
        div #meta-submissions hx-swap-oob=(true) {
            @if !submissions.is_empty() {
                h5 { "Current Peer Submissions" }
                div class="table-responsive" {
                    table class="table table-sm" {
                        thead {
                            tr {
                                th { "Peer IDs" }
                                th { "Submission" }
                                th { "Actions" }
                            }
                        }
                        tbody {
                            @for (value_str, peer_ids) in submissions_by_value {
                                tr {
                                    td { (
                                        peer_ids.iter()
                                        .map(|n| n.to_string())
                                        .collect::<Vec<String>>()
                                        .join(", "))
                                    }
                                    td {
                                        pre class="m-0 p-2 bg-light" style="max-height: 150px; overflow-y: auto;" {
                                            code {
                                                (value_str)
                                            }
                                        }
                                    }
                                    @if !peer_ids.contains(&our_id) {
                                        td {
                                            form method="post"
                                                hx-post=(META_SUBMIT_ROUTE)
                                                hx-swap="none"
                                            {
                                                input type="hidden" name="json_content"
                                                    value=(value_str);
                                                button type="submit" class="btn btn-sm btn-success" {
                                                    "Accept This Submission"
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
}

// Form for meta value submission
#[derive(serde::Deserialize, Default)]
pub struct MetaEditForm {
    pub json_content: String,
    #[serde(default)]
    pub add_key: String,
    #[serde(default)]
    pub add_value: String,
    #[serde(default)]
    pub delete_key: String,
}

impl MetaEditForm {
    fn top_level_keys(&self) -> RequestResult<serde_json::Map<String, Value>> {
        Ok(
            if let Some(serde_json::Value::Object(o)) =
                serde_json::from_slice(self.json_content.as_bytes())
                    .map_err(|x| RequestError::BadRequest { source: x.into() })?
            {
                o
            } else {
                serde_json::Map::new()
            },
        )
    }
}

pub async fn post_submit(
    State(state): State<UiState<DynDashboardApi>>,
    _auth: UserAuth,
    Form(form): Form<MetaEditForm>,
) -> RequestResult<Response> {
    let meta_module = state.api.get_module::<Meta>().unwrap();

    let top_level_keys = form.top_level_keys()?;
    let top_level_object = Value::Object(top_level_keys.clone());

    meta_module
        .handle_submit_request_ui(top_level_object.clone())
        .await
        .inspect_err(|msg| warn!(target: LOG_UI, msg= %msg.message, "Request error"))
        .map_err(|_err| RequestError::InternalError)?;

    let mut submissions = meta_module
        .handle_get_submissions_request_ui()
        .await
        .ok()
        .unwrap_or_default();

    submissions.insert(meta_module.our_peer_id, top_level_object);

    let content = html! {
        (render_meta_edit_form(top_level_keys, false, MetaEditForm::default()))

        // Re-render submission with our submission added, as it will take couple of milliseconds
        // for it to get processed and it's confusing if it doesn't immediatel show up.
        (render_submissions_form(meta_module.our_peer_id, &submissions))
    };
    Ok(Html(content.into_string()).into_response())
}

pub async fn post_reset(
    State(state): State<UiState<DynDashboardApi>>,
    _auth: UserAuth,
    Form(_form): Form<MetaEditForm>,
) -> RequestResult<Response> {
    let meta_module = state.api.get_module::<Meta>().unwrap();

    let consensus_value = meta_module
        .handle_get_consensus_request_ui()
        .await
        .ok()
        .flatten();

    let top_level_keys = if let Some(serde_json::Value::Object(o)) = consensus_value {
        o
    } else {
        serde_json::Map::new()
    };
    let top_level_object = Value::Object(top_level_keys.clone());

    meta_module
        .handle_submit_request_ui(top_level_object.clone())
        .await
        .inspect_err(|msg| warn!(target: LOG_UI, msg = %msg.message, "Request error"))
        .map_err(|_err| RequestError::InternalError)?;

    let mut submissions = meta_module
        .handle_get_submissions_request_ui()
        .await
        .ok()
        .unwrap_or_default();

    submissions.remove(&meta_module.our_peer_id);

    let content = html! {
        (render_meta_edit_form(top_level_keys, false, MetaEditForm::default()))

        // Re-render submission with our submission added, as it will take couple of milliseconds
        // for it to get processed and it's confusing if it doesn't immediatel show up.
        (render_submissions_form(meta_module.our_peer_id, &submissions))
    };
    Ok(Html(content.into_string()).into_response())
}

pub async fn post_set(
    _auth: UserAuth,
    Form(mut form): Form<MetaEditForm>,
) -> RequestResult<Response> {
    let mut top_level_object = form.top_level_keys()?;

    let key = form.add_key.trim();
    let raw_value = form.add_value.trim();
    let key_type = WELL_KNOWN_KEYS
        .get(key)
        .map(|s| &s.value_type)
        .unwrap_or(&KeyType::String);
    let value = convert_input_value(raw_value, key_type)?;

    top_level_object.insert(key.to_string(), value);

    form.add_key = "".into();
    form.add_value = "".into();
    let content = render_meta_edit_form(top_level_object, true, MetaEditForm::default());
    Ok(Html(content.into_string()).into_response())
}

pub async fn post_delete(
    _auth: UserAuth,
    Form(mut form): Form<MetaEditForm>,
) -> RequestResult<Response> {
    let mut top_level_json = form.top_level_keys()?;

    let key = form.delete_key.trim();

    top_level_json.remove(key);
    form.delete_key = "".into();

    let content = render_meta_edit_form(top_level_json, true, form);
    Ok(Html(content.into_string()).into_response())
}

/// Renders the appropriate HTML input element for the given key type.
fn render_value_input(key_type: &KeyType, current_value: &str) -> Markup {
    match key_type {
        KeyType::Url => html! {
            input #add-value type="url" name="add_value" class="form-control"
                placeholder="https://..." aria-label="Value"
                value=(current_value) {}
        },
        KeyType::Amount => html! {
            div class="input-group" {
                input #add-value type="number" name="add_value" class="form-control"
                    placeholder="0" aria-label="Value"
                    value=(current_value) {}
                span class="input-group-text" { "msats" }
            }
        },
        KeyType::DateTime => {
            // Pre-fill with today at 00:00 so the time portion isn't blank
            let val = if current_value.is_empty() {
                chrono::Utc::now().format("%Y-%m-%dT00:00").to_string()
            } else {
                current_value.to_string()
            };
            html! {
                div class="input-group" {
                    input #add-value type="datetime-local" name="add_value" class="form-control"
                        aria-label="Value"
                        value=(val) {}
                    span class="input-group-text" { "UTC" }
                }
            }
        }
        KeyType::Json => html! {
            textarea #add-value name="add_value" class="form-control" rows="3"
                placeholder="{}" aria-label="Value"
            { (current_value) }
        },
        KeyType::String => html! {
            input #add-value type="text" name="add_value" class="form-control"
                placeholder="Value" aria-label="Value"
                value=(current_value) {}
        },
    }
}

/// Renders the description hint for a well-known key (if any).
fn render_value_description(key: &str) -> Markup {
    let description = WELL_KNOWN_KEYS
        .get(key)
        .map(|s| s.description)
        .unwrap_or("");

    html! {
        @if !description.is_empty() {
            small class="form-text text-muted" { (description) }
        }
    }
}

/// Query params for the value-input HTMX endpoint.
#[derive(serde::Deserialize)]
pub struct ValueInputQuery {
    #[serde(default)]
    pub add_key: String,
}

/// HTMX endpoint: returns a type-appropriate input fragment for the given key,
/// plus an OOB swap for the description hint below.
pub async fn get_value_input(
    _auth: UserAuth,
    Query(query): Query<ValueInputQuery>,
) -> impl IntoResponse {
    let key = query.add_key.trim();
    let key_type = WELL_KNOWN_KEYS
        .get(key)
        .map(|s| &s.value_type)
        .unwrap_or(&KeyType::String);

    let content = html! {
        (render_value_input(key_type, ""))
        div #value-description-container hx-swap-oob="innerHTML" {
            (render_value_description(key))
        }
    };
    Html(content.into_string())
}

/// Converts raw form input to a [`serde_json::Value`] appropriate for the
/// given key type.
fn convert_input_value(raw: &str, key_type: &KeyType) -> RequestResult<Value> {
    match key_type {
        KeyType::DateTime => {
            // Browsers may send with or without seconds
            let dt = NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S")
                .or_else(|_| NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M"))
                .map_err(|e| RequestError::BadRequest {
                    source: anyhow::anyhow!("Invalid datetime: {e}"),
                })?;
            Ok(Value::String(dt.and_utc().timestamp().to_string()))
        }
        KeyType::Amount => {
            let _: u64 = raw.parse().map_err(|e| RequestError::BadRequest {
                source: anyhow::anyhow!("Invalid amount: {e}"),
            })?;
            Ok(Value::String(raw.to_string()))
        }
        KeyType::Json => serde_json::from_str(raw).map_err(|e| RequestError::BadRequest {
            source: anyhow::anyhow!("Invalid JSON: {e}"),
        }),
        KeyType::Url | KeyType::String => {
            // Try JSON parse first (backward compat), fall back to plain string
            Ok(serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_string())))
        }
    }
}

pub fn render_meta_edit_form(
    mut top_level_json: serde_json::Map<String, Value>,
    // was the value edited via set/delete
    pending: bool,
    form: MetaEditForm,
) -> Markup {
    top_level_json.sort_keys();

    let known_keys: BTreeSet<String> = top_level_json
        .keys()
        .cloned()
        .chain(WELL_KNOWN_KEYS.keys().map(|k| (*k).to_string()))
        .collect();

    let default_input = render_value_input(&KeyType::String, &form.add_value);

    html! {
        form #meta-edit-form hx-swap-oob=(true) {
            h5 {
                "Proposal"
                @if pending {
                    " (Pending)"
                }
            }
            div class="input-group mb-2" {
                textarea class="form-control" rows="15" readonly
                    name="json_content"
                {
                    (serde_json::to_string_pretty(&top_level_json).expect("Can't fail"))
                }
            }
            div class="input-group mb-1" {
                input #add-key type="text" class="form-control" placeholder="Key" aria-label="Key" list="keyOptions"
                    // keys are usually shorter than values, so keep it small
                    style="max-width: 250px;"
                    name="add_key"
                    value=(form.add_key)
                    hx-get=(META_VALUE_INPUT_ROUTE)
                    hx-trigger="change, input changed delay:300ms"
                    hx-target="#value-input-container"
                    hx-swap="innerHTML"
                {}
                span class="input-group-text" { ":" }

                datalist id="keyOptions" {
                    @for key in &known_keys {
                        option value=(key) {}
                    }
                }

                div #value-input-container class="flex-grow-1" {
                    (default_input)
                }
                button class="btn btn-primary btn-min-width"
                    type="button" id="button-set"
                    title="Set a value in a meta proposal"
                    hx-post=(META_SET_ROUTE)
                    hx-swap="none"
                    hx-trigger="click, keypress[key=='Enter'] from:#add-value, keypress[key=='Enter'] from:#add-key"
                { "Set" }
            }
            div #value-description-container class="mb-2" {}
            div class="input-group mb-2" {
                select class="form-select"
                    id="delete-key"
                    name="delete_key"
                {
                    option value="" {}
                    @for key in top_level_json.keys() {
                        option value=(key) selected[key == &form.delete_key]{ (key) }
                    }
                }
                button class="btn btn-primary btn-min-width"
                    hx-post=(META_DELETE_ROUTE)
                    hx-swap="none"
                    hx-trigger="click, keypress[key=='Enter'] from:#delete-key"
                    title="Delete a value in a meta proposal"
                { "Delete" }
            }
            div class="d-flex justify-content-between btn-min-width" {
                button class="btn btn-outline-warning me-5"
                    title="Reset to current consensus"
                    hx-post=(META_RESET_ROUTE)
                    hx-swap="none"
                { "Reset" }
                button class="btn btn-success btn-min-width"
                    hx-post=(META_SUBMIT_ROUTE)
                    hx-swap="none"
                    title="Submit new meta document for approval of other peers"
                { "Submit" }
            }
        }
    }
}

/// Wrapper over `T` to make it a json request response
#[derive(FromRequest)]
#[from_request(via(axum::Json), rejection(RequestError))]
struct AppJson<T>(pub T);

impl<T> IntoResponse for AppJson<T>
where
    axum::Json<T>: IntoResponse,
{
    fn into_response(self) -> Response {
        axum::Json(self.0).into_response()
    }
}

/// Whatever can go wrong with a request
#[derive(Debug, Error)]
pub enum RequestError {
    #[error("Bad request: {source}")]
    BadRequest { source: anyhow::Error },
    #[error("Internal Error")]
    InternalError,
}

pub type RequestResult<T> = std::result::Result<T, RequestError>;

impl IntoResponse for RequestError {
    fn into_response(self) -> Response {
        debug!(target: LOG_UI, err=%self, "Request Error");

        let (status_code, message) = match self {
            Self::BadRequest { source } => {
                (StatusCode::BAD_REQUEST, format!("Bad Request: {source}"))
            }
            _ => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal Service Error".to_owned(),
            ),
        };

        (status_code, AppJson(UserErrorResponse { message })).into_response()
    }
}

// How we want user errors responses to be serialized
#[derive(Serialize)]
pub struct UserErrorResponse {
    pub message: String,
}
