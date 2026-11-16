# Quickstart

This guide builds a bot that:

- connects to Discord over TLS,
- logs incoming messages,
- answers a prefix command (`!ping`),
- registers a slash command (`/ping`).

The example targets the default feature set, which includes automatic TLS.

---

## 1. Create the application

1. Open the [Discord Developer Portal](https://discord.com/developers/applications).
2. Create an application and open its **Bot** page.
3. Copy the bot token. Treat it like a password.
4. Enable **Message Content Intent** if the bot reads message text.
5. Invite the bot with the `bot` and `applications.commands` scopes.

---

## 2. Create the project

```bash
cargo new my_discord_bot
cd my_discord_bot
```

Add the library:

```toml
[dependencies]
discord-rust = "0.1.1"
```

The `tls` feature is enabled by default. To keep the runtime dependency graph
standard-library only, see [Transport & TLS](tls.md).

---

## 3. Write the bot

Replace `src/main.rs` with:

```rust
use discord_rust::{
    BotBuilder, CommandRegistry, Context, Event, Intents, SlashCommandRegistry,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let token = std::env::var("DISCORD_TOKEN")?;

    // Prefix commands: !ping
    let mut prefix = CommandRegistry::new("!");
    prefix
        .add_command("ping", |context: &Context| context.respond("Pong!"))
        .expect("register !ping");

    // Slash commands: /ping
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
        .on_event_name("MESSAGE_CREATE", |event| {
            if let Event::MessageCreate(message) = event {
                println!(
                    "{}: {}",
                    message.author.user().name,
                    message.content
                );
            }
        })
        .on_event_name("READY", |event| {
            if let Event::Ready(ready) = event {
                println!("Connected as {}", ready.user.name);
            }
        })
        .build()?;

    // Connects to wss:// with the bundled web PKI roots and installs a REST
    // client for command responses.
    bot.run_auto()?;
    Ok(())
}
```

> [!NOTE]
> `run_auto` blocks the current thread. Use `start_auto` when the gateway should
> run in the background; it returns a `BotHandle` that can be shut down later.

---

## 4. Run it

```bash
export DISCORD_TOKEN="your token"
cargo run
```

Type `!ping` or use `/ping` in a channel where the bot can send messages. Both
handlers call `Context::respond`, which routes to the interaction callback for
slash commands and to the REST responder for prefix commands.

---

## Next steps

- [BotBuilder & Lifecycle](bot-builder.md) covers hooks, worker dispatch and
  graceful shutdown.
- [Commands & Interactions](commands.md) covers options, caches and interaction
  responses.
- [Transport & TLS](tls.md) explains custom transports and `--no-default-features`.
