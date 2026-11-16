<p align="center"><img src="Vector.svg" alt="discord-rust" width="120"></p>

# discord-rust

[![Documentation](https://img.shields.io/badge/docs-discord--rust.netlify.app-CE412B.svg)](https://discord-rust.netlify.app/)
[![Crates.io](https://img.shields.io/crates/v/discord-rust.svg)](https://crates.io/crates/discord-rust)
[![License: MIT](https://img.shields.io/badge/License-MIT-CE412B.svg)](LICENSE)

A synchronous, modular Discord API and gateway library for Rust. The runtime
crates use only the standard library; TLS is an optional layer enabled by the
default `tls` feature, and the proc-macro crate uses compile-time-only
dependencies.

---

## Features

- **Gateway lifecycle**: connect, Hello, Identify, Ready, Resume, heartbeats and
  automatic reconnection.
- **Sharding**: `ShardManager` assignments with identify delays, plus a
  `ConcurrentShardRuntime` that runs one worker per shard.
- **Bounded cache**: per-resource `ClientCache` limits and a `MessageEditTracker`
  for user-message/bot-response pairs.
- **Worker pools**: `ThreadPool`, `ThreadedEventDispatcher` and
  `PooledEventDispatcher` for sequential or parallel event handling.
- **Commands**: prefix commands, slash commands, typed options, command groups
  and the `#[command]` attribute macro.
- **Interaction components**: buttons, select menus, text inputs, modals,
  component routing and a multi-page `Paginator`.
- **REST client**: raw and typed endpoint helpers, retry policy and optional
  `rustls` HTTPS transport.
- **Voice primitives**: voice state, RTP packet framing, `FfmpegSource` PCM
  streaming and volume scaling.
- **Typed models**: users, guilds, channels, roles, messages, embeds, presence,
  automod, stages, soundboard and monetization.
- **Event utilities**: `EventWaiter` for waiting on future matching events with
  a timeout.
- **Structured logging**: `StderrLogger`, `MemoryLogger` and `NullLogger` without
  a third-party logging runtime.

---

## Documentation

The interactive guides live at **[https://discord-rust.netlify.app/](https://discord-rust.netlify.app/)**.
The same pages are available as raw markdown in [`content/`](content/).

---

## Installation

```toml
[dependencies]
discord-rust = "0.1.1"
```

The `tls` feature is enabled by default. To keep the runtime dependency graph
standard-library only and supply your own transports:

```toml
[dependencies]
discord-rust = { version = "0.1.1", default-features = false }
```

---

## Quickstart

This bot connects over TLS, logs messages and answers `!ping` and `/ping`.

```rust
use discord_rust::{
    BotBuilder, CommandRegistry, Context, Event, Intents, SlashCommandRegistry,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let token = std::env::var("DISCORD_TOKEN")?;

    let mut prefix = CommandRegistry::new("!");
    prefix
        .add_command("ping", |context: &Context| context.respond("Pong!"))
        .expect("register !ping");

    let mut slash = SlashCommandRegistry::new();
    slash
        .add("ping", "Check that the bot is online", |context: &Context| {
            context.respond("Pong!")
        })
        .expect("register /ping");

    let bot = BotBuilder::new()
        .token(token)
        .intents_flags(Intents::default() | Intents::MESSAGE_CONTENT)
        .commands(prefix)
        .slash_commands(slash)
        .on_event(|event| match event {
            Event::MessageCreate(message) => {
                println!("{}: {}", message.author.user().name, message.content);
            }
            Event::Ready(ready) => {
                println!("Connected as {}", ready.user.name);
            }
            _ => {}
        })
        .build()?;

    bot.run_auto()?;
    Ok(())
}
```

`run_auto` blocks the calling thread and installs a REST client automatically.
Use `start_auto` to run it in the background and receive a `BotHandle`.

Read the [Quickstart](content/quickstart.md) for the Developer Portal setup, or
[BotBuilder & Lifecycle](content/bot-builder.md) for hooks, worker dispatch and
shutdown.

---

## License

MIT
