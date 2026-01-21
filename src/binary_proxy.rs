//! Binary port proxy with a small pool of serialized upstream connections.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use casper_binary_port::{
    AccountInformation, AddressableEntityInformation, BalanceResponse, BinaryMessage,
    BinaryResponse, BinaryResponseAndRequest, Command, CommandHeader, CommandTag, ConsensusStatus,
    ConsensusValidatorChanges, ContractInformation, DictionaryQueryResult, ErrorCode, GetRequest,
    GetTrieFullResult, GlobalStateQueryResult, InformationRequest, InformationRequestTag,
    LastProgress, NetworkName, NodeStatus, ReactorStateName, ResponseType, RewardResponse,
    SpeculativeExecutionResult, TransactionWithExecutionInfo, Uptime, ValueWithProof,
};
use casper_types::bytesrepr::{Bytes, FromBytes, ToBytes};
use casper_types::contracts::ContractPackage;
use casper_types::execution::{ExecutionResult, ExecutionResultV1};
use casper_types::{
    Approval, AvailableBlockRange, BlockBody, BlockBodyV1, BlockHeader, BlockHeaderV1,
    BlockSignatures, BlockSignaturesV1, BlockSynchronizerStatus, BlockWithSignatures,
    ChainspecRawBytes, Deploy, NextUpgrade, Package, Peers, ProtocolVersion, StoredValue,
    Transaction, Transfer,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use tracing::{debug, error, warn};

use crate::metrics::AppMetrics;

const LENGTH_BYTES: usize = 4;
const MAX_MESSAGE_SIZE_BYTES: usize = 4 * 1024 * 1024;

/// Errors produced by binary port forwarding.
#[derive(Debug)]
pub enum BinaryError {
    Io(std::io::Error),
    ChannelClosed,
    ConnectionUnavailable,
    RequestTooLarge { allowed: usize, got: usize },
    ResponseEncoding(String),
}

impl std::fmt::Display for BinaryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BinaryError::Io(err) => write!(f, "io error: {err}"),
            BinaryError::ChannelClosed => write!(f, "channel closed"),
            BinaryError::ConnectionUnavailable => write!(f, "connection unavailable"),
            BinaryError::RequestTooLarge { allowed, got } => {
                write!(f, "request too large (allowed {allowed}, got {got})")
            }
            BinaryError::ResponseEncoding(err) => write!(f, "response encoding error: {err}"),
        }
    }
}

impl std::error::Error for BinaryError {}

impl From<std::io::Error> for BinaryError {
    fn from(err: std::io::Error) -> Self {
        BinaryError::Io(err)
    }
}

/// Concurrency-limited upstream connector for the binary port.
#[derive(Clone)]
pub struct BinaryPool {
    address: Arc<String>,
    network: Arc<String>,
    semaphore: Arc<Semaphore>,
    pool: Arc<Mutex<Vec<PooledConnection>>>,
    metrics: Arc<AppMetrics>,
}

struct PooledConnection {
    stream: TcpStream,
    _permit: OwnedSemaphorePermit,
}

impl BinaryPool {
    /// Create a connector with an upper bound on concurrent upstream connections.
    pub fn new(
        network: String,
        address: String,
        max_connections: usize,
        metrics: Arc<AppMetrics>,
    ) -> Self {
        Self {
            address: Arc::new(address),
            network: Arc::new(network),
            semaphore: Arc::new(Semaphore::new(max_connections.max(1))),
            pool: Arc::new(Mutex::new(Vec::new())),
            metrics,
        }
    }

