# Transport & TLS

The runtime crates use only the Rust standard library. TLS is an optional layer
provided by the `clients` crate and enabled by the root `tls` feature, which is
on by default. The proc-macro crate uses `syn` and `quote` at compile time.

```toml
[dependencies]
discord-rust = "0.1.1"           # tls is enabled

# or:
discord-rust = { version = "0.1.1", default-features = false }
```

Enabling `tls` adds `rustls`, `rustls-pki-types` and `webpki-roots` to
`discord-rust-clients`. The remaining runtime crates stay dependency-free.

---

## Zero-configuration runtime

`Bot::run_auto` and `Bot::start_auto` are available when `tls` is enabled. They:

- connect to the gateway over `wss://` with the bundled web PKI roots,
- create a REST client for command responses and interaction callbacks,
- register slash commands after `READY` when a registry is supplied.

```rust
let bot = BotBuilder::new()
    .token(std::env::var("DISCORD_TOKEN")?)
    .slash_commands(slash)
    .build()?;

bot.run_auto()?;
```

No `rustls` provider installation is required. `clients::tls::client_config`
constructs the configuration with an explicit `ring` provider.

---

## Using the transports directly

The TLS types implement the same traits as their plain counterparts.

```rust
use discord_rust::clients::{DiscordApi, HttpClient, HttpsTransport};
use discord_rust::TlsConnector;
use discord_rust::WebSocketClient;
use discord_rust::WebSocketGateway;

// HTTPS REST
let api = DiscordApi::new(HttpClient::new(token, HttpsTransport::discord()));

// WSS gateway
let transport = WebSocketGateway::new(WebSocketClient::new(TlsConnector::new()));
bot.run(transport)?;
```

`HttpsTransport::new` accepts a custom host and port, and `with_timeout` sets
the timeout. `TlsConnector` reads the host from the gateway URL, so it only
needs an optional timeout:

```rust
let https = HttpsTransport::new("example.com", 443)
    .with_timeout(std::time::Duration::from_secs(10));

let connector = TlsConnector::with_timeout(std::time::Duration::from_secs(10));
```

`clients::tls::TlsStream::connect(host, port, timeout)` exposes the underlying
stream for custom transports.

---

## Plain connections and custom transports

Without the `tls` feature, `run_auto` and `start_auto` are not compiled. Provide
a `GatewayTransport` to `Bot::run`, and an `HttpClient` transport for REST:

```rust
use discord_rust::clients::{DiscordApi, HttpClient, TcpTransport};
use discord_rust::BotBuilder;

let api = DiscordApi::new(HttpClient::new(token, TcpTransport::new("localhost", 8080)));

let bot = BotBuilder::new()
    .token(token)
    .api(api)                 // REST client for command replies
    .build()?;

bot.run(my_gateway_transport)?;
```

The relevant traits are:

| Trait | Purpose |
|---|---|
| `clients::Transport` | Executes an `HttpRequest` and returns `HttpResponse` |
| `clients::GatewayTransport` | Connects, receives/sends frames and closes |
| `clients::WebSocketIo` | Byte-stream abstraction used by the WebSocket client |

A custom transport is also the recommended way to test the bot without a network:
the test suite uses a scripted `GatewayTransport` to replay frames.

---

## Tokens

`HttpClient` and `BotBuilder` both accept either `TOKEN` or `Bot TOKEN`. The
prefix is removed during construction and added back only on the REST
`Authorization` header. The gateway receives the raw token, as required by the
Identify payload.

Empty tokens are rejected by `BotBuilder::build` with
`BotBuildError::MissingToken` or `BotBuildError::EmptyToken`.
