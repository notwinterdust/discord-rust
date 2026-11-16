# REST API Client

`DiscordApi` is a thin, typed facade over `HttpClient`. It builds Discord paths,
adds authentication and returns either raw `HttpResponse` values or decoded
models.

The REST client does not own a connection pool: I/O is delegated to a
`Transport`, so the same API works with TLS, plain TCP, a mock or an external
HTTP stack.

---

## Automatic TLS

With the default `tls` feature:

```rust
use discord_rust::clients::{DiscordApi, HttpClient, HttpsTransport};

let api = DiscordApi::new(HttpClient::new(
    "YOUR_TOKEN",                 // "Bot YOUR_TOKEN" is also accepted
    HttpsTransport::discord(),
));
```

`HttpsTransport::discord()` targets `discord.com:443` with the bundled web PKI
roots. See [Transport & TLS](tls.md) for timeouts, custom hosts and the
`--no-default-features` setup.

For a local HTTP endpoint or test server, use `TcpTransport` with an explicit
host and port:

```rust
use discord_rust::clients::{DiscordApi, HttpClient, TcpTransport};

let http = HttpClient::new("TOKEN", TcpTransport::new("127.0.0.1", 8080));
let api = DiscordApi::new(http);
```

---

## Raw requests

Every endpoint method returns `Result<HttpResponse, HttpError>`:

```rust
let response = api.send_message(channel_id, "Hello from discord-rust!")?;
println!("status: {}", response.status);

let body = response.json()?;       // data::Value
let text = response.text()?;       // UTF-8 string
```

`HttpResponse` exposes `status`, `headers` and `body`. Helpers include
`header(name)`, `is_success()`, `is_rate_limited()` and `rate_limit()`.

Requests with a JSON body use the workspace encoder:

```rust
use discord_rust::data::{Object, Value};

let mut body = Object::new();
body.insert("content", Value::from("hello"));
api.create_message(channel_id, Value::Object(body).to_string())?;
```

`create_message` accepts any `Into<Vec<u8>>`, so an encoded `Value`, a `String`
or a `Vec<u8>` all work.

---

## Typed helpers

The `*_typed` methods check the status, decode the JSON body and cast it into a
model. They return `ApiError`, which distinguishes HTTP, JSON and model errors.

```rust
use discord_rust::clients::ApiError;

let me = api.current_user_typed()?;
let user = api.user_typed("123456789012345678")?;
let guild = api.guild_typed(guild_id)?;
let channel = api.channel_typed(channel_id, guild_id)?;
let message = api.message_typed(channel_id, message_id)?;

let sent = api.send_message_typed(channel_id, "hello")?;
let with_embed = api.send_embed_typed(channel_id, &embed)?;
```

`channel_typed` takes the guild ID because guild channels need it for model
resolution; pass `0` for direct-message channels.

Match `ApiError` when the failure mode matters:

```rust
use discord_rust::clients::{ApiError, RetryPolicy};
use discord_rust::except::HttpError;
use std::time::Duration;

match api.guild_typed(guild_id) {
    Ok(guild) => println!("{}", guild.name),
    Err(ApiError::Http(HttpError::Status { status, error })) => {
        eprintln!("Discord returned {status}: {error:?}");
    }
    Err(ApiError::Model(error)) => eprintln!("unexpected payload: {error}"),
    Err(error) => eprintln!("{error}"),
}
```

---

## Endpoint coverage

DiscordApi groups methods by resource:

| Resource | Examples |
|---|---|
| Users | `user`, `current_user_typed`, `current_user_guilds`, `create_dm` |
| Guilds | `guild_typed`, `guild_preview`, `guild_channels`, `roles`, `bans`, `audit_log` |
| Channels | `channel_typed`, `modify_channel`, `delete_channel`, `trigger_typing` |
| Messages | `send_message_typed`, `create_message`, `edit_message`, `delete_message`, `bulk_delete_messages` |
| Reactions | `add_reaction`, `remove_own_reaction`, `remove_user_reaction` |
| Interactions | `interaction_callback`, `interaction_followup`, `overwrite_global_commands`, `overwrite_guild_commands` |

The module-level documentation for `discord_rust::clients::api::DiscordApi`
lists the complete surface. Methods are intentionally thin: they encode the
route and delegate execution to the configured transport.

---

## Rate limits

Discord reports rate limits through response headers and `429` responses.
Normal endpoint methods return the response unchanged; they do not sleep
implicitly. Inspect the response or use the retry helper:

```rust
use discord_rust::clients::RetryPolicy;
use std::time::Duration;

let request = api.client().get("channels/123/messages");
let response = api
    .client()
    .execute_with_retry(request, RetryPolicy::new(3, Duration::from_millis(250)))?;
```

`HttpResponse::rate_limit()` parses `X-RateLimit-*` headers and
`HttpResponse::retry_after()` reads `Retry-After`. `HttpError::RateLimited`
carries the same information when a transport reports it as an error.

> [!NOTE]
> `execute_with_retry` blocks the calling thread. In the bot runtime, REST calls
> made by command handlers run on the dispatch worker, not on the gateway reader.

---

## Token handling

`HttpClient` strips an optional `Bot ` prefix and stores the raw token. It adds
`Authorization: Bot <token>` when preparing each request, unless the request
already has an `Authorization` header. `BotBuilder` applies the same
normalization to the token used by the gateway.