    /// Send one request and await the corresponding response.
    pub async fn send(&self, payload: Vec<u8>) -> Result<Vec<u8>, BinaryError> {
        let _inflight = self.metrics.binary_inflight_guard(self.network.as_ref());
        let address = self.address.as_ref();
        let parsed_request = match parse_request(address, &payload) {
            Ok(parsed) => parsed,
            Err(err) => {
                self.metrics
                    .increment_binary_response(self.network.as_ref(), "error");
                return build_error_response(err.error_code(), &payload);
            }
        };
        let request_id = Some(parsed_request.header.id());
        let payload_len = payload.len();
        if payload_len > MAX_MESSAGE_SIZE_BYTES {
            warn!(
                %address,
                request_id,
                payload_len,
                allowed_len = MAX_MESSAGE_SIZE_BYTES,
                "binary port request too large; dropping"
            );
            self.metrics
                .increment_binary_response(self.network.as_ref(), "error");
            return Err(BinaryError::RequestTooLarge {
                allowed: MAX_MESSAGE_SIZE_BYTES,
                got: payload_len,
            });
        }
        if matches!(
            parsed_request.information_request,
            Some(InformationRequest::Peers)
        ) {
            self.metrics
                .increment_binary_response(self.network.as_ref(), "error");
            return build_error_response(ErrorCode::FunctionDisabled, &payload);
        }

        let wait_start = Instant::now();
        let connection_result = self.get_connection().await;
        self.metrics
            .observe_binary_queue_wait(self.network.as_ref(), wait_start.elapsed());
        let mut connection = match connection_result {
            Ok(conn) => conn,
            Err(err) => {
                self.metrics
                    .increment_binary_response(self.network.as_ref(), "error");
                return Err(err);
            }
        };
        let mut result = send_and_receive(&mut connection.stream, &payload).await;
        if is_retryable_io_error(&result) {
            warn!(
                %address,
                request_id,
                payload_len,
                "binary port request failed with retryable error; reconnecting"
            );
            match reconnect(address, &mut connection).await {
                Ok(()) => {
                    result = send_and_receive(&mut connection.stream, &payload).await;
                }
                Err(err) => {
                    result = Err(err);
                }
            }
        }

        if let Ok(response) = &result {
            let rewritten = match rewrite_response_bytes(response) {
                Ok(bytes) => bytes,
                Err(err) => {
                    warn!(
                        %address,
                        request_id,
                        payload_len,
                        error = %err,
                        "binary port response could not be decoded; returning error"
                    );
                    build_error_response(err.error_code(), &payload)?
                }
            };
            log_binary_response(address, request_id, &rewritten);
            self.return_connection(connection).await;
            self.metrics
                .increment_binary_response(self.network.as_ref(), "ok");
            return Ok(rewritten);
        }
        if let Err(err) = &result {
            error!(
                %address,
                request_id,
                payload_len,
                "binary port request failed: {err}"
            );
            self.metrics
                .increment_binary_response(self.network.as_ref(), "error");
        }
        result
    }

    async fn get_connection(&self) -> Result<PooledConnection, BinaryError> {
        if let Some(connection) = self.pool.lock().await.pop() {
            return Ok(connection);
        }
        let permit = self
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| BinaryError::ConnectionUnavailable)?;
        let stream = TcpStream::connect(self.address.as_ref()).await?;
        Ok(PooledConnection {
            stream,
            _permit: permit,
        })
    }

    async fn return_connection(&self, connection: PooledConnection) {
        self.pool.lock().await.push(connection);
    }
}

async fn send_and_receive(stream: &mut TcpStream, payload: &[u8]) -> Result<Vec<u8>, BinaryError> {
    // Binary port frames are length-prefixed (little-endian u32).
    let message = BinaryMessage::new(payload.to_vec());
    let length = message.payload().len() as u32;
    let length_bytes = length.to_le_bytes();
    stream.write_all(&length_bytes).await?;
    stream.write_all(message.payload()).await?;
    stream.flush().await?;

    let mut length_buf = [0u8; LENGTH_BYTES];
    stream.read_exact(&mut length_buf).await?;
    let response_len = u32::from_le_bytes(length_buf) as usize;
    let mut response = vec![0u8; response_len];
    stream.read_exact(&mut response).await?;
    Ok(response)
}

fn is_retryable_io_error(result: &Result<Vec<u8>, BinaryError>) -> bool {
    match result {
        Err(BinaryError::Io(err)) => matches!(
            err.kind(),
            std::io::ErrorKind::UnexpectedEof
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
        ),
        _ => false,
    }
}

