# Webhooks

Webhooks post messages to a channel without a bot account or gateway
connection. The `webhook` crate is deliberately small: it builds endpoint paths,
message payloads and multipart bodies, leaving transport and retries to your HTTP
client.

---

## Endpoint paths

A webhook is identified by an ID and token:

```rust
use discord_rust::webhook::Webhook;

let webhook = Webhook::new(123456789012345678, "your_webhook_token");

println!("{}", webhook.execute_path());
// webhooks/123456789012345678/your_webhook_token
```

Paths are relative to `https://discord.com/api/v10/`. Pass them to
`HttpClient`/`DiscordApi` or your own transport:

```rust
let request = api.client().post(webhook.execute_path());
```

| Method | Path suffix |
|---|---|
| `execute_path()` | Base execute endpoint |
| `edit_message_path(id)` | `/messages/{id}` |
| `delete_message_path(id)` | `/messages/{id}` |
| `fetch_message_path(id)` | `/messages/{id}` |
| `edit_path()` | Base endpoint (edit the webhook) |
| `delete_path()` | Base endpoint (delete the webhook) |

`SyncWebhook` is an alias for `Webhook`.

---

## Messages

`WebhookMessage` currently supports content and a `silent` flag:

```rust
use discord_rust::webhook::{Webhook, WebhookMessage};

let webhook = Webhook::new(id, token);

let message = WebhookMessage::new()
    .with_content("Build #42 passed")
    .silent(true);

let body = webhook.message(message);
// {"content":"Build #42 passed","silent":true}
```

`message` returns the crate's compact JSON representation. It is not a full
Discord message object: embeds, usernames, avatars, files and components are not
encoded by `WebhookMessage`. Build those as `data::Value` and post them with the
REST client when you need the complete payload. Discord's execute endpoint uses
`flags: 4096` (suppress notifications) for silent delivery; map the `silent`
field to that flag, or send a raw payload, if the webhook must honor it.

---

## Multipart uploads

`Multipart` assembles a `multipart/form-data` body without external
dependencies:

```rust
use discord_rust::webhook::Multipart;

let mut form = Multipart::new("discord-rust-boundary");
form.field("payload_json", r#"{"content":"Crash log attached"}"#);
form.file(
    "files[0]",
    "crash.log",
    "text/plain",
    b"2026-11-12 20:00:00 [ERROR] out of memory",
);

let (content_type, body) = form.finish();
println!("{content_type}");
// multipart/form-data; boundary=discord-rust-boundary
```

Send `body` with a `Content-Type` header set to `content_type`. The caller owns
the HTTP request; `Multipart` only encodes it.

---

## Execution example

```rust
use discord_rust::clients::{DiscordApi, HttpClient, HttpsTransport};
use discord_rust::data;
use discord_rust::webhook::{Webhook, WebhookMessage};

let api = DiscordApi::new(HttpClient::new("unused", HttpsTransport::discord()));
let webhook = Webhook::new(id, token);

let request = api
    .client()
    .post(webhook.execute_path())
    .with_header("Content-Type", "application/json");

let response = api
    .client()
    .execute_json(request, &data::from_str(&webhook.message(WebhookMessage::new().with_content("hello")))?)?;

println!("status: {}", response.status);
```

Webhook execution does not require bot authentication; the token in the path is
the credential. The placeholder `HttpClient` token is sent as an
`Authorization` header, which Discord ignores for webhook routes.
