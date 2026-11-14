//! Standard-library WebSocket framing for the gateway.
//!
//! [`WebSocketStream`] performs the RFC 6455 opening handshake and frame
//! encoding over any byte stream supplied by the host. [`WebSocketClient`]
//! creates a fresh stream on every connection so it can reconnect, and
//! [`WebSocketGateway`] adapts the WebSocket layer to [`GatewayTransport`].
//!
//! TLS is not part of the standard library. A host that needs `wss://`
//! implements [`WebSocketConnector`] with its TLS stack; the connector only has
//! to produce a [`WebSocketIo`] stream for the requested host and port.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use data::GatewayCloseCode;
use version::USER_AGENT;

use crate::connection::{
    GatewayFrame, GatewayTransport, GatewayTransportError, WebSocketFrame, WebSocketTransport,
};

/// The WebSocket handshake GUID from RFC 6455.
const HANDSHAKE_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// The maximum time spent waiting for the opening handshake response.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The largest frame this client accepts, in bytes.
const MAX_FRAME_SIZE: u64 = 64 * 1024 * 1024;

/// A byte stream that can bound blocking reads.
///
/// Implementations wrap a socket or TLS stream. The default methods do nothing,
/// which is correct for in-memory streams and acceptable for hosts that manage
/// timeouts outside the adapter.
pub trait WebSocketIo: Read + Write {
    /// Sets the read timeout used while waiting for the next frame.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the timeout cannot be applied.
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        let _ = timeout;
        Ok(())
    }
}

impl WebSocketIo for TcpStream {
    fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        TcpStream::set_read_timeout(self, timeout)
    }
}

/// Creates fresh byte streams for WebSocket connections.
///
/// A connector is called for every gateway (re)connect, so it owns any TLS
/// configuration rather than a single connected stream.
pub trait WebSocketConnector {
    /// The stream produced for a connection.
    type Stream: WebSocketIo;

    /// Opens a byte stream to `host` and `port`.
    ///
    /// # Errors
    ///
    /// Returns [`GatewayTransportError`] when the stream cannot be opened.
    fn connect(&mut self, host: &str, port: u16) -> Result<Self::Stream, GatewayTransportError>;
}

/// A [`WebSocketConnector`] for plain TCP.
///
/// This connector supports `ws://` URLs. Discord's public gateway requires TLS,
/// so applications targeting `wss://` use a host connector that wraps this one
/// with their TLS implementation.
#[derive(Clone, Copy, Debug, Default)]
pub struct TcpConnector {
    timeout: Option<Duration>,
}

impl TcpConnector {
    /// Creates a connector with the operating system's default timeout.
    #[must_use]
    pub const fn new() -> Self {
        Self { timeout: None }
    }

    /// Sets the connect timeout.
    #[must_use]
    pub const fn with_timeout(timeout: Duration) -> Self {
        Self {
            timeout: Some(timeout),
        }
    }
}

impl WebSocketConnector for TcpConnector {
    type Stream = TcpStream;

    fn connect(&mut self, host: &str, port: u16) -> Result<TcpStream, GatewayTransportError> {
        let host = host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .unwrap_or(host);
        let addresses = (host, port)
            .to_socket_addrs()
            .map_err(|error| GatewayTransportError::new(error.to_string()))?;

        let mut last_error = None;
        for address in addresses {
            let result = match self.timeout {
                Some(timeout) => TcpStream::connect_timeout(&address, timeout),
                None => TcpStream::connect(address),
            };
            match result {
                Ok(stream) => return Ok(stream),
                Err(error) => last_error = Some(error),
            }
        }

        Err(GatewayTransportError::new(last_error.map_or_else(
            || format!("{host}:{port} did not resolve"),
            |error| error.to_string(),
        )))
    }
}

/// A parsed WebSocket URL.
struct WebSocketTarget {
    host: String,
    host_header: String,
    port: u16,
    path: String,
}

impl WebSocketTarget {
    fn parse(url: &str) -> Result<Self, GatewayTransportError> {
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| GatewayTransportError::new("websocket URL is missing a scheme"))?;
        let default_port = match scheme {
            "ws" => 80,
            "wss" => 443,
            other => {
                return Err(GatewayTransportError::new(format!(
                    "unsupported websocket scheme `{other}`"
                )));
            }
        };

        let (authority, path) = match rest.find(['/', '?']) {
            Some(index) => {
                let (authority, path) = rest.split_at(index);
                let path = if path.starts_with('/') {
                    path.to_owned()
                } else {
                    format!("/{path}")
                };
                (authority, path)
            }
            None => (rest, String::from("/")),
        };