async fn reconnect(address: &str, connection: &mut PooledConnection) -> Result<(), BinaryError> {
    connection.stream = TcpStream::connect(address).await?;
    Ok(())
}

#[derive(Debug)]
struct ParsedRequest {
    header: CommandHeader,
    information_request: Option<InformationRequest>,
}

#[derive(Debug)]
enum RequestParseError {
    UnsupportedVersion,
    InvalidRequest,
}

impl RequestParseError {
    fn error_code(&self) -> ErrorCode {
        match self {
            RequestParseError::UnsupportedVersion => ErrorCode::UnsupportedRequest,
            RequestParseError::InvalidRequest => ErrorCode::BadRequest,
        }
    }
}

fn parse_request(address: &str, payload: &[u8]) -> Result<ParsedRequest, RequestParseError> {
    let payload_len = payload.len();
    let (header, remainder) = CommandHeader::from_bytes(payload).map_err(|err| {
        warn!(
            %address,
            payload_len,
            "failed to decode binary request header: {err}"
        );
        RequestParseError::InvalidRequest
    })?;
    if header.version() != CommandHeader::HEADER_VERSION {
        warn!(
            %address,
            request_id = header.id(),
            payload_len,
            request_version = header.version(),
            "unsupported binary request version"
        );
        return Err(RequestParseError::UnsupportedVersion);
    }
    let tag = CommandTag::try_from(header.type_tag()).map_err(|_| {
        warn!(
            %address,
            request_id = header.id(),
            payload_len,
            "invalid binary request tag"
        );
        RequestParseError::InvalidRequest
    })?;
    let command = Command::try_from((tag, remainder)).map_err(|err| {
        warn!(
            %address,
            request_id = header.id(),
            payload_len,
            "failed to decode binary request: {err}"
        );
        RequestParseError::InvalidRequest
    })?;
    let information_request = match &command {
        Command::Get(GetRequest::Information { info_type_tag, key }) => {
            let info_tag = InformationRequestTag::try_from(*info_type_tag).map_err(|_| {
                warn!(
                    %address,
                    request_id = header.id(),
                    payload_len,
                    info_type_tag,
                    "invalid binary info request tag"
                );
                RequestParseError::InvalidRequest
            })?;
            let info_request =
                InformationRequest::try_from((info_tag, key.as_slice())).map_err(|err| {
                    warn!(
                        %address,
                        request_id = header.id(),
                        payload_len,
                        "failed to decode binary info request: {err}"
                    );
                    RequestParseError::InvalidRequest
                })?;
            Some(info_request)
        }
        _ => None,
    };
    debug!(
        %address,
        request_id = header.id(),
        request_version = header.version(),
        request_tag = ?command.tag(),
        request = ?command,
        info_request = ?information_request,
        request_payload_len = payload_len,
        "binary port request"
    );
    Ok(ParsedRequest {
        header,
        information_request,
    })
}

#[derive(Debug)]
enum ResponseEnvelope {
    WithRequest {
        request: Bytes,
        response: BinaryResponse,
    },
    Response {
        response: BinaryResponse,
    },
}

impl ResponseEnvelope {
    fn response(&self) -> &BinaryResponse {
        match self {
            ResponseEnvelope::WithRequest { response, .. } => response,
            ResponseEnvelope::Response { response } => response,
        }
    }

    fn set_response(&mut self, response: BinaryResponse) {
        match self {
            ResponseEnvelope::WithRequest {
                response: inner, ..
            } => *inner = response,
            ResponseEnvelope::Response { response: inner } => *inner = response,
        }
    }
}

#[derive(Debug)]
struct OpaquePayload(Vec<u8>);

impl FromBytes for OpaquePayload {
    fn from_bytes(bytes: &[u8]) -> Result<(Self, &[u8]), casper_types::bytesrepr::Error> {
        Ok((OpaquePayload(bytes.to_vec()), &[]))
    }
}

impl ToBytes for OpaquePayload {
    fn to_bytes(&self) -> Result<Vec<u8>, casper_types::bytesrepr::Error> {
        Ok(self.0.clone())
    }

