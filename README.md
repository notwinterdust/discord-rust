# discord-rust

[![Documentation](https://img.shields.io/badge/docs-discord--rust.netlify.app-5865F2.svg)](https://discord-rust.netlify.app/)
[![Crates.io](https://img.shields.io/crates/v/discord-rust.svg)](https://crates.io/crates/discord-rust)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

A feature-rich, high-performance Discord API and Gateway library for Rust.

---

## Features

- **Full Gateway Lifecycle**: Connect, Hello, Identify, Resume, Heartbeat, and automatic reconnection.
- **Standard-Library WebSocket**: RFC 6455 handshake and framing over any host-provided byte stream, with a reconnectable client and gateway adapter. TLS remains host-supplied.
- **Multi-Shard Architecture**: `ShardManager` with automatic shard counting and `ConcurrentShardRuntime`.
- **LRU In-Memory Cache & Edit Tracking**: Thread-safe `ClientCache` with resource lookup helpers, webhook tracking, and automatic event invalidation.
- **Bounded Worker Thread Pool**: Dedicated `ThreadPool` with `PooledEventDispatcher` for concurrent event fan-out across CPU cores.
- **Application Commands & Proc Macros**: Slash commands, typed options, subcommands, and `#[command]` attribute macro for prefix commands.
- **UI Components & Paginator**: Buttons, select menus, modals, and interactive multi-page `Paginator`.
- **Voice & FFmpeg Streaming**: Voice gateway protocol, RTP packet framing, `FfmpegSource` child process audio streamer, and volume scaling.
- **Rich Domain Models**: Users, guilds, channels, roles, messages, embeds, presence, automod, stages, soundboard, and monetization.
- **Event Ergonomics**: `EventWaiter` for waiting on future matching gateway events with timeouts.
- **Structured Logging**: Built-in structured logging framework with zero third-party runtime dependencies.

---

## Documentation & Wiki

Explore the full interactive documentation, wiki guides, and complete architecture manuals at:

**[https://discord-rust.netlify.app/](https://discord-rust.netlify.app/)**

---

## Installation

Add `discord-rust` to your `Cargo.toml`:

```toml
[dependencies]
discord-rust = "0.1.1"
```

---

## Quickstart: Bot Builder

For the complete 5-minute interactive tutorial and guide, see the **[Step-by-Step Quickstart on the Docs Site](https://discord-rust.netlify.app/#quickstart)**.

Here is a quick overview of setting up and running a bot with `BotBuilder`:

```rust,no_run
use discord_rust::{BotBuilder, Event, GatewayTransport, Intents};

fn run<T: GatewayTransport>(transport: T) {
    let bot = BotBuilder::new()
        .token("YOUR_BOT_TOKEN")
        .intents_flags(Intents::default_intents() | Intents::MESSAGE_CONTENT)
        .on_event(|event| {
            match event {
                Event::MessageCreate(msg) => {
                    println!("Received message: {}", msg.content);
                }
                Event::Ready(ready) => {
                    println!("Connected as {}", ready.user.user.name);
                }
                _ => {}
            }
        })
        .build()
        .expect("Failed to build bot");

    bot.run(transport).expect("gateway error");
}

fn main() {
    // Pass your WebSocket + TLS transport implementation to `run`.
}
```

Learn more in the **[BotBuilder & Lifecycle Guide](https://discord-rust.netlify.app/#bot-builder)** or visit the **[Live Documentation](https://discord-rust.netlify.app/)**.

---

## Quickstart: Prefix Commands

`CommandRegistry` plugs into `BotBuilder`. Matching `MessageCreate` commands run
before event handlers, bot authors are ignored, and handlers receive message
metadata plus a responder for replies:

```rust,no_run
use discord_rust::{BotBuilder, CommandRegistry, Context, Intents};

fn run<T, A>(transport: T, api: discord_rust::clients::DiscordApi<A>)
where
    T: discord_rust::GatewayTransport,
    A: discord_rust::clients::Transport + Send + Sync + 'static,
{
    let mut registry = CommandRegistry::new("!");
    registry
        .add("ping", Vec::<String>::new(), |context: &Context| {
            context.reply("pong")
        })
        .unwrap();
    registry.add_check(|context: &Context| {
        if context.author_is_bot {
            Err("bots cannot invoke commands".into())
        } else {
            Ok(())
        }
    });

    let bot = BotBuilder::new()
        .token("YOUR_BOT_TOKEN")
        .intents_flags(Intents::default_intents() | Intents::MESSAGE_CONTENT)
        .commands(registry)
        .api(api)
        .build()
        .expect("Failed to build bot");

    bot.run(transport).expect("gateway error");
}

fn main() {}
```

`Context` exposes `guild_id`, `channel_id`, `user_id`, `message_id`,
`permissions`, and helpers such as `respond`, `reply`, `respond_embed`,
`typing`, and `has_permissions`. Use `BotBuilder::responder` to supply a custom
`Responder` instead of a REST client.

---

## Message Helpers

`AllowedMentions` controls which mentions a message may trigger. Explicit user
and role mentions are separate from parsing mentions written in the content:

```rust,no_run
use discord_rust::models::{AllowedMentions, Embed};

fn send<T: discord_rust::clients::Transport>(
    api: &discord_rust::clients::DiscordApi<T>,
) -> Result<(), discord_rust::except::HttpError> {
    let mentions = AllowedMentions::none()
        .add_user(42_u64)
        .with_replied_user(true);

    api.send_message_with_mentions(123, "hello", &mentions)?;
    api.send_reply_with_mentions(123, 456, "hi", &mentions)?;

    let embed = Embed::new()
        .with_title("Status")
        .with_description("All systems operational");
    api.send_embed_model(123, &embed)?;
    Ok(())
}
```

`ClientCache` resolves cached resources with `guild`, `channel`, `user`, `role`,
`message`, `webhook`, `guild_channel`, `guild_member`, `channel_by_name`, and
`member_by_name`. Webhooks received from the REST API can be stored with
`cache_webhook` / `cache_webhooks` and are invalidated automatically on
`WEBHOOKS_UPDATE`.

---

## License

This project is licensed under the [MIT License](LICENSE).
