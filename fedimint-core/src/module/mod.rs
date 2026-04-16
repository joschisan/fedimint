//! Core module system traits and types.
//!
//! Fedimint supports modules to allow extending its functionality.
//! Some of the standard functionality is implemented in form of modules as
//! well. This rust module houses the core trait
//! [`fedimint_core::module::ModuleCommon`] used by both the server and client
//! side module traits. Specific server and client traits exist in their
//! respective crates.
//!
//! The top level server-side types are:
//!
//! * `fedimint_server::core::ServerModuleInit`
//! * `fedimint_server::core::ServerModule`
//!
//! Top level client-side types are:
//!
//! * `ClientModuleInit` (in `fedimint_client`)
//! * `ClientModule` (in `fedimint_client`)
pub mod audit;
pub mod registry;

use std::error::Error;
use std::fmt::{self, Debug, Formatter};
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};

use fedimint_logging::LOG_NET_API;
use futures::Future;
use jsonrpsee_core::JsonValue;
use serde::{Deserialize, Serialize};
use tracing::Instrument;

// TODO: Make this module public and remove theDkgPeerMessage`pub use` below
mod version;
pub use self::version::*;
use crate::core::ModuleInstanceId;
use crate::encoding::{Decodable, Encodable};
use crate::fmt_utils::AbbreviateHexBytes;
use crate::task::MaybeSend;
use crate::util::FmtCompact;
use crate::{Amount, apply, async_trait_maybe_send, maybe_add_send, maybe_add_send_sync};

#[derive(Debug, PartialEq, Eq)]
pub struct InputMeta {
    pub amount: TransactionItemAmounts,
    pub pub_key: secp256k1::PublicKey,
}

/// Information about the amount represented by an input or output.
///
/// * For **inputs** the amount is funding the transaction while the fee is
///   consuming funding
/// * For **outputs** the amount and the fee consume funding
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct TransactionItemAmounts {
    pub amount: Amount,
    pub fee: Amount,
}

impl TransactionItemAmounts {
    pub const ZERO: Self = Self {
        amount: Amount::ZERO,
        fee: Amount::ZERO,
    };
}

/// All requests from client to server contain these fields
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ApiRequest<T> {
    /// Authentication secret for this API request, if required
    pub auth: Option<ApiAuth>,
    /// Parameters required by the API
    pub params: T,
}

pub type ApiRequestErased = ApiRequest<JsonValue>;

impl Default for ApiRequestErased {
    fn default() -> Self {
        Self {
            auth: None,
            params: JsonValue::Null,
        }
    }
}

impl ApiRequestErased {
    pub fn new<T: Serialize>(params: T) -> Self {
        Self {
            auth: None,
            params: serde_json::to_value(params)
                .expect("parameter serialization error - this should not happen"),
        }
    }

    pub fn to_json(&self) -> JsonValue {
        serde_json::to_value(self).expect("parameter serialization error - this should not happen")
    }

    pub fn with_auth(self, auth: ApiAuth) -> Self {
        Self {
            auth: Some(auth),
            params: self.params,
        }
    }

