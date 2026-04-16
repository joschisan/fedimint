use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::response::Redirect;
use axum_extra::extract::CookieJar;
use fedimint_core::net::auth::GuardianAuthToken;

use crate::ui::{LOGIN_ROUTE, UiState};

/// Extractor that validates the UI admin cookie.
///
/// Holding a `UserAuth` proves the admin password was verified — and by
/// construction yields a `GuardianAuthToken`, which is what the
/// fedimint-core internals require for guardian-scoped operations.
pub struct UserAuth {
    pub guardian_auth_token: GuardianAuthToken,
}

impl UserAuth {
    fn authenticated() -> Self {
        Self {
            guardian_auth_token: GuardianAuthToken::new_unchecked(),
        }
    }
}

impl<Api> FromRequestParts<UiState<Api>> for UserAuth
where
    Api: Send + Sync + 'static,
{
    type Rejection = Redirect;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &UiState<Api>,
    ) -> Result<Self, Self::Rejection> {
        let jar = CookieJar::from_request_parts(parts, state)
            .await
            .map_err(|_| Redirect::to(LOGIN_ROUTE))?;

        match jar.get(&state.auth_cookie_name) {
            Some(cookie) if cookie.value() == state.auth_cookie_value => {
                Ok(UserAuth::authenticated())
            }
            _ => Err(Redirect::to(LOGIN_ROUTE)),
        }
    }
}
