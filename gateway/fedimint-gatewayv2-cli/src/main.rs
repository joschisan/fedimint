//! `gatewaydv2-cli` — the admin CLI for the `gatewaydv2` Lightning gateway.
//!
//! Each command POSTs a JSON request body to the gateway's admin Unix socket
//! at `{DATA_DIR}/cli.sock` and pretty-prints the JSON response. A refused
//! request prints one JSON object on stderr, `{"code": ..., "error": ...}`,
//! and exits 1; a usage error exits 2 and an unreachable daemon 3. Every
//! command's `--help` ends with the JSON Schema of what it prints and the
//! codes it fails with. Modelled on picomint's gateway CLI.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use clap::{Parser, Subcommand};
use fedimint_core::error::ErrorCode;
use fedimint_gatewayv2_cli_core::{
    ANALYTICS_SCHEMA_SQL, CLI_SOCKET_FILENAME, FederationBalanceRequest, FederationBalanceResponse,
    FederationConfigRequest, FederationConfigResponse, FederationDisableRequest,
    FederationEnableRequest, FederationJoinError, FederationJoinRequest, FederationListResponse,
    FederationMintCountRequest, FederationMintCountResponse, FederationMintReceiveRequest,
    FederationMintReceiveResponse, FederationMintSendRequest, FederationMintSendResponse,
    FederationWalletReceiveRequest, FederationWalletReceiveResponse,
    FederationWalletSendFeeRequest, FederationWalletSendFeeResponse, FederationWalletSendRequest,
    FederationWalletSendResponse, InfoResponse, LdkBalancesResponse, LdkChannelCloseRequest,
    LdkChannelListResponse, LdkChannelOpenRequest, LdkChannelSpliceInRequest,
    LdkChannelSpliceOutRequest, LdkError, LdkLnProbeRequest, LdkLnReceiveRequest,
    LdkLnReceiveResponse, LdkLnSendRequest, LdkLnSendResponse, LdkOnchainReceiveResponse,
    LdkOnchainSendRequest, LdkOnchainSendResponse, LdkPeerConnectRequest, LdkPeerDisconnectRequest,
    LdkPeerListResponse, LdkReceiveError, LdkSendError, MintReceiveError, MintSendError,
    MnemonicResponse, NotJoinedError, QueryError, QueryRequest, QueryResponse,
    ROUTE_FEDERATION_BALANCE, ROUTE_FEDERATION_CONFIG, ROUTE_FEDERATION_DISABLE,
    ROUTE_FEDERATION_ENABLE, ROUTE_FEDERATION_JOIN, ROUTE_FEDERATION_LIST,
    ROUTE_FEDERATION_MODULE_MINT_COUNT, ROUTE_FEDERATION_MODULE_MINT_RECEIVE,
    ROUTE_FEDERATION_MODULE_MINT_SEND, ROUTE_FEDERATION_MODULE_WALLET_RECEIVE,
    ROUTE_FEDERATION_MODULE_WALLET_SEND, ROUTE_FEDERATION_MODULE_WALLET_SEND_FEE, ROUTE_INFO,
    ROUTE_LDK_BALANCES, ROUTE_LDK_CHANNEL_CLOSE, ROUTE_LDK_CHANNEL_LIST, ROUTE_LDK_CHANNEL_OPEN,
    ROUTE_LDK_CHANNEL_SPLICE_IN, ROUTE_LDK_CHANNEL_SPLICE_OUT, ROUTE_LDK_LN_PROBE,
    ROUTE_LDK_LN_RECEIVE, ROUTE_LDK_LN_SEND, ROUTE_LDK_ONCHAIN_RECEIVE, ROUTE_LDK_ONCHAIN_SEND,
    ROUTE_LDK_PEER_CONNECT, ROUTE_LDK_PEER_DISCONNECT, ROUTE_LDK_PEER_LIST, ROUTE_MNEMONIC,
    ROUTE_QUERY, WalletSendError, WalletSendFeeError,
};
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper::body::Bytes;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::net::UnixStream;
use tower_service::Service;

