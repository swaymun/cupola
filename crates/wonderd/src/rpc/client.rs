//! A small Wonder RPC client over the WebSocket. The `wonder` CLI uses it
//! with the loopback capability; tests use it against a real router.
use super::*;
use futures_util::{stream::SplitSink, SinkExt, StreamExt};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc, Mutex,
    },
};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::{
    tungstenite::{client::IntoClientRequest, http::HeaderValue, Message},
    MaybeTlsStream, WebSocketStream,
};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Response>>>>;

#[derive(Debug)]
pub enum CallError {
    /// The host answered with an error.
    Rpc(RpcError),
    /// The connection closed before the answer.
    Closed,
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rpc(error) => f.write_str(&error.message),
            Self::Closed => f.write_str("The connection to Cupola closed."),
        }
    }
}

impl std::error::Error for CallError {}

pub struct Client {
    /// The socket's sending half, with the sealer on `/api/v1/rpc/sealed`.
    sink: tokio::sync::Mutex<(SplitSink<Socket, Message>, Option<sealed::Sealer>)>,
    pending: Pending,
    next_id: AtomicI64,
    notifications: tokio::sync::Mutex<mpsc::UnboundedReceiver<Notification>>,
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for Client {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl Client {
    /// Connects to `url` (`ws://host:port/api/v1/rpc`) with extra headers,
    /// such as `x-wonder-loopback-capability`.
    pub async fn connect(
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<Self, tokio_tungstenite::tungstenite::Error> {
        let mut request = url.into_client_request()?;
        for (name, value) in headers {
            let name =
                tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(name.as_bytes())
                    .map_err(tokio_tungstenite::tungstenite::http::Error::from)?;
            let value = HeaderValue::from_str(value)
                .map_err(tokio_tungstenite::tungstenite::http::Error::from)?;
            request.headers_mut().insert(name, value);
        }
        let (socket, _) = tokio_tungstenite::connect_async(request).await?;
        Ok(Self::over(socket, None))
    }

    /// Connects to `url` (`wss://host/api/v1/rpc/sealed`) as a paired device:
    /// checks the host's proof against `pin` and `origin`, then sends
    /// `session` inside the sealed channel. No cookie or other credential
    /// crosses the connection in the clear. Then call `auth.challenge` and
    /// `initialize` as on `/api/v1/rpc`. The phone is the real client; this
    /// one drives the host's sealed route in tests.
    #[cfg(test)]
    pub async fn connect_sealed(
        url: &str,
        origin: &str,
        pin: &str,
        session: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let (mut socket, mut sealer, opener) = Self::sealed_handshake(url, origin, pin).await?;
        let opening = serde_json::to_vec(&sealed::Opening::Session(session.into()))?;
        socket.send(Message::binary(sealer.seal(&opening)?)).await?;
        Ok(Self::over(socket, Some((sealer, opener))))
    }

    /// Like [`Client::connect_sealed`] for a device whose session has ended:
    /// renews it inside the channel, answering the host's challenge with what
    /// `answer` returns (a [`sealed::RenewAnswer`] unless a test means to
    /// misbehave), and returns the new session too.
    #[cfg(test)]
    pub async fn renew_sealed(
        url: &str,
        origin: &str,
        pin: &str,
        device_id: &str,
        answer: impl FnOnce(&wonder_api::pairing_protocol::Challenge) -> Value,
    ) -> Result<(Self, sealed::RenewedSession), Box<dyn std::error::Error + Send + Sync>> {
        let (mut socket, mut sealer, mut opener) = Self::sealed_handshake(url, origin, pin).await?;
        let opening = serde_json::to_vec(&sealed::Opening::Renew(sealed::RenewRequest {
            device_id: device_id.into(),
        }))?;
        socket.send(Message::binary(sealer.seal(&opening)?)).await?;
        let sealed::Renewal::Challenge(challenge) =
            Self::read_renewal(&mut socket, &mut opener).await?
        else {
            return Err("the host renewed without a challenge".into());
        };
        let answer = serde_json::to_vec(&answer(&challenge))?;
        socket.send(Message::binary(sealer.seal(&answer)?)).await?;
        let sealed::Renewal::Renewed(renewed) =
            Self::read_renewal(&mut socket, &mut opener).await?
        else {
            return Err("the host sent a second challenge".into());
        };
        Ok((Self::over(socket, Some((sealer, opener))), renewed))
    }

    #[cfg(test)]
    async fn read_renewal(
        socket: &mut Socket,
        opener: &mut sealed::Opener,
    ) -> Result<sealed::Renewal, Box<dyn std::error::Error + Send + Sync>> {
        loop {
            match socket.next().await {
                Some(Ok(Message::Binary(frame))) => {
                    let plain = opener.open(&frame)?;
                    // Anything else is a refusal: the host's JSON-RPC error.
                    return serde_json::from_slice(&plain)
                        .map_err(|_| String::from_utf8_lossy(&plain).into_owned().into());
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                _ => return Err("the host closed the connection during renewal".into()),
            }
        }
    }

    /// The two handshake messages: checks the host's proof, opens the channel.
    #[cfg(test)]
    async fn sealed_handshake(
        url: &str,
        origin: &str,
        pin: &str,
    ) -> Result<(Socket, sealed::Sealer, sealed::Opener), Box<dyn std::error::Error + Send + Sync>>
    {
        let mut request = url.into_client_request()?;
        request
            .headers_mut()
            .insert("origin", HeaderValue::from_str(origin)?);
        // The route is the origin's address, as an edge in front of the host
        // forwards it, even when `url` reaches that host another way (a test
        // or a local proxy).
        if let Some(authority) = origin.strip_prefix("https://") {
            request
                .headers_mut()
                .insert("host", HeaderValue::from_str(authority)?);
        }
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await?;
        let initiator = sealed::Initiator::new()?;
        socket.send(Message::text(initiator.hello())).await?;
        let welcome = loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => break text,
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                _ => return Err("the host closed the connection during the handshake".into()),
            }
        };
        let (sealer, opener) = initiator.finish(welcome.as_str(), pin, origin)?;
        Ok((socket, sealer, opener))
    }

    fn over(socket: Socket, sealed: Option<(sealed::Sealer, sealed::Opener)>) -> Self {
        let (sealer, mut opener) = sealed.unzip();
        let (sink, mut stream) = socket.split();
        let pending: Pending = Arc::default();
        let (notify, notifications) = mpsc::unbounded_channel();
        let waiting = pending.clone();
        let reader = tokio::spawn(async move {
            while let Some(Ok(message)) = stream.next().await {
                let text = match (message, opener.as_mut()) {
                    (Message::Text(text), None) => text.as_str().to_owned(),
                    (Message::Binary(frame), Some(opener)) => {
                        // A frame that does not open ends the connection.
                        let Ok(text) = opener
                            .open(&frame)
                            .map_err(|_| ())
                            .and_then(|plain| String::from_utf8(plain).map_err(|_| ()))
                        else {
                            break;
                        };
                        text
                    }
                    // Plaintext on a sealed connection is not the host's.
                    (Message::Text(_), Some(_)) => break,
                    _ => continue,
                };
                let Ok(value) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };
                if value.get("method").is_some() {
                    if let Ok(notification) = serde_json::from_value::<Notification>(value) {
                        let _ = notify.send(notification);
                    }
                    continue;
                }
                let Ok(response) = serde_json::from_value::<Response>(value) else {
                    continue;
                };
                if let Some(RequestId::Number(id)) = response.id {
                    let sender = waiting
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&id);
                    if let Some(sender) = sender {
                        let _ = sender.send(response);
                    }
                }
            }
            // Fail every waiting call rather than leaving it hanging.
            waiting.lock().unwrap_or_else(|e| e.into_inner()).clear();
        });
        Self {
            sink: tokio::sync::Mutex::new((sink, sealer)),
            pending,
            next_id: AtomicI64::new(1),
            notifications: tokio::sync::Mutex::new(notifications),
            reader,
        }
    }