    pub fn to_typed<T: serde::de::DeserializeOwned>(
        self,
    ) -> Result<ApiRequest<T>, serde_json::Error> {
        Ok(ApiRequest {
            auth: self.auth,
            params: serde_json::from_value::<T>(self.params)?,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ApiMethod {
    Core(String),
    Module(ModuleInstanceId, String),
}

impl fmt::Display for ApiMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Core(s) => f.write_str(s),
            Self::Module(module_id, s) => f.write_fmt(format_args!("{module_id}-{s}")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IrohApiRequest {
    pub method: ApiMethod,
    pub request: ApiRequestErased,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IrohGatewayRequest {
    /// REST API route for specifying which action to take
    pub route: String,

    /// Parameters for the request
    pub params: Option<serde_json::Value>,

    /// Password for authenticated requests to the gateway
    pub password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IrohGatewayResponse {
    pub status: u16,
    pub body: serde_json::Value,
}

pub const FEDIMINT_API_ALPN: &[u8] = b"FEDIMINT_API_ALPN";
pub const FEDIMINT_GATEWAY_ALPN: &[u8] = b"FEDIMINT_GATEWAY_ALPN";

/// Authentication secret used to verify guardian admin API requests.
///
/// The inner value is private to prevent timing leaks via direct comparison.
/// Use [`Self::verify`] for authentication checks. [`Self::as_str`] is a
/// temporary escape hatch for I/O that still needs the plaintext value and
/// should be removed once passwords are hashed at rest.
#[derive(Clone, Serialize, Deserialize)]
pub struct ApiAuth(String);

impl ApiAuth {
    pub fn new(s: String) -> Self {
        Self(s)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn verify(&self, password: &str) -> bool {
        use subtle::ConstantTimeEq as _;
        bool::from(self.0.as_bytes().ct_eq(password.as_bytes()))
    }
}

impl Debug for ApiAuth {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "ApiAuth(****)")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub code: i32,
    pub message: String,
}

impl Error for ApiError {}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("{} {}", self.code, self.message))
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(code: i32, message: String) -> Self {
        Self { code, message }
    }

    pub fn not_found(message: String) -> Self {
        Self::new(404, message)
    }

    pub fn bad_request(message: String) -> Self {
        Self::new(400, message)
    }

    pub fn unauthorized() -> Self {
        Self::new(401, "Invalid authorization".to_string())
    }

    pub fn server_error(message: String) -> Self {
        Self::new(500, message)
    }
}

#[apply(async_trait_maybe_send!)]
pub trait TypedApiEndpoint {
    type State: Sync;

    /// example: /transaction
    const PATH: &'static str;

    type Param: serde::de::DeserializeOwned + Send;
    type Response: serde::Serialize;

    async fn handle<'state>(
        state: &'state Self::State,
        request: Self::Param,
    ) -> Result<Self::Response, ApiError>;
}

pub use serde_json;

/// # Example
///
/// ```rust
/// # use fedimint_core::module::ApiVersion;
/// # use fedimint_core::module::{api_endpoint, ApiEndpoint, registry::ModuleInstanceId};
/// struct State;
///
/// let _: ApiEndpoint<State> = api_endpoint! {
///     "/foobar",
///     ApiVersion::new(0, 3),
///     async |state: &State, params: ()| -> i32 {
///         Ok(0)
///     }
/// };
/// ```
#[macro_export]
macro_rules! __api_endpoint {
    (
        $path:expr_2021,
        // Api Version this endpoint was introduced in, at the current consensus level
        // Currently for documentation purposes only.
        $version_introduced:expr_2021,
        async |$state:ident: &$state_ty:ty, $param:ident: $param_ty:ty| -> $resp_ty:ty $body:block
    ) => {{
        struct Endpoint;

        #[$crate::apply($crate::async_trait_maybe_send!)]
        impl $crate::module::TypedApiEndpoint for Endpoint {
            #[allow(deprecated)]
            const PATH: &'static str = $path;
            type State = $state_ty;
            type Param = $param_ty;
            type Response = $resp_ty;

            async fn handle<'state>(
                $state: &'state Self::State,
                $param: Self::Param,
            ) -> ::std::result::Result<Self::Response, $crate::module::ApiError> {
                {
                    // just to enforce the correct type
                    const __API_VERSION: $crate::module::ApiVersion = $version_introduced;
                }
                $body
            }
        }

        $crate::module::ApiEndpoint::from_typed::<Endpoint>()
    }};
}

pub use __api_endpoint as api_endpoint;

type HandlerFnReturn<'a> =
    Pin<Box<maybe_add_send!(dyn Future<Output = Result<serde_json::Value, ApiError>> + 'a)>>;
type HandlerFn<M> =
    Box<maybe_add_send_sync!(dyn for<'a> Fn(&'a M, ApiRequestErased) -> HandlerFnReturn<'a>)>;

/// Definition of an API endpoint defined by a module `M`.
pub struct ApiEndpoint<M> {
    /// Path under which the API endpoint can be reached. It should start with a
    /// `/` e.g. `/transaction`. E.g. this API endpoint would be reachable
    /// under `module_module_instance_id_transaction` depending on the
    /// module name returned by `[FedertionModule::api_base_name]`.
    pub path: &'static str,
    /// Handler for the API call that takes the following arguments:
    ///   * Reference to the module which defined it
    ///   * Request parameters parsed into JSON `[Value](serde_json::Value)`
    pub handler: HandlerFn<M>,
}

/// Global request ID used for logging
static REQ_ID: AtomicU64 = AtomicU64::new(0);

// <()> is used to avoid specify state.
impl ApiEndpoint<()> {
    pub fn from_typed<E: TypedApiEndpoint>() -> ApiEndpoint<E::State>
    where
        <E as TypedApiEndpoint>::Response: MaybeSend,
        E::Param: Debug,
        E::Response: Debug,
    {
        async fn handle_request<'state, E>(
            state: &'state E::State,
            request: ApiRequest<E::Param>,
        ) -> Result<E::Response, ApiError>
        where
            E: TypedApiEndpoint,
            E::Param: Debug,
            E::Response: Debug,
        {
            tracing::debug!(target: LOG_NET_API, path = E::PATH, ?request, "received api request");
            let result = E::handle(state, request.params).await;
            match &result {
                Err(err) => {
                    tracing::warn!(target: LOG_NET_API, path = E::PATH, err = %err.fmt_compact(), "api request error");
                }
                _ => {
                    tracing::trace!(target: LOG_NET_API, path = E::PATH, "api request complete");
                }
            }
            result
        }

        ApiEndpoint {
            path: E::PATH,
            handler: Box::new(|m, request| {
                Box::pin(async move {
                    let request = request
                        .to_typed()
                        .map_err(|e| ApiError::bad_request(e.to_string()))?;

                    let span = tracing::info_span!(
                        target: LOG_NET_API,
                        "api_req",
                        id = REQ_ID.fetch_add(1, Ordering::SeqCst),
                        method = E::PATH,
                    );
                    let ret = handle_request::<E>(m, request).instrument(span).await?;

                    Ok(serde_json::to_value(ret).expect("encoding error"))
                })
            }),
        }
    }
}

/// Trait implemented by every `*ModuleInit` (server or client side)
pub trait ModuleInit: Debug + Clone + Send + Sync + 'static {
    type Common: CommonModuleInit;
}

/// Logic and constant common between server side and client side modules
#[apply(async_trait_maybe_send!)]
pub trait CommonModuleInit: Debug + Sized {
    const CONSENSUS_VERSION: ModuleConsensusVersion;
    const KIND: crate::core::ModuleKind;

    type ClientConfig: Encodable + Decodable + Debug + Clone + Send + Sync + 'static;
}

/// Module associated types required by both client and server
pub trait ModuleCommon {
    type ClientConfig: Encodable + Decodable + Debug + Clone + Send + Sync + 'static;
    type Input: Encodable + Decodable + Debug + Clone + Send + Sync + 'static;
    type Output: Encodable + Decodable + Debug + Clone + Send + Sync + 'static;
    type OutputOutcome: Encodable + Decodable + Debug + Clone + Send + Sync + 'static;
    type ConsensusItem: Encodable + Decodable + Debug + Clone + Send + Sync + 'static;
    type InputError: Encodable + Decodable + Debug + Clone + Send + Sync + 'static;
    type OutputError: Encodable + Decodable + Debug + Clone + Send + Sync + 'static;
}

/// Creates a struct that can be used to make our module-decodable structs
/// interact with `serde`-based APIs (AlephBFT, jsonrpsee). It creates a wrapper
/// that holds the data as serialized
// bytes internally.
#[derive(Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct SerdeModuleEncoding<T: Encodable + Decodable>(
    #[serde(with = "::fedimint_core::encoding::as_hex")] Vec<u8>,
    #[serde(skip)] PhantomData<T>,
);

/// Same as [`SerdeModuleEncoding`] but uses base64 instead of hex encoding.
#[derive(Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct SerdeModuleEncodingBase64<T: Encodable + Decodable>(
    #[serde(with = "::fedimint_core::encoding::as_base64")] Vec<u8>,
    #[serde(skip)] PhantomData<T>,
);

