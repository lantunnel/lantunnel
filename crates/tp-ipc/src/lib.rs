//! Local IPC between the Lantunnel desktop GUI and its background service.
//!
//! Wire format:
//!   * 4-byte big-endian length header
//!   * JSON body
//!   * Max frame: 1 MiB
//!
//! Message shapes:
//!   * Request:  `{"id": <u64>, "method": "<name>", "params": <json>}`
//!   * Response: `{"id": <u64>, "error": "<string|null>", "result": <json|null>}`
//!   * Event:    `{"event": {"kind": "<name>", "payload": <json>}}` (server-push)
//!
//! Transport: Unix domain socket (`/tmp/lan-client.sock`, 0600) on
//! macOS/Linux; Windows named pipe (`\\.\pipe\lan-client`) on
//! Windows. Socket path available via [`default_socket_path`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, Mutex};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME_SIZE: usize = 1 << 20;
// Frozen runtime identifier, not a product name. It is the socket a running
// Client already owns, so renaming it would break the CLI against an
// instance started before the upgrade. `lan-client` is on the avoid-list in
// CONTEXT.md for prose; this is the one place it legitimately survives.
pub const DEFAULT_SOCKET_NAME: &str = "lan-client";

#[cfg(unix)]
pub fn default_socket_path() -> PathBuf {
    PathBuf::from(format!("/tmp/{DEFAULT_SOCKET_NAME}.sock"))
}

#[cfg(windows)]
pub fn default_socket_path() -> PathBuf {
    PathBuf::from(format!("\\\\.\\pipe\\{DEFAULT_SOCKET_NAME}"))
}

