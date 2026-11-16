# Errors & Logging

The library uses typed errors at each layer rather than stringly-typed failures.
The `log` crate provides small structured logging primitives without an
asynchronous runtime or third-party logger.

---

## Error types

| Layer | Type |
|---|---|
| REST requests | `except::HttpError` |
| Typed REST helpers | `clients::ApiError` |
| Bot construction | `clients::BotBuildError` |
| Gateway runtime | `clients::GatewayConnectionError` / `GatewayTransportError` |
| Command dispatch | `commands::CommandError` with `CommandErrorKind` |
| JSON/model decoding | `models::CastError` |

### HTTP errors

`HttpError` is an enum, so match on the variant instead of downcasting:

```rust
use discord_rust::except::HttpError;

match api.send_message(channel_id, "hello") {
    Ok(response) => println!("sent: {}", response.status),
    Err(HttpError::Status { status, error }) => {
        eprintln!("Discord returned {status}: {error:?}");
    }
    Err(HttpError::RateLimited { retry_after, global }) => {
        eprintln!("retry after {retry_after:?} (global: {global})");
    }
    Err(HttpError::Transport { message }) => eprintln!("transport: {message}"),
    Err(HttpError::Decode { message }) => eprintln!("decode: {message}"),
}
```

The typed helpers wrap the same failures in `ApiError` and add model errors:

```rust
use discord_rust::clients::ApiError;

match api.guild_typed(guild_id) {
    Ok(guild) => println!("{}", guild.name),
    Err(ApiError::Http(error)) => eprintln!("HTTP: {error}"),
    Err(ApiError::Json(message)) => eprintln!("JSON: {message}"),
    Err(ApiError::Model(error)) => eprintln!("model: {error}"),
}
```

`HttpResponse` itself is not an `Err`. Non-2xx responses are returned from raw
endpoint methods; `is_success()` distinguishes them. The `*_typed` helpers check
the status for you and build a structured `HttpError`.

### Bot construction

```rust
use discord_rust::{BotBuildError, BotBuilder};

let error = BotBuilder::new().build().unwrap_err();
assert_eq!(error, BotBuildError::MissingToken);
```

Empty tokens after normalization produce `BotBuildError::EmptyToken`.

### Command errors

`CommandError` carries a `CommandErrorKind` and a message:

```rust
use discord_rust::{CommandError, CommandErrorKind};

let error = CommandError::with_kind(CommandErrorKind::Check, "missing permission");
assert_eq!(error.kind(), CommandErrorKind::Check);
```

Kinds: `Parsing`, `Check`, `Handler`, `Response`, `Context`, `Interaction` and
`Other`. Command handlers may return `Result<(), CommandError>` or
`Result<(), E>` for any `E: Display`; the bot converts the error to a string for
the `on_error` hook.

---

## Logging

`discord_rust::log` defines:

- `Level`: `Debug`, `Info`, `Warn`, `Error`.
- `StderrLogger`: writes records to stderr with timestamp, level and scope.
- `MemoryLogger`: captures formatted records in memory for tests.
- `NullLogger`: discards everything.
- The `Logger` trait, for custom sinks.

```rust
use discord_rust::log::{info, warn, Level, Logger, MemoryLogger, StderrLogger};

let stderr = StderrLogger::new(Level::Info);

let memory = MemoryLogger::new(Level::Debug);
info(&stderr, "gateway", "connected");
warn(&memory, "rest", "rate limit approaching");

// Custom sinks implement the same trait.
struct MyLogger;
impl Logger for MyLogger {
    fn log(&self, record: &discord_rust::log::Record<'_>) {
        println!("{record}");
    }
    fn enabled(&self, level: Level) -> bool {
        level >= Level::Info
    }
}
```

The helpers are functions, not macros: `debug`, `info`, `warn` and `error` take a
`&dyn Logger`, a scope string and a message string. This keeps logging usable
from any thread and keeps the workspace free of global logger state.

> [!NOTE]
> Logging is opt-in. The library does not install a logger or write protocol
> traces unless your code or a transport calls these helpers.