/// Shown at the end of the top-level `--help`: the rules for an agent
/// driving the CLI, stated where the agent reads them.
const FOOTER: &str = "\
Commands marked (secret) print key material, and whatever an agent reads \
ends up in its context and transcript. Rules for an agent: run a (secret) \
command only when the operator asks; run it with stdout redirected into a \
file; never read that file; open it for the operator if asked.\n\n\
Errors print one JSON object on stderr, {\"code\": ..., \"error\": ...}: the \
code is what to branch on, the error what to tell the operator. The exit \
code is 1 for a request the daemon refused, 2 for a usage error and 3 when \
the daemon is unreachable. A command's --help lists every code it fails \
with; the one code no help lists, internal, means the daemon hit a bug.";

/// The admin CLI of a `gatewaydv2` Lightning gateway: one LDK Lightning
/// node that is also a client of every federation it serves.
///
/// `ldk` manages the Lightning node, `federation` the gateway's membership
/// and balances in its federations. Run `info` first for the node's
/// identity. Every command prints JSON, and its --help ends with the schema
/// of what it prints and the codes it fails with.
#[derive(Parser)]
#[command(version, after_help = FOOTER)]
struct Cli {
    /// Path to the gateway's data directory (must match the daemon's
    /// `FM_DATA_DIR`). The CLI finds the admin Unix socket at
    /// `{DATA_DIR}/cli.sock`.
    #[arg(long = "data-dir", env = "FM_DATA_DIR")]
    data_dir: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Display gateway info
    #[command(after_long_help = schema::<InfoResponse>())]
    Info,
    /// Print the mnemonic seed words; pipe it into a file (secret)
    #[command(after_long_help = schema::<MnemonicResponse>())]
    Mnemonic,
    /// Query the analytics db with read-only SQL; rows print as JSON objects
    #[command(after_long_help = format!(
        "The analytics db, as the SQL that creates it:\n{ANALYTICS_SCHEMA_SQL}\n{}",
        schema_fallible::<QueryResponse, QueryError>()
    ))]
    Query(QueryRequest),
    /// LDK lightning node management
    #[command(subcommand)]
    Ldk(LdkCommands),
    /// Federation management
    #[command(subcommand)]
    Federation(FederationCommands),
}

#[derive(Subcommand)]
enum LdkCommands {
    /// Get node balances
    #[command(after_long_help = schema::<LdkBalancesResponse>())]
    Balances,
    /// On-chain operations
    #[command(subcommand)]
    Onchain(LdkOnchainCommands),
    /// Channel operations
    #[command(subcommand)]
    Channel(LdkChannelCommands),
    /// Lightning operations
    #[command(subcommand)]
    Ln(LdkLnCommands),
    /// Peer management
    #[command(subcommand)]
    Peer(LdkPeerCommands),
}

#[derive(Subcommand)]
enum LdkOnchainCommands {
    /// Get a receive address
    #[command(after_long_help = schema_fallible::<LdkOnchainReceiveResponse, LdkError>())]
    Receive,
    /// Send funds
    #[command(after_long_help = schema_fallible::<LdkOnchainSendResponse, LdkError>())]
    Send(LdkOnchainSendRequest),
}

#[derive(Subcommand)]
enum LdkChannelCommands {
    /// Open a channel
    #[command(after_long_help = schema_fallible::<(), LdkError>())]
    Open(LdkChannelOpenRequest),
    /// Close a channel
    #[command(after_long_help = schema_fallible::<(), LdkError>())]
    Close(LdkChannelCloseRequest),
    /// List channels
    #[command(after_long_help = schema::<LdkChannelListResponse>())]
    List,
    /// Splice on-chain funds into a channel (experimental)
    #[command(after_long_help = schema_fallible::<(), LdkError>())]
    SpliceIn(LdkChannelSpliceInRequest),
    /// Splice funds out of a channel to an on-chain address (experimental)
    #[command(after_long_help = schema_fallible::<(), LdkError>())]
    SpliceOut(LdkChannelSpliceOutRequest),
}