    fn write_bytes(&self, writer: &mut Vec<u8>) -> Result<(), casper_types::bytesrepr::Error> {
        writer.extend_from_slice(&self.0);
        Ok(())
    }

    fn serialized_length(&self) -> usize {
        self.0.len()
    }
}

#[derive(Debug)]
enum ResponseRewriteError {
    Decode(casper_types::bytesrepr::Error),
    Encode(casper_types::bytesrepr::Error),
    UnknownTypeTag(u8),
    MissingTypeTag,
}

impl ResponseRewriteError {
    fn error_code(&self) -> ErrorCode {
        match self {
            ResponseRewriteError::UnknownTypeTag(_) => ErrorCode::UnsupportedRequest,
            ResponseRewriteError::MissingTypeTag => ErrorCode::InternalError,
            ResponseRewriteError::Decode(_) | ResponseRewriteError::Encode(_) => {
                ErrorCode::InternalError
            }
        }
    }
}

impl std::fmt::Display for ResponseRewriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResponseRewriteError::Decode(err) => write!(f, "decode error: {err}"),
            ResponseRewriteError::Encode(err) => write!(f, "encode error: {err}"),
            ResponseRewriteError::UnknownTypeTag(tag) => {
                write!(f, "unknown response type tag: {tag}")
            }
            ResponseRewriteError::MissingTypeTag => {
                write!(f, "missing response type tag for success response")
            }
        }
    }
}

fn build_error_response(error_code: ErrorCode, request: &[u8]) -> Result<Vec<u8>, BinaryError> {
    let response = BinaryResponse::new_error(error_code);
    let response =
        BinaryResponseAndRequest::new(response, Bytes::from(request.to_vec())).to_bytes();
    response.map_err(|err| BinaryError::ResponseEncoding(err.to_string()))
}

