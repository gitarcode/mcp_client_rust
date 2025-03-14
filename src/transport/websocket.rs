use async_trait::async_trait;
use futures::{Stream, StreamExt};
use futures_util::sink::SinkExt;
use http;
use std::{collections::HashMap, pin::Pin, sync::Arc, time::Duration};
use tokio::sync::{broadcast, Mutex};
use tokio_tungstenite::{
    connect_async_with_config, tungstenite::protocol::WebSocketConfig,
    tungstenite::Message as WsMessage, MaybeTlsStream, WebSocketStream,
};
use url::Url;

use crate::{
    error::Error,
    transport::{Message, Transport},
};

/// Default ping interval in seconds
const DEFAULT_PING_INTERVAL_SECS: u64 = 30;

/// A transport that uses WebSockets for MCP communication.
pub struct WebSocketTransport {
    /// A mutex-protected writer for sending messages.
    writer: Arc<Mutex<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>>,
    /// A broadcast receiver for incoming messages.
    receiver: broadcast::Receiver<Result<Message, Error>>,
    /// Keep sender in scope to avoid dropping.
    _sender: broadcast::Sender<Result<Message, Error>>,
    /// Flag to track if we should stop the ping task
    ping_stop: Arc<Mutex<bool>>,
}

impl WebSocketTransport {
    /// Creates a new WebSocketTransport by connecting to the specified URL.
    ///
    /// # Errors
    ///
    /// Returns an `Error` if the connection fails.
    pub async fn new(url: impl AsRef<str>) -> Result<Self, Error> {
        Self::with_headers(url, None).await
    }

    /// Creates a new WebSocketTransport by connecting to the specified URL with custom headers.
    ///
    /// # Errors
    ///
    /// Returns an `Error` if the connection fails.
    pub async fn with_headers(
        url: impl AsRef<str>,
        headers: Option<HashMap<String, String>>,
    ) -> Result<Self, Error> {
        Self::with_headers_and_ping_interval(url, headers, DEFAULT_PING_INTERVAL_SECS).await
    }

    /// Creates a new WebSocketTransport with custom headers and ping interval.
    ///
    /// # Errors
    ///
    /// Returns an `Error` if the connection fails.
    pub async fn with_headers_and_ping_interval(
        url: impl AsRef<str>,
        headers: Option<HashMap<String, String>>,
        ping_interval_secs: u64,
    ) -> Result<Self, Error> {
        let url_str = url.as_ref();
        Url::parse(url_str).map_err(|e| Error::Other(format!("Invalid URL: {}", e)))?;

        // Set up WebSocket configuration
        let config = WebSocketConfig::default();

        // Parse the URL to extract host information
        let parsed_url =
            Url::parse(url_str).map_err(|e| Error::Other(format!("Invalid URL: {}", e)))?;

        // Extract host and port for the Host header
        let host = format!(
            "{}:{}",
            parsed_url.host_str().unwrap_or("localhost"),
            parsed_url
                .port()
                .unwrap_or(if parsed_url.scheme() == "wss" {
                    443
                } else {
                    80
                })
        );

        // Generate WebSocket key
        let ws_key = tokio_tungstenite::tungstenite::handshake::client::generate_key();

        // Build request with all required WebSocket headers
        let mut request = http::Request::builder()
            .uri(url_str)
            .header(
                "User-Agent",
                format!("MCP Client Rust/{}", env!("CARGO_PKG_VERSION")),
            )
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Host", host)
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", ws_key);

        // Add any custom headers
        if let Some(custom_headers) = headers {
            for (name, value) in custom_headers {
                // Skip if it's a WebSocket protocol header that we've already set
                if [
                    "connection",
                    "upgrade",
                    "sec-websocket-key",
                    "sec-websocket-version",
                ]
                .contains(&name.to_lowercase().as_str())
                {
                    continue;
                }
                request = request.header(name, value);
            }
        }

        let request = request
            .method("GET")
            .body(())
            .map_err(|e| Error::Other(format!("Failed to build request: {}", e)))?;

        // Connect with the request
        let (ws_stream, _) = connect_async_with_config(request, Some(config), false)
            .await
            .map_err(|e| Error::Other(format!("WebSocket connection failed: {}", e)))?;

        let writer = Arc::new(Mutex::new(ws_stream));

        // Channel for incoming messages
        let (sender, receiver) = broadcast::channel(100);

        // Flag to control the ping task
        let ping_stop = Arc::new(Mutex::new(false));

        // Start a task to read from the WebSocket and send to the channel
        let writer_clone = writer.clone();
        let sender_clone = sender.clone();
        tokio::spawn(async move {
            tracing::debug!("Starting WebSocket reader task");
            let mut stream = writer_clone.lock().await;

            while let Some(result) = stream.next().await {
                match result {
                    Ok(msg) => {
                        if msg.is_text() || msg.is_binary() {
                            let text = msg.into_text().unwrap_or_default();
                            match serde_json::from_str::<Message>(&text) {
                                Ok(message) => {
                                    if sender_clone.send(Ok(message)).is_err() {
                                        tracing::error!(
                                            "Failed to forward message - channel closed"
                                        );
                                        break;
                                    }
                                }
                                Err(err) => {
                                    tracing::error!("Error deserializing message: {}", err);
                                    let _ = sender_clone
                                        .send(Err(Error::Serialization(err.to_string())));
                                }
                            }
                        } else if msg.is_ping() {
                            // Automatically respond to ping with pong
                            if let Err(e) = stream.send(WsMessage::Pong(msg.into_data())).await {
                                tracing::error!("Error sending pong: {}", e);
                            }
                        } else if msg.is_close() {
                            tracing::debug!("WebSocket connection closed by server");
                            break;
                        }
                        // Ignore pong messages, they're just confirmations of our pings
                    }
                    Err(err) => {
                        tracing::error!("WebSocket read error: {}", err);
                        let _ = sender_clone
                            .send(Err(Error::Other(format!("WebSocket error: {}", err))));
                        break;
                    }
                }
            }
            tracing::debug!("WebSocket reader task terminated");
        });

        // Start a background task for sending ping frames periodically
        let writer_for_ping = writer.clone();
        let ping_stop_clone = ping_stop.clone();
        tokio::spawn(async move {
            let ping_interval = Duration::from_secs(ping_interval_secs);
            tracing::debug!(
                "Starting WebSocket ping task with interval of {:?}",
                ping_interval
            );

            loop {
                tokio::time::sleep(ping_interval).await;

                // Check if we should stop sending pings
                let should_stop = {
                    let stop = ping_stop_clone.lock().await;
                    *stop
                };

                if should_stop {
                    tracing::debug!("Stopping WebSocket ping task");
                    break;
                }

                // Send a ping frame
                let mut writer = writer_for_ping.lock().await;
                match writer.send(WsMessage::Ping(vec![])).await {
                    Ok(_) => {
                        tracing::trace!("Sent WebSocket ping");
                    }
                    Err(e) => {
                        tracing::error!("Failed to send ping: {}", e);
                        break;
                    }
                }
            }
        });

        Ok(WebSocketTransport {
            writer,
            receiver,
            _sender: sender,
            ping_stop,
        })
    }

