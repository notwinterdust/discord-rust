# BotBuilder & Lifecycle

`BotBuilder` assembles a gateway configuration: the token, intents, event
handlers, hooks, cache and optional command registries. `Bot` owns the assembled
state and runs the gateway loop.

```rust
use discord_rust::{Bot, BotBuilder, Intents};

let bot = BotBuilder::new()
    .token("YOUR_TOKEN")                 // "Bot YOUR_TOKEN" is also accepted
    .intents_flags(Intents::default() | Intents::MESSAGE_CONTENT)
    .build()?;
```

`build` returns a `BotBuildError` when the token is missing or empty. The stored
token never includes the `Bot ` prefix: the gateway sends the raw token while the
REST client adds the prefix to the `Authorization` header.

---

## Intents

Use `intents` for a raw bitfield or `intents_flags` for `Intents`:

```rust
let bot = BotBuilder::new()
    .token(token)
    .intents_flags(Intents::GUILDS | Intents::GUILD_MESSAGES)
    .build()?;
```

Privileged intents (`MESSAGE_CONTENT`, `MEMBERS`, `PRESENCES`) must also be
enabled in the Developer Portal.

---

## Event handlers

Handlers are plain closures. They may return `()` or `Result<(), E>`; an error is
reported to the `on_error` hook.

| Method | Filter |
|---|---|
| `on_event` | Every event |
| `on_event_name(name, handler)` | One gateway event name (for example `"MESSAGE_CREATE"`) |
| `on_event_filter(filter, handler)` | A custom `EventFilter` |
| `on_event_with_cache` | Every event, with `&Arc<Mutex<ClientCache>>` |
| `on_event_name_with_cache` | One event name, with the live cache |
| `on_event_filter_with_cache` | Custom filter, with the live cache |

```rust
use discord_rust::{BotBuilder, ClientCache, Event, Intents};
use std::sync::{Arc, Mutex};

let bot = BotBuilder::new()
    .token(token)
    .intents_flags(Intents::default())
    .on_event_name("MESSAGE_CREATE", |event| {
        if let Event::MessageCreate(message) = event {
            println!("{}", message.content);
        }
    })
    .on_event_name_with_cache("MESSAGE_CREATE", |event, cache: &Arc<Mutex<ClientCache>>| {
        if let Event::MessageCreate(message) = event {
            let cache = cache.lock().unwrap();
            if let Some(author) = cache.user(message.author.id().get()) {
                println!("cached author: {}", author.name);
            }
        }
    })
    .build()?;
```

### Dispatch and threading

Gateway events are received, decoded and applied to the cache on the gateway
thread. They are then queued to one dedicated dispatch worker, where
`before_invoke`, command dispatch, event handlers and `after_invoke` run in order.
This keeps handlers from blocking heartbeats and guarantees that a handler sees
the cache state **after** its event has been applied.

Because handlers no longer run on the gateway thread, a slow handler delays later
handlers but not the WebSocket reader. Use `PooledEventDispatcher` or your own
thread pool when independent handlers should run concurrently.

### Errors and hooks

```rust
let bot = BotBuilder::new()
    .token(token)
    .before_invoke(|event| {
        // Runs before command dispatch and handlers for every event.
    })
    .after_invoke(|event| {
        // Runs only when no command or handler failed.
    })
    .on_error(|event, message| {
        eprintln!("{} failed: {message}", event.name());
    })
    .on_event(|_| -> Result<(), &'static str> {
        Ok(())
    })
    .build()?;
```

Command errors are passed to the same `on_error` hook. `CommandError::kind()`
distinguishes parsing, check, response, interaction and handler failures.

---

## Cache

Without configuration the bot creates a private `ClientCache` with
`CacheConfig::default()`. Adjust the limits with `cache_config`, or supply a
shared cache with `cache`:

```rust
use discord_rust::clients::CacheConfig;
use discord_rust::{BotBuilder, ClientCache};
use std::sync::{Arc, Mutex};

let cache = Arc::new(Mutex::new(ClientCache::new(CacheConfig::default())));

let bot = BotBuilder::new()
    .token(token)
    .cache(Arc::clone(&cache))
    .build()?;

// The same live state is available after construction.
let shared = bot.cache();
```

`BotBuilder::cache_config` is ignored when `cache` is supplied. See
[State Cache](cache.md) for capacity limits and lookup helpers.

---

## Commands

Prefix commands and slash commands are registered separately:

```rust
use discord_rust::{CommandRegistry, SlashCommandRegistry};

let mut prefix = CommandRegistry::new("!");
prefix
    .add_command("ping", |ctx: &discord_rust::Context| ctx.respond("pong"))
    .expect("register !ping");

let mut slash = SlashCommandRegistry::new();
slash
    .add("ping", "Check that the bot is online", |ctx: &discord_rust::Context| {
        ctx.respond("pong")
    })
    .expect("register /ping");

let bot = BotBuilder::new()
    .token(token)
    .commands(prefix)
    .slash_commands(slash)
    .build()?;
```

Slash definitions are registered with Discord after `READY` when a registry and
REST client are available. This is on by default; disable it with
`.sync_commands(false)`. Guild commands propagate immediately and are useful
during development:

```rust
let bot = BotBuilder::new()
    .token(token)
    .slash_commands(slash)
    .command_guild(Some(123456789012345678))
    .build()?;
```

`api` installs a `DiscordApi` for command responses, interaction callbacks and
command registration. With the `tls` feature, a REST client is created
automatically, so `api` is only needed for custom transports. `responder`
installs a custom `Responder` for prefix-command replies.

---

## Running and shutting down

| Method | Behaviour |
|---|---|
| `run(transport)` | Runs on the calling thread with a supplied `GatewayTransport` |
| `run_with_shutdown(transport, &AtomicBool)` | Same, but stops when the flag is set |
| `run_auto()` | Connects over TLS and installs a default REST client |
| `start(transport)` | Runs `run(transport)` on a background thread and returns `BotHandle` |
| `start_auto()` | Runs `run_auto()` on a background thread |

```rust
let handle = bot.start_auto();

// ... let the bot run ...

handle.shutdown()?;
```

`BotHandle::is_running` reports whether the worker is alive. `shutdown` requests
a stop and joins the worker; `join` waits without requesting shutdown. Dropping a
`BotHandle` also requests shutdown and joins.

`run_auto` and `start_auto` require the `tls` feature. Without it, supply your own
transports or call `api`/`responder` explicitly.