/// Method identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Hello,
    Connect,
    Disconnect,
    GetStatus,
    SubscribeStatus,
    SaveCredentials,
    LoadCredentials,
    GetSettings,
    SaveSettings,
    GetLogs,
    SetLogLevel,
    GetLogConfig,
    GetLogFilePath,
    ClearLogs,
    CurrentPlatformURL,
    ValidateAutoStart,
    Ping,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Hello => "Hello",
            Method::Connect => "Connect",
            Method::Disconnect => "Disconnect",
            Method::GetStatus => "GetStatus",
            Method::SubscribeStatus => "SubscribeStatus",
            Method::SaveCredentials => "SaveCredentials",
            Method::LoadCredentials => "LoadCredentials",
            Method::GetSettings => "GetSettings",
            Method::SaveSettings => "SaveSettings",
            Method::GetLogs => "GetLogs",
            Method::SetLogLevel => "SetLogLevel",
            Method::GetLogConfig => "GetLogConfig",
            Method::GetLogFilePath => "GetLogFilePath",
            Method::ClearLogs => "ClearLogs",
            Method::CurrentPlatformURL => "CurrentPlatformURL",
            Method::ValidateAutoStart => "ValidateAutoStart",
            Method::Ping => "Ping",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HelloParams {
    pub protocol_version: u32,
    pub client_version: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct HelloResult {
    pub protocol_version: u32,
    pub server_version: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Frame {
    Request {
        id: u64,
        method: String,
        #[serde(default)]
        params: serde_json::Value,
    },
    Response {
        id: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        #[serde(default)]
        result: serde_json::Value,
    },
    Event {
        event: EventBody,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct EventBody {
    pub kind: String,
    #[serde(default)]
    pub payload: serde_json::Value,
}

#[derive(thiserror::Error, Debug)]
pub enum IpcError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("frame too large: {0} bytes")]
    FrameTooLarge(u32),
    #[error("closed")]
    Closed,
    #[error("protocol version mismatch: got {0}, want {1}")]
    VersionMismatch(u32, u32),
    #[error("remote error: {0}")]
    Remote(String),
    #[error("timeout")]
    Timeout,
}

pub type Result<T> = std::result::Result<T, IpcError>;

pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Frame> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    if len as usize > MAX_FRAME_SIZE {
        return Err(IpcError::FrameTooLarge(len));
    }
    let mut body = vec![0u8; len as usize];
    reader.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, frame: &Frame) -> Result<()> {
    let body = serde_json::to_vec(frame)?;
    if body.len() > MAX_FRAME_SIZE {
        return Err(IpcError::FrameTooLarge(body.len() as u32));
    }
    let hdr = (body.len() as u32).to_be_bytes();
    writer.write_all(&hdr).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

// ---- client --------------------------------------------------------------

pub struct IpcClient {
    next_id: AtomicU64,
    pending: Arc<Mutex<PendingCalls>>,
    out_tx: mpsc::Sender<Frame>,
    events_rx: Arc<Mutex<mpsc::Receiver<EventBody>>>,
    call_timeout: Duration,
}

#[derive(Default)]
struct PendingCalls {
    closed: bool,
    calls: HashMap<u64, oneshot::Sender<Frame>>,
}

async fn close_pending(pending: &Arc<Mutex<PendingCalls>>) {
    let mut pending = pending.lock().await;
    pending.closed = true;
    pending.calls.clear();
}

impl IpcClient {
    pub async fn connect(path: impl Into<PathBuf>, client_version: &str) -> Result<Self> {
        let path = path.into();
        let (read_half, write_half) = platform::connect_client(path).await?;
        let (out_tx, mut out_rx) = mpsc::channel::<Frame>(64);
        let (event_tx, event_rx) = mpsc::channel::<EventBody>(64);
        let pending = Arc::new(Mutex::new(PendingCalls::default()));

        let pending_for_writer = pending.clone();
        let mut writer = write_half;
        tokio::spawn(async move {
            while let Some(frame) = out_rx.recv().await {
                if write_frame(&mut writer, &frame).await.is_err() {
                    break;
                }
            }
            close_pending(&pending_for_writer).await;
        });

        let pending_for_reader = pending.clone();
        let mut reader = read_half;
        tokio::spawn(async move {
            loop {
                let frame = match read_frame(&mut reader).await {
                    Ok(f) => f,
                    Err(_) => break,
                };
                match frame {
                    Frame::Response { id, .. } => {
                        if let Some(tx) = pending_for_reader.lock().await.calls.remove(&id) {
                            let _ = tx.send(frame);
                        }
                    }
                    Frame::Event { event } => {
                        if event_tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    Frame::Request { .. } => {}
                }
            }
            close_pending(&pending_for_reader).await;
        });

        let client = Self {
            next_id: AtomicU64::new(1),
            pending,
            out_tx,
            events_rx: Arc::new(Mutex::new(event_rx)),
            call_timeout: Duration::from_secs(30),
        };

        let params = serde_json::to_value(HelloParams {
            protocol_version: PROTOCOL_VERSION,
            client_version: client_version.to_string(),
        })?;
        let value = client.call(Method::Hello.as_str(), params).await?;
        let result: HelloResult = serde_json::from_value(value)?;
        if result.protocol_version != PROTOCOL_VERSION {
            return Err(IpcError::VersionMismatch(
                result.protocol_version,
                PROTOCOL_VERSION,
            ));
        }

        Ok(client)
    }

    pub async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            if pending.closed {
                return Err(IpcError::Closed);
            }
            pending.calls.insert(id, tx);
        }
        let req = Frame::Request {
            id,
            method: method.to_string(),
            params,
        };
        if self.out_tx.send(req).await.is_err() {
            close_pending(&self.pending).await;
            return Err(IpcError::Closed);
        }
        match tokio::time::timeout(self.call_timeout, rx).await {
            Ok(Ok(Frame::Response { error: Some(e), .. })) => Err(IpcError::Remote(e)),
            Ok(Ok(Frame::Response { result, .. })) => Ok(result),
            Ok(Ok(_)) => Err(IpcError::Remote("unexpected frame".into())),
            Ok(Err(_)) => {
                self.pending.lock().await.calls.remove(&id);
                Err(IpcError::Closed)
            }
            Err(_) => {
                self.pending.lock().await.calls.remove(&id);
                Err(IpcError::Timeout)
            }
        }
    }

    pub fn events(&self) -> Arc<Mutex<mpsc::Receiver<EventBody>>> {
        self.events_rx.clone()
    }
}

// ---- server --------------------------------------------------------------

#[async_trait::async_trait]
pub trait IpcHandler: Send + Sync + 'static {
    async fn handle(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> std::result::Result<serde_json::Value, String>;
}

#[derive(Clone)]
pub struct EventBroadcaster {
    subs: Arc<dashmap::DashMap<u64, mpsc::Sender<EventBody>>>,
    next: Arc<AtomicU64>,
}

impl EventBroadcaster {
    pub fn new() -> Self {
        Self {
            subs: Arc::new(dashmap::DashMap::new()),
            next: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn subscribe(&self) -> (u64, mpsc::Receiver<EventBody>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(64);
        self.subs.insert(id, tx);
        (id, rx)
    }

    pub fn unsubscribe(&self, id: u64) {
        self.subs.remove(&id);
    }

    pub fn broadcast(&self, event: EventBody) {
        let snapshot: Vec<_> = self
            .subs
            .iter()
            .map(|e| (*e.key(), e.value().clone()))
            .collect();
        for (id, tx) in snapshot {
            let ev = EventBody {
                kind: event.kind.clone(),
                payload: event.payload.clone(),
            };
            if tx.try_send(ev).is_err() {
                self.subs.remove(&id);
            }
        }
    }
}

impl Default for EventBroadcaster {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn serve<H: IpcHandler>(
    path: impl Into<PathBuf>,
    handler: Arc<H>,
    events: EventBroadcaster,
) -> Result<()> {
    let path = path.into();
    platform::serve(path, handler, events).await
}

// ---- platform-specific transport ----------------------------------------

#[cfg(unix)]
mod platform {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::{ReadHalf, WriteHalf};
    use tokio::net::{UnixListener, UnixStream};

    struct SocketCleanup(PathBuf);

    impl Drop for SocketCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    pub async fn connect_client(
        path: PathBuf,
    ) -> Result<(ReadHalf<UnixStream>, WriteHalf<UnixStream>)> {
        let stream = UnixStream::connect(&path).await?;
        let (r, w) = tokio::io::split(stream);
        Ok((r, w))
    }

    pub async fn serve<H: IpcHandler>(
        path: PathBuf,
        handler: Arc<H>,
        events: EventBroadcaster,
    ) -> Result<()> {
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let _cleanup = SocketCleanup(path);
        loop {
            let (stream, _) = listener.accept().await?;
            let handler = handler.clone();
            let events = events.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(stream, handler, events).await {
                    tracing::debug!(error = %e, "ipc connection ended");
                }
            });
        }
    }

    async fn handle_connection<H: IpcHandler>(
        stream: UnixStream,
        handler: Arc<H>,
        events: EventBroadcaster,
    ) -> Result<()> {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let (out_tx, mut out_rx) = mpsc::channel::<Frame>(64);
        let writer_task = tokio::spawn(async move {
            while let Some(f) = out_rx.recv().await {
                if super::write_frame(&mut writer, &f).await.is_err() {
                    break;
                }
            }
        });
        let (sub_id, mut event_rx) = events.subscribe();
        let out_tx_events = out_tx.clone();
        let event_pump = tokio::spawn(async move {
            while let Some(ev) = event_rx.recv().await {
                if out_tx_events
                    .send(Frame::Event { event: ev })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        loop {
            let frame = match super::read_frame(&mut reader).await {
                Ok(f) => f,
                Err(_) => break,
            };
            let Frame::Request { id, method, params } = frame else {
                continue;
            };
            let handler = handler.clone();
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let resp = match method.as_str() {
                    "Hello" => match serde_json::to_value(HelloResult {
                        protocol_version: PROTOCOL_VERSION,
                        server_version: env!("CARGO_PKG_VERSION").to_string(),
                    }) {
                        Ok(v) => Ok(v),
                        Err(e) => Err(e.to_string()),
                    },
                    _ => handler.handle(&method, params).await,
                };
                let frame = match resp {
                    Ok(v) => Frame::Response {
                        id,
                        error: None,
                        result: v,
                    },
                    Err(e) => Frame::Response {
                        id,
                        error: Some(e),
                        result: serde_json::Value::Null,
                    },
                };
                let _ = out_tx.send(frame).await;
            });
        }
        events.unsubscribe(sub_id);
        drop(out_tx);
        let _ = event_pump.await;
        let _ = writer_task.await;
        Ok(())
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ffi::c_void;
    use std::io;
    use tokio::io::{ReadHalf, WriteHalf};
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

    struct LocalSecurityDescriptor(*mut c_void);

    impl Drop for LocalSecurityDescriptor {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }

    fn owner_only_security_attributes() -> io::Result<(SECURITY_ATTRIBUTES, LocalSecurityDescriptor)>
    {
        // Protected DACL: LocalSystem and the object owner only. The Owner
        // Rights SID (OW) resolves against the current user's default owner
        // assigned by CreateNamedPipeW.
        let sddl: Vec<u16> = "D:P(A;;GA;;;SY)(A;;GA;;;OW)\0".encode_utf16().collect();
        let mut descriptor = std::ptr::null_mut();
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if converted == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((
            SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            },
            LocalSecurityDescriptor(descriptor),
        ))
    }

    pub async fn connect_client(
        path: PathBuf,
    ) -> Result<(
        ReadHalf<tokio::net::windows::named_pipe::NamedPipeClient>,
        WriteHalf<tokio::net::windows::named_pipe::NamedPipeClient>,
    )> {
        let name = path.to_string_lossy().to_string();
        let stream = ClientOptions::new().open(name)?;
        let (r, w) = tokio::io::split(stream);
        Ok((r, w))
    }

    pub async fn serve<H: IpcHandler>(
        path: PathBuf,
        handler: Arc<H>,
        events: EventBroadcaster,
    ) -> Result<()> {
        let name = path.to_string_lossy().to_string();
        loop {
            // CreateNamedPipeW consumes the attributes synchronously, so the
            // descriptor can be freed before awaiting a client. Keeping its
            // raw pointers across the await would make this server future
            // non-Send and prevent both the UI and headless runtimes from
            // spawning it on Windows.
            let server = {
                let (mut security_attributes, _security_descriptor) =
                    owner_only_security_attributes()?;
                unsafe {
                    ServerOptions::new().create_with_security_attributes_raw(
                        &name,
                        (&mut security_attributes as *mut SECURITY_ATTRIBUTES).cast(),
                    )?
                }
            };
            server.connect().await?;
            let handler = handler.clone();
            let events = events.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connection(server, handler, events).await {
                    tracing::debug!(error = %e, "ipc connection ended");
                }
            });
        }
    }

    async fn handle_connection<H: IpcHandler>(
        stream: NamedPipeServer,
        handler: Arc<H>,
        events: EventBroadcaster,
    ) -> Result<()> {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let (out_tx, mut out_rx) = mpsc::channel::<Frame>(64);
        let writer_task = tokio::spawn(async move {
            while let Some(f) = out_rx.recv().await {
                if super::write_frame(&mut writer, &f).await.is_err() {
                    break;
                }
            }
        });
        let (sub_id, mut event_rx) = events.subscribe();
        let out_tx_events = out_tx.clone();
        let event_pump = tokio::spawn(async move {
            while let Some(ev) = event_rx.recv().await {
                if out_tx_events
                    .send(Frame::Event { event: ev })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        loop {
            let frame = match super::read_frame(&mut reader).await {
                Ok(f) => f,
                Err(_) => break,
            };
            let Frame::Request { id, method, params } = frame else {
                continue;
            };
            let handler = handler.clone();
            let out_tx = out_tx.clone();
            tokio::spawn(async move {
                let resp = match method.as_str() {
                    "Hello" => match serde_json::to_value(HelloResult {
                        protocol_version: PROTOCOL_VERSION,
                        server_version: env!("CARGO_PKG_VERSION").to_string(),
                    }) {
                        Ok(v) => Ok(v),
                        Err(e) => Err(e.to_string()),
                    },
                    _ => handler.handle(&method, params).await,
                };
                let frame = match resp {
                    Ok(v) => Frame::Response {
                        id,
                        error: None,
                        result: v,
                    },
                    Err(e) => Frame::Response {
                        id,
                        error: Some(e),
                        result: serde_json::Value::Null,
                    },
                };
                let _ = out_tx.send(frame).await;
            });
        }
        events.unsubscribe(sub_id);
        drop(out_tx);
        let _ = event_pump.await;
        let _ = writer_task.await;
        Ok(())
    }

    #[allow(dead_code)]
    fn assert_server_future_is_send<H: IpcHandler>(
        path: PathBuf,
        handler: Arc<H>,
        events: EventBroadcaster,
    ) {
        fn assert_send<T: Send>(_: T) {}
        assert_send(serve(path, handler, events));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    struct TestSocket(PathBuf);

    #[cfg(unix)]
    impl TestSocket {
        fn unique() -> Self {
            static NEXT_SOCKET: AtomicU64 = AtomicU64::new(1);
            let suffix = NEXT_SOCKET.fetch_add(1, Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!(
                "tp-ipc-disconnect-{}-{suffix}.sock",
                std::process::id()
            )))
        }
    }

    #[cfg(unix)]
    impl Drop for TestSocket {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[tokio::test]
    async fn frame_roundtrip() {
        use std::io::Cursor;
        let frame = Frame::Request {
            id: 42,
            method: "Ping".into(),
            params: serde_json::json!({"nonce": 1}),
        };
        let mut buf: Vec<u8> = Vec::new();
        write_frame(&mut buf, &frame).await.unwrap();
        let mut cursor = Cursor::new(buf);
        let back = read_frame(&mut cursor).await.unwrap();
        match back {
            Frame::Request { id, method, .. } => {
                assert_eq!(id, 42);
                assert_eq!(method, "Ping");
            }
            _ => panic!("wrong frame"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn server_disconnect_closes_all_pending_calls_without_waiting_for_timeout() {
        use tokio::net::UnixListener;

        let socket = TestSocket::unique();
        let listener = UnixListener::bind(&socket.0).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let hello_id = match read_frame(&mut stream).await.unwrap() {
                Frame::Request { id, method, .. } if method == Method::Hello.as_str() => id,
                frame => panic!("expected Hello request, got {frame:?}"),
            };
            write_frame(
                &mut stream,
                &Frame::Response {
                    id: hello_id,
                    error: None,
                    result: serde_json::to_value(HelloResult {
                        protocol_version: PROTOCOL_VERSION,
                        server_version: "test".into(),
                    })
                    .unwrap(),
                },
            )
            .await
            .unwrap();

            for _ in 0..2 {
                match read_frame(&mut stream).await.unwrap() {
                    Frame::Request { method, .. } if method == Method::Ping.as_str() => {}
                    frame => panic!("expected Ping request, got {frame:?}"),
                }
            }
            // Dropping the accepted stream simulates the IPC server disappearing
            // while multiple calls are in flight.
        });

        let client = Arc::new(IpcClient::connect(&socket.0, "test").await.unwrap());
        let first = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .call(Method::Ping.as_str(), serde_json::Value::Null)
                    .await
            })
        };
        let second = {
            let client = client.clone();
            tokio::spawn(async move {
                client
                    .call(Method::Ping.as_str(), serde_json::Value::Null)
                    .await
            })
        };

        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("server did not receive both requests")
            .unwrap();
        let (first, second) = tokio::time::timeout(Duration::from_secs(1), async {
            (first.await.unwrap(), second.await.unwrap())
        })
        .await
        .expect("pending calls waited for the 30 second call timeout");

        assert!(matches!(first, Err(IpcError::Closed)));
        assert!(matches!(second, Err(IpcError::Closed)));

        let after_disconnect = tokio::time::timeout(
            Duration::from_millis(100),
            client.call(Method::Ping.as_str(), serde_json::Value::Null),
        )
        .await
        .expect("a call started after reader shutdown waited instead of closing");
        assert!(matches!(after_disconnect, Err(IpcError::Closed)));
    }

    #[tokio::test]
    async fn closed_output_channel_does_not_retain_the_failed_call() {
        let pending = Arc::new(Mutex::new(PendingCalls::default()));
        let (out_tx, out_rx) = mpsc::channel(1);
        drop(out_rx);
        let (_event_tx, event_rx) = mpsc::channel(1);
        let client = IpcClient {
            next_id: AtomicU64::new(1),
            pending: pending.clone(),
            out_tx,
            events_rx: Arc::new(Mutex::new(event_rx)),
            call_timeout: Duration::from_secs(30),
        };

        let result = client
            .call(Method::Ping.as_str(), serde_json::Value::Null)
            .await;

        assert!(matches!(result, Err(IpcError::Closed)));
        let pending = pending.lock().await;
        assert!(
            pending.calls.is_empty(),
            "failed call leaked in pending map"
        );
        assert!(pending.closed, "closed output channel was not remembered");
    }

    #[tokio::test]
    async fn open_connection_without_response_reports_timeout_not_closed() {
        let pending = Arc::new(Mutex::new(PendingCalls::default()));
        let (out_tx, _out_rx) = mpsc::channel(1);
        let (_event_tx, event_rx) = mpsc::channel(1);
        let client = IpcClient {
            next_id: AtomicU64::new(1),
            pending: pending.clone(),
            out_tx,
            events_rx: Arc::new(Mutex::new(event_rx)),
            call_timeout: Duration::from_millis(10),
        };

        let result = client
            .call(Method::Ping.as_str(), serde_json::Value::Null)
            .await;

        assert!(matches!(result, Err(IpcError::Timeout)));
        let pending = pending.lock().await;
        assert!(
            pending.calls.is_empty(),
            "timed out call leaked in pending map"
        );
        assert!(
            !pending.closed,
            "a response timeout closed a live transport"
        );
    }
}