    /// Creates a new WebSocketTransport by connecting to a host and port.
    ///
    /// # Errors
    ///
    /// Returns an `Error` if the connection fails.
    pub async fn with_host_port(
        host: impl AsRef<str>,
        port: u16,
        secure: bool,
        headers: Option<HashMap<String, String>>,
    ) -> Result<Self, Error> {
        let scheme = if secure { "wss" } else { "ws" };
        let url = format!("{}://{}:{}", scheme, host.as_ref(), port);
        Self::with_headers(url, headers).await
    }

    /// Creates a new WebSocketTransport by connecting to a host and port with a custom ping interval.
    ///
    /// # Errors
    ///
    /// Returns an `Error` if the connection fails.
    pub async fn with_host_port_and_ping_interval(
        host: impl AsRef<str>,
        port: u16,
        secure: bool,
        headers: Option<HashMap<String, String>>,
        ping_interval_secs: u64,
    ) -> Result<Self, Error> {
        let scheme = if secure { "wss" } else { "ws" };
        let url = format!("{}://{}:{}", scheme, host.as_ref(), port);
        Self::with_headers_and_ping_interval(url, headers, ping_interval_secs).await
    }
}

#[async_trait]
impl Transport for WebSocketTransport {
    /// Sends a message over the WebSocket connection.
    async fn send(&self, message: Message) -> Result<(), Error> {
        let json = serde_json::to_string(&message)?;
        let mut writer = self.writer.lock().await;
        writer
            .send(WsMessage::Text(json))
            .await
            .map_err(|e| Error::Other(format!("WebSocket send error: {}", e)))?;
        Ok(())
    }

    /// Provides a stream of incoming messages received from the WebSocket.
    fn receive(&self) -> Pin<Box<dyn Stream<Item = Result<Message, Error>> + Send>> {
        let rx = self.receiver.resubscribe();
        Box::pin(futures::stream::unfold(rx, |mut rx| async move {
            match rx.recv().await {
                Ok(msg) => Some((msg, rx)),
                Err(_) => None,
            }
        }))
    }

    /// Closes the WebSocket connection.
    async fn close(&self) -> Result<(), Error> {
        // First, set the flag to stop the ping task
        {
            let mut stop = self.ping_stop.lock().await;
            *stop = true;
        }

        // Then close the WebSocket connection
        let mut writer = self.writer.lock().await;
        writer
            .close(None)
            .await
            .map_err(|e| Error::Other(format!("Error closing WebSocket: {}", e)))?;
        Ok(())
    }
}
