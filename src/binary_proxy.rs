//! Binary port proxy with a small pool of serialized upstream connections.

use std::sync::Arc;
use std::time::Instant;

use casper_binary_port::{
    BinaryMessage, BinaryResponse, BinaryResponseAndRequest, Command, CommandHeader, CommandTag,
};
use casper_types::bytesrepr::FromBytes;
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
        let request_id = log_binary_request(address, &payload);
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
            log_binary_response(address, request_id, response);
            self.return_connection(connection).await;
            self.metrics
                .increment_binary_response(self.network.as_ref(), "ok");
        } else if let Err(err) = &result {
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

fn log_binary_request(address: &str, payload: &[u8]) -> Option<u16> {
    let payload_len = payload.len();
    let (header, remainder) = match CommandHeader::from_bytes(payload) {
        Ok((header, remainder)) => (header, remainder),
        Err(err) => {
            warn!(
                %address,
                payload_len,
                "failed to decode binary request header: {err}"
            );
            return None;
        }
    };
    let tag = match CommandTag::try_from(header.type_tag()) {
        Ok(tag) => tag,
        Err(_) => {
            warn!(
                %address,
                request_id = header.id(),
                payload_len,
                "invalid binary request tag"
            );
            return Some(header.id());
        }
    };
    let command = match Command::try_from((tag, remainder)) {
        Ok(command) => command,
        Err(err) => {
            warn!(
                %address,
                request_id = header.id(),
                payload_len,
                "failed to decode binary request: {err}"
            );
            return Some(header.id());
        }
    };
    debug!(
        %address,
        request_id = header.id(),
        request_version = header.version(),
        request_tag = ?command.tag(),
        request = ?command,
        request_payload_len = payload_len,
        "binary port request"
    );
    Some(header.id())
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