        if authority.is_empty() {
            return Err(GatewayTransportError::new("websocket URL has no host"));
        }
        if authority.contains('@') {
            return Err(GatewayTransportError::new(
                "websocket URL userinfo is not supported",
            ));
        }

        let (host, port) = parse_authority(authority, default_port)?;
        let host_header = if port == default_port {
            host.clone()
        } else {
            format!("{host}:{port}")
        };

        Ok(Self {
            host,
            host_header,
            port,
            path,
        })
    }
}

fn parse_authority(
    authority: &str,
    default_port: u16,
) -> Result<(String, u16), GatewayTransportError> {
    if let Some(rest) = authority.strip_prefix('[') {
        let end = rest.find(']').ok_or_else(|| {
            GatewayTransportError::new("websocket URL has an unterminated IPv6 host")
        })?;
        let host = format!("[{}]", &rest[..end]);
        let remainder = &rest[end + 1..];
        let port = if remainder.is_empty() {
            default_port
        } else if let Some(port) = remainder.strip_prefix(':') {
            port.parse::<u16>().map_err(|error| {
                GatewayTransportError::new(format!("invalid websocket port: {error}"))
            })?
        } else {
            return Err(GatewayTransportError::new(
                "invalid websocket authority after IPv6 host",
            ));
        };
        return Ok((host, port));
    }

    match authority.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && !port.is_empty() => Ok((
            host.to_owned(),
            port.parse::<u16>().map_err(|error| {
                GatewayTransportError::new(format!("invalid websocket port: {error}"))
            })?,
        )),
        Some(_) => Err(GatewayTransportError::new(
            "invalid websocket authority or port",
        )),
        None => Ok((authority.to_owned(), default_port)),
    }
}

/// A client-side WebSocket connection over a host byte stream.
///
/// The stream is single-use: call [`WebSocketTransport::open`] once, then
/// exchange frames until closing. Use [`WebSocketClient`] when the connection
/// must be recreated after a gateway reconnect.
pub struct WebSocketStream<S> {
    stream: S,
    handshaken: bool,
    closed: bool,
    fragment_opcode: Option<u8>,
    fragments: Vec<u8>,
}

impl<S> WebSocketStream<S> {
    /// Wraps an unconnected byte stream.
    #[must_use]
    pub const fn new(stream: S) -> Self {
        Self {
            stream,
            handshaken: false,
            closed: false,
            fragment_opcode: None,
            fragments: Vec::new(),
        }
    }

    /// Returns the underlying stream.
    #[must_use]
    pub const fn stream(&self) -> &S {
        &self.stream
    }

    /// Returns the underlying stream mutably.
    pub fn stream_mut(&mut self) -> &mut S {
        &mut self.stream
    }

    /// Consumes the stream and returns it.
    #[must_use]
    pub fn into_stream(self) -> S {
        self.stream
    }

    /// Returns whether the opening handshake completed.
    #[must_use]
    pub const fn is_handshaken(&self) -> bool {
        self.handshaken
    }

    /// Returns whether the connection was closed through this wrapper.
    #[must_use]
    pub const fn is_closed(&self) -> bool {
        self.closed
    }
}