#[derive(Subcommand)]
enum LdkLnCommands {
    /// Create a bolt11 invoice to receive a payment
    #[command(after_long_help = schema_fallible::<LdkLnReceiveResponse, LdkReceiveError>())]
    Receive(LdkLnReceiveRequest),
    /// Pay a bolt11 invoice
    #[command(after_long_help = schema_fallible::<LdkLnSendResponse, LdkSendError>())]
    Send(LdkLnSendRequest),
    /// Probe routes towards a node to warm the pathfinding scorer; outcomes
    /// surface only in the daemon's logs
    #[command(after_long_help = schema_fallible::<(), LdkError>())]
    Probe(LdkLnProbeRequest),
}

#[derive(Subcommand)]
enum LdkPeerCommands {
    /// Connect to a peer
    #[command(after_long_help = schema_fallible::<(), LdkError>())]
    Connect(LdkPeerConnectRequest),
    /// Disconnect from a peer
    #[command(after_long_help = schema_fallible::<(), LdkError>())]
    Disconnect(LdkPeerDisconnectRequest),
    /// List peers
    #[command(after_long_help = schema::<LdkPeerListResponse>())]
    List,
}

#[derive(Subcommand)]
enum FederationCommands {
    /// Join a federation
    #[command(after_long_help = schema_fallible::<(), FederationJoinError>())]
    Join(FederationJoinRequest),
    /// Disable a federation's public client API. In-flight contracts continue
    /// to settle and it can be re-enabled
    #[command(after_long_help = schema_fallible::<(), NotJoinedError>())]
    Disable(FederationDisableRequest),
    /// Re-enable a previously disabled federation
    #[command(after_long_help = schema::<()>())]
    Enable(FederationEnableRequest),
    /// List joined federations
    #[command(after_long_help = schema::<FederationListResponse>())]
    List,
    /// Get a joined federation's JSON client config
    #[command(after_long_help = schema_fallible::<FederationConfigResponse, NotJoinedError>())]
    Config(FederationConfigRequest),
    /// Get the gateway's ecash balance in a federation
    #[command(after_long_help = schema_fallible::<FederationBalanceResponse, NotJoinedError>())]
    Balance(FederationBalanceRequest),
    /// Per-federation module commands
    #[command(subcommand)]
    Module(ModuleCommands),
}

#[derive(Subcommand)]
enum ModuleCommands {
    /// Mint module commands
    #[command(subcommand)]
    Mint(MintCommands),
    /// Wallet module commands
    #[command(subcommand)]
    Wallet(WalletCommands),
}

#[derive(Subcommand)]
enum MintCommands {
    /// Count ecash notes per denomination, keyed by the denomination's exponent
    #[command(after_long_help = schema_fallible::<FederationMintCountResponse, NotJoinedError>())]
    Count(FederationMintCountRequest),
    /// Send ecash
    #[command(after_long_help = schema_fallible::<FederationMintSendResponse, MintSendError>())]
    Send(FederationMintSendRequest),
    /// Receive ecash
    #[command(after_long_help = schema_fallible::<FederationMintReceiveResponse, MintReceiveError>())]
    Receive(FederationMintReceiveRequest),
}

#[derive(Subcommand)]
enum WalletCommands {
    /// Get send fee estimate
    #[command(after_long_help = schema_fallible::<FederationWalletSendFeeResponse, WalletSendFeeError>())]
    SendFee(FederationWalletSendFeeRequest),
    /// Send on-chain from the federation wallet
    #[command(after_long_help = schema_fallible::<FederationWalletSendResponse, WalletSendError>())]
    Send(FederationWalletSendRequest),
    /// Get a receive address
    #[command(after_long_help = schema_fallible::<FederationWalletReceiveResponse, NotJoinedError>())]
    Receive(FederationWalletReceiveRequest),
}

/// The JSON Schema of a command's response, rendered for the tail of its
/// `--help`: every field explained, no daemon needed.
fn schema<Resp: JsonSchema>() -> String {
    format!(
        "Prints, as JSON Schema:\n{}",
        serde_json::to_string_pretty(&schema_for!(Resp)).expect("a schema serializes")
    )
}

