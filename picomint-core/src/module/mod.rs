//! Core module system traits and types.
//!
//! Picomint supports modules to allow extending its functionality.
//! Some of the standard functionality is implemented in form of modules as
//! well. This rust module houses the core trait
//! [`picomint_core::module::ModuleCommon`] used by both the server and client
//! side module traits. Specific server and client traits exist in their
//! respective crates.
//!
//! The top level server-side types are:
//!
//! * `picomint_server::core::ServerModuleInit`
//! * `picomint_server::core::ServerModule`
//!
//! Top level client-side types are:
//!
//! * `ClientModuleInit` (in `picomint_client`)
//! * `ClientModule` (in `picomint_client`)
pub mod audit;

use std::error::Error;
use std::fmt::{self, Debug, Formatter};

use serde::{Deserialize, Serialize};

mod version;
pub use self::version::*;
use crate::Amount;
use crate::encoding::{Decodable, Encodable};

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

/// Type-erased API request: `params` carries the consensus-encoded parameter
/// bytes, which the endpoint decodes into its concrete `Param` type.
#[derive(Debug, Clone, Encodable, Decodable)]
pub struct ApiRequestErased {
    pub params: Vec<u8>,
}

impl Default for ApiRequestErased {
    fn default() -> Self {
        Self::new(())
    }
}

impl ApiRequestErased {
    pub fn new<T: Encodable>(params: T) -> Self {
        Self {
            params: params.consensus_encode_to_vec(),
        }
    }

    pub fn to_typed<T: Decodable>(&self) -> std::io::Result<T> {
        T::consensus_decode_exact(&self.params)
    }
}

#[derive(Debug, Clone, Encodable, Decodable)]
pub enum ApiMethod {
    Core(String),
    Mint(String),
    Ln(String),
    Wallet(String),
}

impl fmt::Display for ApiMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Core(s) => f.write_fmt(format_args!("core/{s}")),
            Self::Mint(s) => f.write_fmt(format_args!("mint/{s}")),
            Self::Ln(s) => f.write_fmt(format_args!("ln/{s}")),
            Self::Wallet(s) => f.write_fmt(format_args!("wallet/{s}")),
        }
    }
}

#[derive(Debug, Clone, Encodable, Decodable)]
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

pub const PICOMINT_ALPN: &[u8] = b"picomint";

/// Authentication secret used to verify guardian admin API requests.
///
/// The inner value is private to prevent timing leaks via direct comparison.
/// Use [`Self::verify`] for authentication checks. [`Self::as_str`] is a
/// temporary escape hatch for I/O that still needs the plaintext value and
/// should be removed once passwords are hashed at rest.
#[derive(Clone, Serialize, Deserialize, Encodable, Decodable)]
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

#[derive(Debug, Clone, Serialize, Deserialize, Encodable, Decodable)]
pub struct ApiError {
    pub code: u32,
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
    pub fn new(code: u32, message: String) -> Self {
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

/// Trait implemented by every `*ModuleInit` (server or client side)
pub trait ModuleInit: Debug + Clone + Send + Sync + 'static {
    type Common: CommonModuleInit;
}

/// Logic and constant common between server side and client side modules
#[async_trait::async_trait]
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
    type ConsensusItem: Encodable + Decodable + Debug + Clone + Send + Sync + 'static;
    type InputError: Encodable + Decodable + Debug + Clone + Send + Sync + 'static;
    type OutputError: Encodable + Decodable + Debug + Clone + Send + Sync + 'static;
}