impl<S: WebSocketIo> WebSocketStream<S> {
    fn handshake(&mut self, target: &WebSocketTarget) -> Result<(), GatewayTransportError> {
        if self.handshaken {
            return Err(GatewayTransportError::new(
                "websocket stream is already open; create a new stream to reconnect",
            ));
        }

        let key = base64_encode(&random_bytes::<16>());
        let request = format!(
            "GET {} HTTP/1.1\r\n\
             Host: {}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             User-Agent: {USER_AGENT}\r\n\
             \r\n",
            target.path, target.host_header
        );

        self.stream
            .write_all(request.as_bytes())
            .and_then(|_| self.stream.flush())
            .map_err(io_error)?;

        self.stream
            .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
            .map_err(io_error)?;
        let response = self.read_http_response();
        self.stream.set_read_timeout(None).map_err(io_error)?;
        let response = response?;

        let mut lines = response.split("\r\n");
        let status = lines
            .next()
            .ok_or_else(|| GatewayTransportError::new("empty websocket handshake response"))?;
        if !status.starts_with("HTTP/1.1 101") && !status.starts_with("HTTP/1.0 101") {
            return Err(GatewayTransportError::new(format!(
                "websocket upgrade was rejected: {status}"
            )));
        }

        let expected = base64_encode(&sha1(format!("{key}{HANDSHAKE_GUID}").as_bytes()));
        let accept = lines.find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("sec-websocket-accept")
                .then(|| value.trim())
        });
        if accept != Some(expected.as_str()) {
            return Err(GatewayTransportError::new(
                "websocket handshake returned an invalid Sec-WebSocket-Accept",
            ));
        }

        self.handshaken = true;
        Ok(())
    }

    fn read_http_response(&mut self) -> Result<String, GatewayTransportError> {
        let mut bytes = Vec::with_capacity(256);
        loop {
            let mut byte = [0_u8; 1];
            self.stream.read_exact(&mut byte).map_err(io_error)?;
            bytes.push(byte[0]);
            if bytes.ends_with(b"\r\n\r\n") {
                break;
            }
            if bytes.len() > 16 * 1024 {
                return Err(GatewayTransportError::new(
                    "websocket handshake response is too large",
                ));
            }
        }
        String::from_utf8(bytes).map_err(|_| {
            GatewayTransportError::new("websocket handshake response was not valid UTF-8")
        })
    }

    fn receive_frame(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<Option<WebSocketFrame>, GatewayTransportError> {
        if !self.handshaken {
            return Err(GatewayTransportError::new(
                "websocket handshake has not completed",
            ));
        }
        if self.closed {
            return Ok(Some(WebSocketFrame::Close(GatewayCloseCode::NORMAL)));
        }

        loop {
            let Some(frame) = self.read_frame(timeout)? else {
                return Ok(None);
            };

            match frame.opcode {
                0x0 => {
                    let Some(opcode) = self.fragment_opcode else {
                        return Err(GatewayTransportError::new(
                            "websocket continuation frame without a start frame",
                        ));
                    };
                    self.fragments.extend_from_slice(&frame.payload);
                    if frame.fin {
                        self.fragment_opcode = None;
                        let payload = std::mem::take(&mut self.fragments);
                        return self.finish_message(opcode, payload).map(Some);
                    }
                }
                0x1 | 0x2 => {
                    if self.fragment_opcode.is_some() {
                        return Err(GatewayTransportError::new(
                            "websocket data frame interrupted a fragmented message",
                        ));
                    }
                    if frame.fin {
                        return self.finish_message(frame.opcode, frame.payload).map(Some);
                    }
                    self.fragment_opcode = Some(frame.opcode);
                    self.fragments = frame.payload;
                }
                0x8 => {
                    let code = match frame.payload.len() {
                        0 => GatewayCloseCode::NORMAL,
                        1 => {
                            return Err(GatewayTransportError::new(
                                "websocket close frame had a one-byte payload",
                            ));
                        }
                        _ => GatewayCloseCode::new(u16::from_be_bytes([
                            frame.payload[0],
                            frame.payload[1],
                        ])),
                    };
                    if !self.closed {
                        let _ = self.write_frame(0x8, &code.value().to_be_bytes());
                    }
                    self.closed = true;
                    return Ok(Some(WebSocketFrame::Close(code)));
                }
                0x9 => return Ok(Some(WebSocketFrame::Ping(frame.payload))),
                0xA => return Ok(Some(WebSocketFrame::Pong(frame.payload))),
                opcode => {
                    return Err(GatewayTransportError::new(format!(
                        "websocket frame used unknown opcode {opcode}"
                    )));
                }
            }
        }
    }

    fn finish_message(
        &self,
        opcode: u8,
        payload: Vec<u8>,
    ) -> Result<WebSocketFrame, GatewayTransportError> {
        match opcode {
            0x1 => String::from_utf8(payload)
                .map(WebSocketFrame::Text)
                .map_err(|_| {
                    GatewayTransportError::new("websocket text frame was not valid UTF-8")
                }),
            0x2 => Ok(WebSocketFrame::Binary(payload)),
            _ => Err(GatewayTransportError::new(format!(
                "websocket message used unknown opcode {opcode}"
            ))),
        }
    }

    fn read_frame(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<Option<RawFrame>, GatewayTransportError> {
        self.stream.set_read_timeout(timeout).map_err(io_error)?;

        let mut header = [0_u8; 2];
        loop {
            match self.stream.read(&mut header[..1]) {
                Ok(0) => {
                    return Err(GatewayTransportError::new(
                        "websocket connection closed before a close frame",
                    ));
                }
                Ok(_) => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if is_timeout(&error) => return Ok(None),
                Err(error) => return Err(io_error(error)),
            }
        }

        // A frame has started; wait for the rest without the polling deadline.
        let _ = self.stream.set_read_timeout(None);
        self.stream.read_exact(&mut header[1..]).map_err(io_error)?;

        let fin = header[0] & 0x80 != 0;
        if header[0] & 0x70 != 0 {
            return Err(GatewayTransportError::new(
                "websocket frame used reserved bits",
            ));
        }
        let opcode = header[0] & 0x0F;
        let masked = header[1] & 0x80 != 0;
        let mut length = u64::from(header[1] & 0x7F);

        if length == 126 {
            let mut extended = [0_u8; 2];
            self.stream.read_exact(&mut extended).map_err(io_error)?;
            length = u64::from(u16::from_be_bytes(extended));
        } else if length == 127 {
            let mut extended = [0_u8; 8];
            self.stream.read_exact(&mut extended).map_err(io_error)?;
            length = u64::from_be_bytes(extended);
        }

        if length > MAX_FRAME_SIZE {
            return Err(GatewayTransportError::new(
                "websocket frame exceeded the maximum accepted size",
            ));
        }

        if opcode & 0x08 != 0 {
            if !fin {
                return Err(GatewayTransportError::new(
                    "websocket control frame was fragmented",
                ));
            }
            if length > 125 {
                return Err(GatewayTransportError::new(
                    "websocket control frame payload exceeded 125 bytes",
                ));
            }
        }

        let mask = if masked {
            let mut mask = [0_u8; 4];
            self.stream.read_exact(&mut mask).map_err(io_error)?;
            Some(mask)
        } else {
            None
        };

        let mut payload = vec![0_u8; length as usize];
        self.stream.read_exact(&mut payload).map_err(io_error)?;
        if let Some(mask) = mask {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
        }

        Ok(Some(RawFrame {
            fin,
            opcode,
            payload,
        }))
    }

    fn write_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), GatewayTransportError> {
        if !self.handshaken {
            return Err(GatewayTransportError::new(
                "websocket handshake has not completed",
            ));
        }
        if self.closed {
            return Err(GatewayTransportError::new("websocket is closed"));
        }

        let mask = random_bytes::<4>();
        let mut header = Vec::with_capacity(14);
        header.push(0x80 | opcode);
        let length = payload.len();
        if length <= 125 {
            header.push(0x80 | length as u8);
        } else if length <= usize::from(u16::MAX) {
            header.push(0x80 | 126);
            header.extend_from_slice(&(length as u16).to_be_bytes());
        } else {
            header.push(0x80 | 127);
            header.extend_from_slice(&(length as u64).to_be_bytes());
        }
        header.extend_from_slice(&mask);

        let mut masked = payload.to_vec();
        for (index, byte) in masked.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }

        self.stream
            .write_all(&header)
            .and_then(|_| self.stream.write_all(&masked))
            .and_then(|_| self.stream.flush())
            .map_err(io_error)
    }

    fn send(&mut self, frame: WebSocketFrame) -> Result<(), GatewayTransportError> {
        match frame {
            WebSocketFrame::Text(text) => self.write_frame(0x1, text.as_bytes()),
            WebSocketFrame::Binary(bytes) => self.write_frame(0x2, &bytes),
            WebSocketFrame::Ping(payload) => {
                ensure_control_payload(&payload)?;
                self.write_frame(0x9, &payload)
            }
            WebSocketFrame::Pong(payload) => {
                ensure_control_payload(&payload)?;
                self.write_frame(0xA, &payload)
            }
            WebSocketFrame::Close(code) => {
                self.write_frame(0x8, &code.value().to_be_bytes())?;
                self.closed = true;
                Ok(())
            }
        }
    }

    fn shutdown(&mut self) {
        if self.closed {
            return;
        }
        if self.handshaken {
            let payload = GatewayCloseCode::NORMAL.value().to_be_bytes();
            let _ = self.write_frame(0x8, &payload);
        }
        self.closed = true;
    }
}

