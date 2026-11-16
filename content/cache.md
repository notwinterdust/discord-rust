# State Cache & Edit Tracking

`ClientCache` is a bounded, in-memory snapshot of guilds, channels, roles, users,
members, messages, presences and webhooks. It is updated from gateway events so
command handlers can answer common questions without extra REST calls.

The cache is deliberately **not** a database. It evicts entries when a capacity
limit is reached and only contains state the bot has observed.

---

## Defaults and configuration

```rust
use discord_rust::clients::{CacheConfig, ClientCache};

let config = CacheConfig {
    guilds: 1_000,
    channels: 10_000,
    roles: 10_000,
    users: 10_000,
    messages: 1_000,
    presences: 10_000,
    webhooks: 1_000,
    message_edits: 1_000,
};

let mut cache = ClientCache::new(config);
```

A capacity of `0` disables that sub-cache. `CacheConfig::default()` uses the
values above.

| Field | Stored by |
|---|---|
| `guilds` | Guild ID |
| `channels` | Channel ID (guild channels and threads) |
| `roles` | Role ID |
| `users` | User ID |
| `messages` | Message ID |
| `presences` | User ID |
| `webhooks` | Webhook ID |
| `message_edits` | `MessageEditTracker` capacity |

Members do not have a separate capacity. They are stored per guild in
`ClientCache::members`, so their memory use follows the number of cached guilds
and the members your intents deliver.

---

## Feeding events

```rust
let mutated = cache.update(&event);
```

`update` applies supported gateway events and returns whether it changed any
state. To capture the previous message on edits, use `update_tracked`:

```rust
let (mutated, edit) = cache.update_tracked(&event);
if let Some((previous, updated)) = edit {
    if let Some(previous) = previous {
        println!("edited: {} -> {}", previous.content, updated.content);
    }
}
```

The bot runtime calls the cache before enqueueing an event to the dispatch
worker, so handlers always observe the state produced by their own event.

---

## Sharing the cache with the bot

By default the bot creates a private cache from `CacheConfig::default()`.
Supply an existing cache to share it with other code:

```rust
use discord_rust::clients::CacheConfig;
use discord_rust::{BotBuilder, ClientCache};
use std::sync::{Arc, Mutex};

let cache = Arc::new(Mutex::new(ClientCache::new(CacheConfig::default())));

let bot = BotBuilder::new()
    .token(token)
    .cache(Arc::clone(&cache))
    .build()?;

let live = bot.cache(); // same Arc<Mutex<ClientCache>>
```

Use `cache_config` when the bot should create the cache:

```rust
let bot = BotBuilder::new()
    .token(token)
    .cache_config(CacheConfig {
        messages: 5_000,
        ..CacheConfig::default()
    })
    .build()?;
```

`cache_config` is ignored when `cache` is supplied.

---

## Cache-aware handlers

Register a handler with one of the `*_with_cache` methods to receive the live
cache without another lookup:

```rust
use discord_rust::{BotBuilder, ClientCache, Event};
use std::sync::{Arc, Mutex};

let bot = BotBuilder::new()
    .token(token)
    .on_event_name_with_cache(
        "MESSAGE_CREATE",
        |event, cache: &Arc<Mutex<ClientCache>>| {
            if let Event::MessageCreate(message) = event {
                let cache = cache.lock().unwrap();
                if let Some(author) = cache.user(message.author.id().get()) {
                    println!("{} said {}", author.name, message.content);
                }
            }
        },
    )
    .build()?;
```

The lock is held only for the duration of the handler's access. Avoid holding it
across network calls or other blocking work.

The same cache is available inside command handlers through `Context::cache()`.
See [Commands & Interactions](commands.md#the-context).

---

## Lookups

```rust
if let Some(guild) = cache.guild(guild_id) {
    println!("{}", guild.name);
}

if let Some(channel) = cache.channel(channel_id) {
    println!("channel: {}", channel.name);
}

if let Some(member) = cache.member(guild_id, user_id) {
    println!("nickname: {:?}", member.nick);
}

if let Some(permissions) = cache.permissions_for(guild_id, user_id, channel_id) {
    println!("manage server: {}", permissions.manage_guild());
}
```

`permissions_for` resolves the member's roles and channel overwrites. It returns
`None` when the guild, channel or member is not cached.

---

## Message edit tracking

`MessageEditTracker` maps a user's command message to the bot's response, in both
directions:

```rust
use discord_rust::MessageEditTracker;

let mut tracker = MessageEditTracker::new(1_000);

tracker.register_response(user_message_id, bot_reply_id);

assert_eq!(tracker.get_response(user_message_id), Some(bot_reply_id));
assert_eq!(tracker.get_user_message(bot_reply_id), Some(user_message_id));
```

The tracker is embedded as `cache.edit_tracker`. `MESSAGE_DELETE` and
`MESSAGE_DELETE_BULK` remove the corresponding entries.

> [!NOTE]
> The tracker stores IDs only. Deleting a mapped message invalidates the pair;
> it does not edit or delete the other side automatically.

---

## Invalidation

`ClientCache` removes dependent entries when their parent disappears:

- `CHANNEL_DELETE` and `THREAD_DELETE` remove the channel.
- `GUILD_DELETE` removes the guild and its per-guild member cache.
- `MESSAGE_DELETE` and `MESSAGE_DELETE_BULK` remove messages and edit-tracker
  pairs.
- `WEBHOOKS_UPDATE` evicts cached webhooks for the affected channel.
- Role create, update and delete events keep both the role store and the
  guild's role list in sync.

Capacity eviction is independent of these rules: a cache can evict a live entity
when it is full, so treat every lookup as fallible.