fn rewrite_response_bytes(response_bytes: &[u8]) -> Result<Vec<u8>, ResponseRewriteError> {
    let mut envelope = parse_response_envelope(response_bytes)?;
    let response = envelope.response();
    if !response.is_success() {
        return serialize_response_envelope(envelope);
    }
    let payload = response.payload();
    let response_type_tag = match response.returned_data_type_tag() {
        Some(tag) => tag,
        None => {
            if payload.is_empty() {
                return serialize_response_envelope(envelope);
            }
            return Err(ResponseRewriteError::MissingTypeTag);
        }
    };
    let response_type = ResponseType::try_from(response_type_tag)
        .map_err(|_| ResponseRewriteError::UnknownTypeTag(response_type_tag))?;
    let new_response = match response_type {
        ResponseType::Peers => BinaryResponse::new_error(ErrorCode::FunctionDisabled),
        ResponseType::NodeStatus => {
            let payload = sanitize_node_status_payload(payload)?;
            BinaryResponse::from_raw_bytes(response_type, payload)
        }
        ResponseType::BlockHeaderV1 => roundtrip_response::<BlockHeaderV1>(response_type, payload)?,
        ResponseType::BlockHeader => roundtrip_response::<BlockHeader>(response_type, payload)?,
        ResponseType::BlockBodyV1 => roundtrip_response::<BlockBodyV1>(response_type, payload)?,
        ResponseType::BlockBody => roundtrip_response::<BlockBody>(response_type, payload)?,
        ResponseType::ApprovalsHashesV1 | ResponseType::ApprovalsHashes => {
            roundtrip_response::<OpaquePayload>(response_type, payload)?
        }
        ResponseType::BlockSignaturesV1 => {
            roundtrip_response::<BlockSignaturesV1>(response_type, payload)?
        }
        ResponseType::BlockSignatures => {
            roundtrip_response::<BlockSignatures>(response_type, payload)?
        }
        ResponseType::Deploy => roundtrip_response::<Deploy>(response_type, payload)?,
        ResponseType::Transaction => roundtrip_response::<Transaction>(response_type, payload)?,
        ResponseType::ExecutionResultV1 => {
            roundtrip_response::<ExecutionResultV1>(response_type, payload)?
        }
        ResponseType::ExecutionResult => {
            roundtrip_response::<ExecutionResult>(response_type, payload)?
        }
        ResponseType::WasmV1Result => roundtrip_response::<OpaquePayload>(response_type, payload)?,
        ResponseType::Transfers => roundtrip_response::<Vec<Transfer>>(response_type, payload)?,
        ResponseType::FinalizedDeployApprovals | ResponseType::FinalizedApprovals => {
            roundtrip_response::<Vec<Approval>>(response_type, payload)?
        }
        ResponseType::BlockWithSignatures => {
            roundtrip_response::<BlockWithSignatures>(response_type, payload)?
        }
        ResponseType::TransactionWithExecutionInfo => {
            roundtrip_response::<TransactionWithExecutionInfo>(response_type, payload)?
        }
        ResponseType::LastProgress => roundtrip_response::<LastProgress>(response_type, payload)?,
        ResponseType::ReactorState => {
            roundtrip_response::<ReactorStateName>(response_type, payload)?
        }
        ResponseType::NetworkName => roundtrip_response::<NetworkName>(response_type, payload)?,
        ResponseType::ConsensusValidatorChanges => {
            roundtrip_response::<ConsensusValidatorChanges>(response_type, payload)?
        }
        ResponseType::BlockSynchronizerStatus => {
            roundtrip_response::<BlockSynchronizerStatus>(response_type, payload)?
        }
        ResponseType::AvailableBlockRange => {
            roundtrip_response::<AvailableBlockRange>(response_type, payload)?
        }
        ResponseType::NextUpgrade => roundtrip_response::<NextUpgrade>(response_type, payload)?,
        ResponseType::ConsensusStatus => {
            roundtrip_response::<ConsensusStatus>(response_type, payload)?
        }
        ResponseType::ChainspecRawBytes => {
            roundtrip_response::<ChainspecRawBytes>(response_type, payload)?
        }
        ResponseType::Uptime => roundtrip_response::<Uptime>(response_type, payload)?,
        ResponseType::HighestBlockSequenceCheckResult => {
            roundtrip_response::<OpaquePayload>(response_type, payload)?
        }
        ResponseType::SpeculativeExecutionResult => {
            roundtrip_response::<SpeculativeExecutionResult>(response_type, payload)?
        }
        ResponseType::GlobalStateQueryResult => {
            roundtrip_response::<GlobalStateQueryResult>(response_type, payload)?
        }
        ResponseType::StoredValues => {
            roundtrip_response::<Vec<StoredValue>>(response_type, payload)?
        }
        ResponseType::GetTrieFullResult => {
            roundtrip_response::<GetTrieFullResult>(response_type, payload)?
        }
        ResponseType::DictionaryQueryResult => {
            roundtrip_response::<DictionaryQueryResult>(response_type, payload)?
        }
        ResponseType::BalanceResponse => {
            roundtrip_response::<BalanceResponse>(response_type, payload)?
        }
        ResponseType::Reward => roundtrip_response::<RewardResponse>(response_type, payload)?,
        ResponseType::ProtocolVersion => {
            roundtrip_response::<ProtocolVersion>(response_type, payload)?
        }
        ResponseType::ContractPackageWithProof => {
            roundtrip_response::<ValueWithProof<ContractPackage>>(response_type, payload)?
        }
        ResponseType::ContractInformation => {
            roundtrip_response::<ContractInformation>(response_type, payload)?
        }
        ResponseType::AccountInformation => {
            roundtrip_response::<AccountInformation>(response_type, payload)?
        }
        ResponseType::PackageWithProof => {
            roundtrip_response::<ValueWithProof<Package>>(response_type, payload)?
        }
        ResponseType::AddressableEntityInformation => {
            roundtrip_response::<AddressableEntityInformation>(response_type, payload)?
        }
    };
    envelope.set_response(new_response);
    serialize_response_envelope(envelope)
}