impl<S: WebSocketIo> WebSocketTransport for WebSocketStream<S> {
    fn open(&mut self, url: &str) -> Result<(), GatewayTransportError> {
        let target = WebSocketTarget::parse(url)?;
        self.handshake(&target)
    }

    fn receive(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<Option<WebSocketFrame>, GatewayTransportError> {
        self.receive_frame(timeout)
    }

    fn send_frame(&mut self, frame: WebSocketFrame) -> Result<(), GatewayTransportError> {
        self.send(frame)
    }

    fn close(&mut self) {
        self.shutdown();
    }
}

/// A reconnectable WebSocket transport built from a [`WebSocketConnector`].
///
/// Every [`WebSocketTransport::open`] call creates a fresh byte stream and
/// repeats the handshake, which is what the gateway reconnect loop expects.
pub struct WebSocketClient<C: WebSocketConnector> {
    connector: C,
    connection: Option<WebSocketStream<C::Stream>>,
}

impl<C: WebSocketConnector> WebSocketClient<C> {
    /// Creates a client that uses `connector` for every connection.
    #[must_use]
    pub const fn new(connector: C) -> Self {
        Self {
            connector,
            connection: None,
        }
    }

    /// Returns the connector.
    #[must_use]
    pub const fn connector(&self) -> &C {
        &self.connector
    }

