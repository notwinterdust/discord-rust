# Introduction

`discord-rust` is a synchronous, modular Discord library for Rust. It favours
explicit ownership, typed models and ordinary OS threads over an async runtime.

The runtime crates are standard-library only. TLS is an optional layer enabled
by the default `tls` feature; the proc-macro crate uses `syn` and `quote` at
compile time only. See [Transport & TLS](tls.md) for the trade-off.

---

## Design

### Synchronous by default

Gateway, REST and command handlers use blocking I/O and `std::thread` /
`std::sync` primitives. There is no hidden executor and no runtime to configure.
`Bot::run` blocks the calling thread; `Bot::start` moves the same loop to a
background thread and returns a `BotHandle`.

The gateway loop and one dispatch worker are the two moving parts:

1. The gateway thread reads frames, decodes events, applies cache updates and
   queues work.
2. The dispatch worker runs `before_invoke`, command dispatch, user handlers and
   `after_invoke` in order.

`Bot::start` moves the gateway loop to a background thread and returns a
`BotHandle`. The split keeps handlers from blocking heartbeats
while preserving deterministic per-event ordering. `ThreadPool` and
`PooledEventDispatcher` are available when independent handlers should run
concurrently.

### Transport traits

Networking is defined by traits, not by a built-in client stack:

- `clients::Transport` executes HTTP requests.
- `clients::GatewayTransport` connects, receives/sends gateway frames and closes.
- `clients::WebSocketIo` abstracts the byte stream used by the WebSocket client.

TLS builds implement these traits with `rustls`; tests implement them with
scripted in-memory transports.

### Typed models

Discord payloads decode into the `models` crate: users, guilds, channels,
messages, interactions, components and events. `data::Value` remains available
for new or undocumented fields, and `models::CastError` reports schema
mismatches with field paths.

### Errors

Each layer exposes its own error type instead of collapsing failures into
strings. See [Errors & Logging](logging-errors.md) for the full map.

---

## Workspace map

The root `discord-rust` crate re-exports the workspace and is the only crate most
applications need to depend on.

| Crate | Purpose |
|---|---|
| `discord-rust` | Umbrella facade and root re-exports |
| `discord-rust-abc` | `Snowflake`, colours and CDN asset helpers |
| `discord-rust-app` | Application command definitions, UI components and paginator |
| `discord-rust-clients` | REST client, gateway, cache, bot runtime and sharding |
| `discord-rust-commands` | Prefix commands, slash commands and `Context` |
| `discord-rust-data` | JSON values and encoding |
| `discord-rust-enums` | Intents, permissions, bitflags and API enums |
| `discord-rust-event` | Standalone event bus primitives |
| `discord-rust-except` | HTTP and API exception types |
| `discord-rust-log` | Structured logging primitives |
| `discord-rust-macros` | `#[command]` attribute macro |
| `discord-rust-models` | Domain models, events and model casting |
| `discord-rust-util` | Query encoding and the bounded thread pool |
| `discord-rust-version` | API version and endpoint helpers |
| `discord-rust-voice` | Voice gateway, RTP and FFmpeg source |
| `discord-rust-webhook` | Standalone webhook execution |

---

## Documentation map

- [Quickstart](quickstart.md) — a bot that responds to prefix and slash commands.
- [BotBuilder & Lifecycle](bot-builder.md) — configuration, dispatch and shutdown.
- [Commands & Interactions](commands.md) — command registration and responses.
- [REST API Client](rest-api.md) — raw requests, typed helpers and retries.
- [State Cache](cache.md) — capacities, shared caches and edit tracking.
- [Transport & TLS](tls.md) — automatic TLS and custom transports.
- [Errors & Logging](logging-errors.md) — typed errors and log sinks.

The remaining guides cover the gateway protocol, sharding, thread pools, models,
UI components, webhooks and voice. They are maintained alongside the crates they
describe.