/// [`schema`] followed by every code the command fails with, from the error
/// enum the daemon's handler returns, so help and daemon cannot disagree.
fn schema_fallible<Resp: JsonSchema, Err: ErrorCode>() -> String {
    let codes = Err::codes()
        .iter()
        .map(|entry| format!("{}: {}", entry.0, entry.1))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "{}\n\nFails with, as the code of the JSON error on stderr:\n{codes}",
        schema::<Resp>()
    )
}

/// Why a command failed, and how the CLI exits on it: one JSON object on
/// stderr, `{"code": ..., "error": ...}`, and an exit code by class.
#[derive(Debug)]
enum RequestError {
    /// The daemon did not answer: no socket at the data dir, or the
    /// connection failed. Exit code 3.
    Unreachable(String),
    /// The daemon answered with an error body; `code` and `error` are its.
    /// Exit code 1.
    Rejected { code: String, error: String },
    /// The daemon's reply was not the JSON expected. Exit code 1.
    Malformed(String),
}

impl RequestError {
    /// Print the error on stderr and exit with its class's code.
    fn exit(self) -> ! {
        let (code, error, exit) = match self {
            RequestError::Unreachable(error) => ("daemon_unreachable".to_string(), error, 3),
            RequestError::Rejected { code, error } => (code, error, 1),
            RequestError::Malformed(error) => ("malformed".to_string(), error, 1),
        };

        eprintln!("{}", serde_json::json!({ "code": code, "error": error }));

        std::process::exit(exit)
    }
}

/// The body the daemon answers a refused request with.
#[derive(Deserialize)]
struct ErrorBody {
    code: String,
    error: String,
}

/// Tiny connector that dials a fixed Unix socket path, ignoring the URI
/// entirely. Plugs into `hyper_util::client::legacy::Client` where a TCP
/// connector would normally go.
#[derive(Clone)]
struct UnixConnector {
    path: PathBuf,
}

impl Service<hyper::Uri> for UnixConnector {
    type Response = TokioIo<UnixStream>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<TokioIo<UnixStream>>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: hyper::Uri) -> Self::Future {
        let path = self.path.clone();
        Box::pin(async move { UnixStream::connect(path).await.map(TokioIo::new) })
    }
}

/// POST `payload` as JSON to `route` on the daemon whose data directory is
/// `data_dir`, and return the JSON reply — `Null` for an empty body.
async fn request<R: Serialize>(
    data_dir: &Path,
    route: &str,
    payload: R,
) -> Result<Value, RequestError> {
    let socket_path = data_dir.join(CLI_SOCKET_FILENAME);
    let connector = UnixConnector {
        path: socket_path.clone(),
    };
    let client = Client::builder(TokioExecutor::new()).build(connector);

    let body_bytes = serde_json::to_vec(&payload).expect("a request serializes");
    let uri: hyper::Uri = format!("http://localhost{route}")
        .parse()
        .expect("every route is a valid path");
    let req = Request::post(uri)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body_bytes)))
        .expect("a request with a body builds");

    let resp = client.request(req).await.map_err(|e| {
        RequestError::Unreachable(format!(
            "Failed to POST {route} to the daemon at {}: {e}",
            socket_path.display()
        ))
    })?;

    let status = resp.status();
    let resp_bytes = resp
        .into_body()
        .collect()
        .await
        .map_err(|e| RequestError::Unreachable(format!("The daemon's reply broke off: {e}")))?
        .to_bytes();

    if !status.is_success() {
        return Err(match serde_json::from_slice::<ErrorBody>(&resp_bytes) {
            Ok(body) => RequestError::Rejected {
                code: body.code,
                error: body.error,
            },
            Err(_) => RequestError::Rejected {
                code: if status.is_server_error() {
                    "internal".to_string()
                } else {
                    "bad_request".to_string()
                },
                error: String::from_utf8_lossy(&resp_bytes).into_owned(),
            },
        });
    }

    if resp_bytes.is_empty() {
        Ok(Value::Null)
    } else {
        serde_json::from_slice(&resp_bytes)
            .map_err(|e| RequestError::Malformed(format!("The daemon's reply is not JSON: {e}")))
    }
}