    /// Returns the connector mutably.
    pub fn connector_mut(&mut self) -> &mut C {
        &mut self.connector
    }

    /// Returns whether a connection is open.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.connection.is_some()
    }
}

impl<C: WebSocketConnector> WebSocketTransport for WebSocketClient<C> {
    fn open(&mut self, url: &str) -> Result<(), GatewayTransportError> {
        self.close();
        let target = WebSocketTarget::parse(url)?;
        let stream = self.connector.connect(&target.host, target.port)?;
        let mut connection = WebSocketStream::new(stream);
        connection.handshake(&target)?;
        self.connection = Some(connection);
        Ok(())
    }

    fn receive(
        &mut self,
        timeout: Option<Duration>,
    ) -> Result<Option<WebSocketFrame>, GatewayTransportError> {
        match &mut self.connection {
            Some(connection) => connection.receive_frame(timeout),
            None => Err(GatewayTransportError::new("websocket is not connected")),
        }
    }

    fn send_frame(&mut self, frame: WebSocketFrame) -> Result<(), GatewayTransportError> {
        match &mut self.connection {
            Some(connection) => connection.send(frame),
            None => Err(GatewayTransportError::new("websocket is not connected")),
        }
    }

    fn close(&mut self) {
        if let Some(mut connection) = self.connection.take() {
            connection.shutdown();
        }
    }
}

/// Adapts a [`WebSocketTransport`] to the gateway [`GatewayTransport`] boundary.
///
/// Text and binary frames become [`GatewayFrame::Text`], close frames become
/// [`GatewayFrame::Close`], and pings are answered with a pong while the loop
/// waits for application data.
#[derive(Clone, Debug)]
pub struct WebSocketGateway<W> {
    websocket: W,
}

impl<W> WebSocketGateway<W> {
    /// Wraps a WebSocket transport.
    #[must_use]
    pub const fn new(websocket: W) -> Self {
        Self { websocket }
    }

    /// Returns the WebSocket transport.
    #[must_use]
    pub const fn websocket(&self) -> &W {
        &self.websocket
    }

    /// Returns the WebSocket transport mutably.
    pub fn websocket_mut(&mut self) -> &mut W {
        &mut self.websocket
    }

    /// Consumes the adapter and returns the WebSocket transport.
    #[must_use]
    pub fn into_websocket(self) -> W {
        self.websocket
    }
}

impl<W: WebSocketTransport> GatewayTransport for WebSocketGateway<W> {
    fn connect(&mut self, url: &str) -> Result<(), GatewayTransportError> {
        self.websocket.open(url)
    }

    fn recv(&mut self, timeout: Option<Duration>) -> Result<GatewayFrame, GatewayTransportError> {
        let deadline = timeout.map(|timeout| Instant::now() + timeout);
        loop {
            let remaining = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Ok(GatewayFrame::Timeout);
                    }
                    Some(remaining)
                }
                None => None,
            };

            match self.websocket.receive(remaining)? {
                None => return Ok(GatewayFrame::Timeout),
                Some(WebSocketFrame::Text(text)) => return Ok(GatewayFrame::Text(text)),
                Some(WebSocketFrame::Binary(bytes)) => {
                    return String::from_utf8(bytes)
                        .map(GatewayFrame::Text)
                        .map_err(|_| {
                            GatewayTransportError::new(
                                "gateway sent a binary frame that was not UTF-8",
                            )
                        });
                }
                Some(WebSocketFrame::Close(code)) => return Ok(GatewayFrame::Close(code)),
                Some(WebSocketFrame::Ping(payload)) => {
                    self.websocket.send_frame(WebSocketFrame::Pong(payload))?;
                }
                Some(WebSocketFrame::Pong(_)) => {}
            }
        }
    }

    fn send(&mut self, text: &str) -> Result<(), GatewayTransportError> {
        self.websocket
            .send_frame(WebSocketFrame::Text(text.to_owned()))
    }

    fn close(&mut self) {
        self.websocket.close();
    }
}