fn parse_response_envelope(
    response_bytes: &[u8],
) -> Result<ResponseEnvelope, ResponseRewriteError> {
    if let Ok((response_and_request, remainder)) =
        BinaryResponseAndRequest::from_bytes(response_bytes)
    {
        if !remainder.is_empty() {
            return Err(ResponseRewriteError::Decode(
                casper_types::bytesrepr::Error::LeftOverBytes,
            ));
        }
        return Ok(ResponseEnvelope::WithRequest {
            request: Bytes::from(response_and_request.request().to_vec()),
            response: BinaryResponse::from(response_and_request),
        });
    }
    let (response, remainder) =
        BinaryResponse::from_bytes(response_bytes).map_err(ResponseRewriteError::Decode)?;
    if !remainder.is_empty() {
        return Err(ResponseRewriteError::Decode(
            casper_types::bytesrepr::Error::LeftOverBytes,
        ));
    }
    Ok(ResponseEnvelope::Response { response })
}

fn serialize_response_envelope(
    envelope: ResponseEnvelope,
) -> Result<Vec<u8>, ResponseRewriteError> {
    match envelope {
        ResponseEnvelope::WithRequest { request, response } => {
            BinaryResponseAndRequest::new(response, request)
                .to_bytes()
                .map_err(ResponseRewriteError::Encode)
        }
        ResponseEnvelope::Response { response } => {
            response.to_bytes().map_err(ResponseRewriteError::Encode)
        }
    }
}

fn roundtrip_response<T>(
    response_type: ResponseType,
    payload: &[u8],
) -> Result<BinaryResponse, ResponseRewriteError>
where
    T: FromBytes + ToBytes,
{
    let payload = roundtrip_payload::<T>(payload)?;
    Ok(BinaryResponse::from_raw_bytes(response_type, payload))
}

fn roundtrip_payload<T>(payload: &[u8]) -> Result<Vec<u8>, ResponseRewriteError>
where
    T: FromBytes + ToBytes,
{
    let (value, remainder) = T::from_bytes(payload).map_err(ResponseRewriteError::Decode)?;
    if !remainder.is_empty() {
        return Err(ResponseRewriteError::Decode(
            casper_types::bytesrepr::Error::LeftOverBytes,
        ));
    }
    value.to_bytes().map_err(ResponseRewriteError::Encode)
}

fn sanitize_node_status_payload(payload: &[u8]) -> Result<Vec<u8>, ResponseRewriteError> {
    let (mut status, remainder) =
        NodeStatus::from_bytes(payload).map_err(ResponseRewriteError::Decode)?;
    if !remainder.is_empty() {
        return Err(ResponseRewriteError::Decode(
            casper_types::bytesrepr::Error::LeftOverBytes,
        ));
    }
    status.peers = Peers::from(BTreeMap::<String, String>::new());
    status.to_bytes().map_err(ResponseRewriteError::Encode)
}

fn log_binary_response(address: &str, request_id: Option<u16>, response: &[u8]) {
    if let Ok((response_and_request, remainder)) = BinaryResponseAndRequest::from_bytes(response) {
        if !remainder.is_empty() {
            warn!(
                %address,
                request_id,
                remaining_bytes = remainder.len(),
                "binary response contained trailing bytes"
            );
        }
        let response = response_and_request.response();
        debug!(
            %address,
            request_id,
            response_success = response.is_success(),
            response_error_code = response.error_code(),
            response_payload_type = ?response.returned_data_type_tag(),
            response_payload_len = response.payload().len(),
            response_request_len = response_and_request.request().len(),
            "binary port response"
        );
        return;
    }

    let (response, remainder) = match BinaryResponse::from_bytes(response) {
        Ok((response, remainder)) => (response, remainder),
        Err(err) => {
            warn!(%address, request_id, "failed to decode binary response: {err}");
            return;
        }
    };
    if !remainder.is_empty() {
        warn!(
            %address,
            request_id,
            remaining_bytes = remainder.len(),
            "binary response contained trailing bytes"
        );
    }
    debug!(
        %address,
        request_id,
        response_success = response.is_success(),
        response_error_code = response.error_code(),
        response_payload_type = ?response.returned_data_type_tag(),
        response_payload_len = response.payload().len(),
        "binary port response"
    );
}