impl<T> fmt::Debug for SerdeModuleEncoding<T>
where
    T: Encodable + Decodable,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("SerdeModuleEncoding(")?;
        fmt::Debug::fmt(&AbbreviateHexBytes(&self.0), f)?;
        f.write_str(")")?;
        Ok(())
    }
}

impl<T: Encodable + Decodable> From<&T> for SerdeModuleEncoding<T> {
    fn from(value: &T) -> Self {
        let mut bytes = vec![];
        fedimint_core::encoding::Encodable::consensus_encode(value, &mut bytes)
            .expect("Writing to buffer can never fail");
        Self(bytes, PhantomData)
    }
}

impl<T: Encodable + Decodable + 'static> SerdeModuleEncoding<T> {
    pub fn try_into_inner(&self) -> std::io::Result<T> {
        Decodable::consensus_decode_exact(&self.0)
    }
}

impl<T: Encodable + Decodable> Encodable for SerdeModuleEncoding<T> {
    fn consensus_encode<W: std::io::Write>(&self, writer: &mut W) -> std::io::Result<()> {
        self.0.consensus_encode(writer)
    }
}

impl<T: Encodable + Decodable> Decodable for SerdeModuleEncoding<T> {
    fn consensus_decode<R: std::io::Read>(reader: &mut R) -> std::io::Result<Self> {
        Ok(Self(Vec::<u8>::consensus_decode(reader)?, PhantomData))
    }
}

impl<T> fmt::Debug for SerdeModuleEncodingBase64<T>
where
    T: Encodable + Decodable,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("SerdeModuleEncoding2(")?;
        fmt::Debug::fmt(&AbbreviateHexBytes(&self.0), f)?;
        f.write_str(")")?;
        Ok(())
    }
}

impl<T: Encodable + Decodable> From<&T> for SerdeModuleEncodingBase64<T> {
    fn from(value: &T) -> Self {
        let mut bytes = vec![];
        fedimint_core::encoding::Encodable::consensus_encode(value, &mut bytes)
            .expect("Writing to buffer can never fail");
        Self(bytes, PhantomData)
    }
}

impl<T: Encodable + Decodable + 'static> SerdeModuleEncodingBase64<T> {
    pub fn try_into_inner(&self) -> std::io::Result<T> {
        Decodable::consensus_decode_exact(&self.0)
    }
}