/// Pretty-print a reply.
fn print_json(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("Cannot serialize")
    );
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    let d = &cli.data_dir;

    let result = match cli.command {
        Commands::Info => request(d, ROUTE_INFO, ()).await,
        Commands::Mnemonic => request(d, ROUTE_MNEMONIC, ()).await,
        Commands::Query(req) => request(d, ROUTE_QUERY, req).await,

        Commands::Ldk(cmd) => match cmd {
            LdkCommands::Balances => request(d, ROUTE_LDK_BALANCES, ()).await,
            LdkCommands::Onchain(cmd) => match cmd {
                LdkOnchainCommands::Receive => request(d, ROUTE_LDK_ONCHAIN_RECEIVE, ()).await,
                LdkOnchainCommands::Send(req) => request(d, ROUTE_LDK_ONCHAIN_SEND, req).await,
            },
            LdkCommands::Channel(cmd) => match cmd {
                LdkChannelCommands::Open(req) => request(d, ROUTE_LDK_CHANNEL_OPEN, req).await,
                LdkChannelCommands::Close(req) => request(d, ROUTE_LDK_CHANNEL_CLOSE, req).await,
                LdkChannelCommands::List => request(d, ROUTE_LDK_CHANNEL_LIST, ()).await,
                LdkChannelCommands::SpliceIn(req) => {
                    request(d, ROUTE_LDK_CHANNEL_SPLICE_IN, req).await
                }
                LdkChannelCommands::SpliceOut(req) => {
                    request(d, ROUTE_LDK_CHANNEL_SPLICE_OUT, req).await
                }
            },
            LdkCommands::Ln(cmd) => match cmd {
                LdkLnCommands::Receive(req) => request(d, ROUTE_LDK_LN_RECEIVE, req).await,
                LdkLnCommands::Send(req) => request(d, ROUTE_LDK_LN_SEND, req).await,
                LdkLnCommands::Probe(req) => request(d, ROUTE_LDK_LN_PROBE, req).await,
            },
            LdkCommands::Peer(cmd) => match cmd {
                LdkPeerCommands::Connect(req) => request(d, ROUTE_LDK_PEER_CONNECT, req).await,
                LdkPeerCommands::Disconnect(req) => {
                    request(d, ROUTE_LDK_PEER_DISCONNECT, req).await
                }
                LdkPeerCommands::List => request(d, ROUTE_LDK_PEER_LIST, ()).await,
            },
        },

        Commands::Federation(cmd) => match cmd {
            FederationCommands::Join(req) => request(d, ROUTE_FEDERATION_JOIN, req).await,
            FederationCommands::Disable(req) => request(d, ROUTE_FEDERATION_DISABLE, req).await,
            FederationCommands::Enable(req) => request(d, ROUTE_FEDERATION_ENABLE, req).await,
            FederationCommands::List => request(d, ROUTE_FEDERATION_LIST, ()).await,
            FederationCommands::Config(req) => request(d, ROUTE_FEDERATION_CONFIG, req).await,
            FederationCommands::Balance(req) => request(d, ROUTE_FEDERATION_BALANCE, req).await,
            FederationCommands::Module(cmd) => match cmd {
                ModuleCommands::Mint(cmd) => match cmd {
                    MintCommands::Count(req) => {
                        request(d, ROUTE_FEDERATION_MODULE_MINT_COUNT, req).await
                    }
                    MintCommands::Send(req) => {
                        request(d, ROUTE_FEDERATION_MODULE_MINT_SEND, req).await
                    }
                    MintCommands::Receive(req) => {
                        request(d, ROUTE_FEDERATION_MODULE_MINT_RECEIVE, req).await
                    }
                },
                ModuleCommands::Wallet(cmd) => match cmd {
                    WalletCommands::SendFee(req) => {
                        request(d, ROUTE_FEDERATION_MODULE_WALLET_SEND_FEE, req).await
                    }
                    WalletCommands::Send(req) => {
                        request(d, ROUTE_FEDERATION_MODULE_WALLET_SEND, req).await
                    }
                    WalletCommands::Receive(req) => {
                        request(d, ROUTE_FEDERATION_MODULE_WALLET_RECEIVE, req).await
                    }
                },
            },
        },
    };

    match result {
        Ok(value) => print_json(&value),
        Err(error) => error.exit(),
    }
}