impl<W> From<W> for WebSocketGateway<W> {
    fn from(websocket: W) -> Self {
        Self::new(websocket)
    }
}

struct RawFrame {
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

fn ensure_control_payload(payload: &[u8]) -> Result<(), GatewayTransportError> {
    if payload.len() > 125 {
        return Err(GatewayTransportError::new(
            "websocket control frame payload exceeded 125 bytes",
        ));
    }
    Ok(())
}

fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

fn io_error(error: io::Error) -> GatewayTransportError {
    GatewayTransportError::new(error.to_string())
}

fn random_bytes<const N: usize>() -> [u8; N] {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    let state = RandomState::new();
    let mut output = [0_u8; N];
    let mut offset = 0;
    let mut counter = 0_u64;

    while offset < N {
        let mut hasher = state.build_hasher();
        hasher.write_u64(counter);
        let bytes = hasher.finish().to_le_bytes();
        for byte in bytes {
            if offset == N {
                break;
            }
            output[offset] = byte;
            offset += 1;
        }
        counter = counter.wrapping_add(1);
    }

    output
}

fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);

        output.push(ALPHABET[(first >> 2) as usize] as char);
        output.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            output.push(ALPHABET[(((second & 0x0F) << 2) | (third >> 6)) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(ALPHABET[(third & 0x3F) as usize] as char);
        } else {
            output.push('=');
        }
    }
    output
}