    /// Whether the connection has closed.
    pub fn is_closed(&self) -> bool {
        self.reader.is_finished()
    }

    /// Calls `method` and waits for its answer. Wrap it in a timeout; the
    /// host itself never gives up on a route.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, CallError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, sender);
        let request = Request {
            jsonrpc: JSONRPC.into(),
            id: Some(RequestId::Number(id)),
            method: method.into(),
            params: (!params.is_null()).then_some(params),
        };
        let text = serde_json::to_string(&request).unwrap_or_default();
        let sent = {
            let mut sink = self.sink.lock().await;
            let (sink, sealer) = &mut *sink;
            // Sealed in send order: the lock keeps nonces and frames aligned.
            let message = match sealer {
                Some(sealer) => sealer.seal(text.as_bytes()).map(Message::binary),
                None => Ok(Message::Text(text.into())),
            };
            match message {
                Ok(message) => sink.send(message).await.is_ok(),
                Err(_) => false,
            }
        };
        if !sent {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return Err(CallError::Closed);
        }
        let response = receiver.await.map_err(|_| CallError::Closed)?;
        match (response.result, response.error) {
            (_, Some(error)) => Err(CallError::Rpc(error)),
            (Some(result), None) => Ok(result),
            (None, None) => Ok(Value::Null),
        }
    }

    /// `initialize` as the local owner (no device proof).
    pub async fn initialize(&self, name: &str) -> Result<InitializeResult, CallError> {
        let result = self
            .call(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "clientInfo": { "name": name, "version": env!("CARGO_PKG_VERSION") },
                }),
            )
            .await?;
        serde_json::from_value(result).map_err(|error| {
            CallError::Rpc(RpcError::new(
                codes::INTERNAL_ERROR,
                format!("Unreadable initialize result: {error}"),
            ))
        })
    }

    /// The next notification, or `None` once the connection closed.
    pub async fn notification(&self) -> Option<Notification> {
        self.notifications.lock().await.recv().await
    }
}