fn sha1(input: &[u8]) -> [u8; 20] {
    let mut state = [
        0x6745_2301_u32,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let bit_length = (input.len() as u64).wrapping_mul(8);

    let mut message = input.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_be_bytes());

    for chunk in message.chunks(64) {
        let mut words = [0_u32; 80];
        for (index, word) in chunk.chunks(4).enumerate() {
            words[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }

        let [mut a, mut b, mut c, mut d, mut e] = state;
        for (index, &word) in words.iter().enumerate() {
            let (function, constant) = match index {
                0..=19 => ((b & c) | ((!b) & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let temp = a
                .rotate_left(5)
                .wrapping_add(function)
                .wrapping_add(e)
                .wrapping_add(constant)
                .wrapping_add(word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = temp;
        }

        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
    }

    let mut digest = [0_u8; 20];
    for (index, value) in state.iter().enumerate() {
        digest[index * 4..index * 4 + 4].copy_from_slice(&value.to_be_bytes());
    }
    digest
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{self, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;
    use std::time::Duration;

    use data::GatewayCloseCode;

    use super::{
        HANDSHAKE_GUID, TcpConnector, WebSocketClient, WebSocketFrame, WebSocketGateway,
        WebSocketIo, WebSocketStream, WebSocketTarget, base64_encode, random_bytes, sha1,
    };
    use crate::connection::{GatewayFrame, GatewayTransport};

    #[derive(Default)]
    struct MockIo {
        input: VecDeque<u8>,
        output: Vec<u8>,
        timeout: Option<Duration>,
    }

    impl MockIo {
        fn with_input(input: impl IntoIterator<Item = u8>) -> Self {
            Self {
                input: input.into_iter().collect(),
                ..Self::default()
            }
        }
    }

    impl Read for MockIo {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if buffer.is_empty() {
                return Ok(0);
            }
            if self.input.is_empty() {
                if self.timeout.is_some() {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, "timed out"));
                }
                return Ok(0);
            }
            let count = buffer.len().min(self.input.len());
            for slot in &mut buffer[..count] {
                *slot = self.input.pop_front().expect("input is not empty");
            }
            Ok(count)
        }
    }

    impl Write for MockIo {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.output.extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl WebSocketIo for MockIo {
        fn set_read_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
            self.timeout = timeout;
            Ok(())
        }
    }

    fn server_frame(fin: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.push(if fin { 0x80 | opcode } else { opcode });
        if payload.len() <= 125 {
            frame.push(payload.len() as u8);
        } else if payload.len() <= usize::from(u16::MAX) {
            frame.push(126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        } else {
            frame.push(127);
            frame.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        }
        frame.extend_from_slice(payload);
        frame
    }

    fn client_frame(bytes: &[u8]) -> (u8, Vec<u8>) {
        assert!(bytes.len() >= 2);
        let opcode = bytes[0] & 0x0F;
        assert_ne!(bytes[1] & 0x80, 0, "client frames must be masked");
        let mut offset = 2;
        let mut length = u64::from(bytes[1] & 0x7F);
        if length == 126 {
            length = u64::from(u16::from_be_bytes([bytes[2], bytes[3]]));
            offset = 4;
        } else if length == 127 {
            let mut extended = [0_u8; 8];
            extended.copy_from_slice(&bytes[2..10]);
            length = u64::from_be_bytes(extended);
            offset = 10;
        }
        let mask = &bytes[offset..offset + 4];
        offset += 4;
        let mut payload = bytes[offset..offset + length as usize].to_vec();
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
        (opcode, payload)
    }

    #[test]
    fn sha1_matches_known_vectors() {
        assert_eq!(
            sha1(b""),
            [
                0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d, 0x32, 0x55, 0xbf, 0xef, 0x95, 0x60,
                0x18, 0x90, 0xaf, 0xd8, 0x07, 0x09
            ]
        );
        assert_eq!(
            sha1(b"abc"),
            [
                0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78, 0x50,
                0xc2, 0x6c, 0x9c, 0xd0, 0xd8, 0x9d
            ]
        );
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(base64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64_encode(&[0, 1, 2]), "AAEC");
    }

    #[test]
    fn handshake_accept_matches_rfc_example() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        assert_eq!(
            base64_encode(&sha1(format!("{key}{HANDSHAKE_GUID}").as_bytes())),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn urls_are_parsed_for_handshakes() {
        let target =
            WebSocketTarget::parse("wss://gateway.discord.gg/?v=10&encoding=json").unwrap();
        assert_eq!(target.host, "gateway.discord.gg");
        assert_eq!(target.host_header, "gateway.discord.gg");
        assert_eq!(target.port, 443);
        assert_eq!(target.path, "/?v=10&encoding=json");

        let target = WebSocketTarget::parse("ws://127.0.0.1:8080/gateway").unwrap();
        assert_eq!(target.host_header, "127.0.0.1:8080");
        assert_eq!(target.port, 8080);
        assert_eq!(target.path, "/gateway");

        let target = WebSocketTarget::parse("ws://[::1]:9000").unwrap();
        assert_eq!(target.host, "[::1]");
        assert_eq!(target.host_header, "[::1]:9000");
        assert_eq!(target.path, "/");

        assert!(WebSocketTarget::parse("https://example.com").is_err());
        assert!(WebSocketTarget::parse("wss://").is_err());
    }

    #[test]
    fn random_bytes_are_not_all_zero() {
        let bytes = random_bytes::<16>();
        assert!(bytes.iter().any(|byte| *byte != 0));
    }

    #[test]
    fn frames_round_trip_without_a_socket() {
        let input = server_frame(true, 0x1, b"hello");
        let mut stream = WebSocketStream::new(MockIo::with_input(input));
        stream.handshaken = true;

        let frame = stream.receive_frame(None).unwrap().unwrap();
        assert_eq!(frame, WebSocketFrame::Text("hello".into()));

        stream.send(WebSocketFrame::Text("world".into())).unwrap();
        let (opcode, payload) = client_frame(&stream.stream().output);
        assert_eq!(opcode, 0x1);
        assert_eq!(payload, b"world");
    }

    #[test]
    fn fragmented_text_frames_are_joined() {
        let mut input = server_frame(false, 0x1, b"hel");
        input.extend_from_slice(&server_frame(true, 0x0, b"lo"));
        let mut stream = WebSocketStream::new(MockIo::with_input(input));
        stream.handshaken = true;

        let frame = stream.receive_frame(None).unwrap().unwrap();
        assert_eq!(frame, WebSocketFrame::Text("hello".into()));
    }

    #[test]
    fn receive_reports_idle_streams() {
        let mut stream = WebSocketStream::new(MockIo::default());
        stream.handshaken = true;
        assert_eq!(
            stream.receive_frame(Some(Duration::from_millis(5))),
            Ok(None)
        );
    }

    #[test]
    fn gateway_answers_pings_before_returning_text() {
        let mut input = server_frame(true, 0x9, b"ping");
        input.extend_from_slice(&server_frame(true, 0x1, b"payload"));
        let mut stream = WebSocketStream::new(MockIo::with_input(input));
        stream.handshaken = true;
        let mut gateway = WebSocketGateway::new(stream);

        match gateway.recv(Some(Duration::from_secs(1))).unwrap() {
            GatewayFrame::Text(text) => assert_eq!(text, "payload"),
            other => panic!("unexpected frame: {other:?}"),
        }

        let (opcode, payload) = client_frame(&gateway.websocket().stream().output);
        assert_eq!(opcode, 0xA);
        assert_eq!(payload, b"ping");
    }

    fn read_http_request(socket: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        loop {
            let mut byte = [0_u8; 1];
            socket.read_exact(&mut byte).unwrap();
            bytes.push(byte[0]);
            if bytes.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8(bytes).unwrap()
    }

    fn accept_handshake(listener: &TcpListener) -> TcpStream {
        let (mut socket, _) = listener.accept().unwrap();
        let request = read_http_request(&mut socket);
        let key = request
            .lines()
            .find_map(|line| line.strip_prefix("Sec-WebSocket-Key: "))
            .expect("handshake key")
            .trim()
            .to_owned();
        let accept = base64_encode(&sha1(format!("{key}{HANDSHAKE_GUID}").as_bytes()));
        let response = format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\
             \r\n"
        );
        socket.write_all(response.as_bytes()).unwrap();
        socket
    }

    fn read_socket_frame(socket: &mut TcpStream) -> (u8, Vec<u8>) {
        let mut header = [0_u8; 2];
        socket.read_exact(&mut header).unwrap();
        let opcode = header[0] & 0x0F;
        let masked = header[1] & 0x80 != 0;
        let mut length = u64::from(header[1] & 0x7F);
        if length == 126 {
            let mut extended = [0_u8; 2];
            socket.read_exact(&mut extended).unwrap();
            length = u64::from(u16::from_be_bytes(extended));
        } else if length == 127 {
            let mut extended = [0_u8; 8];
            socket.read_exact(&mut extended).unwrap();
            length = u64::from_be_bytes(extended);
        }
        let mask = if masked {
            let mut mask = [0_u8; 4];
            socket.read_exact(&mut mask).unwrap();
            Some(mask)
        } else {
            None
        };
        let mut payload = vec![0_u8; length as usize];
        socket.read_exact(&mut payload).unwrap();
        if let Some(mask) = mask {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
        }
        (opcode, payload)
    }

    #[test]
    fn client_handshakes_and_exchanges_frames_over_tcp() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();

        let server = thread::spawn(move || {
            let mut socket = accept_handshake(&listener);
            socket
                .write_all(&server_frame(true, 0x1, b"hello"))
                .unwrap();

            let (opcode, payload) = read_socket_frame(&mut socket);
            assert_eq!(opcode, 0x1);
            assert_eq!(payload, b"from client");

            socket
                .write_all(&server_frame(true, 0x8, &1000_u16.to_be_bytes()))
                .unwrap();
        });

        let mut gateway = WebSocketGateway::new(WebSocketClient::new(TcpConnector::new()));
        gateway
            .connect(&format!("ws://{address}/?v=10"))
            .expect("handshake");

        match gateway.recv(Some(Duration::from_secs(2))).unwrap() {
            GatewayFrame::Text(text) => assert_eq!(text, "hello"),
            other => panic!("unexpected frame: {other:?}"),
        }
        gateway.send("from client").unwrap();
        match gateway.recv(Some(Duration::from_secs(2))).unwrap() {
            GatewayFrame::Close(code) => assert_eq!(code, GatewayCloseCode::NORMAL),
            other => panic!("unexpected frame: {other:?}"),
        }
        gateway.close();

        server.join().unwrap();
    }

    #[test]
    fn client_reconnects_with_a_fresh_stream() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();

        let server = thread::spawn(move || {
            for round in 0..2 {
                let mut socket = accept_handshake(&listener);
                socket
                    .write_all(&server_frame(
                        true,
                        0x1,
                        format!("round {round}").as_bytes(),
                    ))
                    .unwrap();
                let (opcode, payload) = read_socket_frame(&mut socket);
                assert_eq!(opcode, 0x8, "client should send a close frame");
                assert_eq!(payload, 1000_u16.to_be_bytes());
            }
        });

        let mut gateway = WebSocketGateway::new(WebSocketClient::new(TcpConnector::new()));
        for round in 0..2 {
            gateway
                .connect(&format!("ws://{address}/"))
                .expect("reconnect handshake");
            match gateway.recv(Some(Duration::from_secs(2))).unwrap() {
                GatewayFrame::Text(text) => assert_eq!(text, format!("round {round}")),
                other => panic!("unexpected frame: {other:?}"),
            }
            gateway.close();
        }

        server.join().unwrap();
    }
}
