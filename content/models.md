# Models, Snowflakes & CDN Assets

Discord payloads decode into the `models` crate. The models follow Discord's
API objects closely; use `data::Value` when you need to inspect new or
undocumented fields directly.

Supported groups include applications, audit logs, guilds, guild channels,
permission overwrites, invites, messages, components, embeds, forum tags,
interactions, reactions, emojis, stickers, polls, scheduled events, threads,
webhooks, users, client users, roles and members.

---

## Snowflakes

Every Discord entity is identified by a 64-bit **snowflake**. `Snowflake` is a
newtype over `u64` with helpers for the embedded timestamp and worker metadata:

```rust
use discord_rust::abc::Snowflake;

let id = Snowflake::new(175928847299117063);

println!("created: {:?}", id.created_at());
println!("created (ms): {}", id.timestamp_ms());
println!("worker: {}", id.worker_id());
println!("process: {}", id.process_id());
println!("increment: {}", id.increment());
```

`Snowflake::from_timestamp_ms` and `from_timestamp_ms_high` build synthetic IDs
for tests and pagination. Models implement `HasId`, so generic code can accept
anything that exposes an ID:

```rust
use discord_rust::abc::{HasId, Snowflake};

fn log_id<T: HasId>(value: &T) {
    println!("{}", value.id());
}
```

---

## Decoding

Each model exposes `from_value` (and usually `to_value`) and returns
`models::CastError` with the path of the offending field:

```rust
use discord_rust::data::{self, Value};
use discord_rust::models::User;

let value: Value = data::from_str(r#"{"id":"123","username":"winter"}"#)?;
let user = User::from_value(&value)?;
println!("{}", user.name);
```

The `id` fields are `Snowflake`, not `u64`. Cache lookups and REST helpers take
raw `u64` IDs, while many model and event constructors accept
`impl Into<Snowflake>`. Use `.get()` when you need the raw integer.

---

## CDN assets

`Asset` represents avatars, guild icons, banners, emojis and soundboard sounds.
It stores a CDN URL, the identifying hash and whether the asset is animated.

```rust
use discord_rust::abc::{Asset, AssetFormat};

let avatar = Asset::new(
    "https://cdn.discordapp.com/avatars/12345/abcdef.png",
    "abcdef",
    false,
);

// Build a new asset with a different size and format; both checks are fallible.
let jpeg = avatar.with_size(512)?.with_format(AssetFormat::Jpeg)?;
println!("{}", jpeg.url());
```

Sizes must be powers of two between 16 and 4096. Animated assets accept
`AssetFormat::Gif`; static assets reject it. Convenience constructors such as
`Asset::from_avatar`, `Asset::from_guild_avatar` and
`Asset::from_default_avatar` build the standard CDN URLs for you.

`Asset::read` and `Asset::save` take a fetch closure, so you stay in control of
the HTTP stack:

```rust
use discord_rust::clients::{HttpMethod, HttpRequest, HttpsTransport, Transport};

let cdn = HttpsTransport::new("cdn.discordapp.com", 443);

let bytes = avatar.read(|url| {
    let path = url.trim_start_matches("https://cdn.discordapp.com");
    cdn.execute(&HttpRequest::new(HttpMethod::Get, path))
        .map(|response| response.body)
})?;
```

The closure returns `Result<Vec<u8>, E>`; `save` returns
`AssetSaveError<E>` to distinguish fetch failures from filesystem errors.

---

## Users and members

`User` stores API names under `name`; `username` is not a field.

```rust
use discord_rust::models::{Member, User};

println!("name: {}", user.name);
println!("display name: {:?}", user.global_name);
println!("bot: {}", user.bot);

println!("joined: {:?}", member.joined_at);
println!("deafened: {}", member.deaf);
println!("muted: {}", member.mute);
println!("roles: {}", member.roles.len());
```

`member.user` holds the underlying `User`. Guild-specific avatars and banners
live in `member.guild_avatar` and `member.guild_banner`.

`ClientUser` (the account the bot is logged in as) adds `verified`,
`mfa_enabled` and `locale`. It is exposed on `ReadyEvent::user`.

---

## Message authors

`MessageAuthor` is an enum: guild messages usually carry a `Member`, while
direct messages and webhooks carry a plain `User`. Use the accessors instead of
matching:

```rust
let author = &message.author;
println!("id: {}", author.id().get());
println!("name: {}", author.user().name);
println!("bot: {}", author.user().bot);
```

---

## Embeds

`Embed` has public fields and infallible builder methods:

```rust
use discord_rust::abc::Colour;
use discord_rust::models::{Embed, EmbedAuthor, EmbedField, EmbedFooter};

let embed = Embed::new()
    .with_title("Server Status")
    .with_description("All systems operational.")
    .with_colour(Colour::GREEN)
    .with_author(EmbedAuthor::new("Status Bot"))
    .add_field(EmbedField::new("Uptime", "99.98%").with_inline(true))
    .add_field(EmbedField::new("Ping", "18ms").with_inline(true))
    .with_footer(EmbedFooter::new("Last checked: 1 minute ago"));
```

Send it with `Context::respond_embed` or `DiscordApi::send_embed_typed`.
`Colour` also offers `Color` as an American alias.

---

## Auto moderation

```rust
use discord_rust::models::AutoModRule;

println!("rule: {}", rule.name);
println!("trigger type: {}", rule.trigger_type);
println!("actions: {}", rule.actions.len());
```

`trigger_type` and `event_type` are raw API integers; `enabled`, `exempt_roles`
and `exempt_channels` are decoded.
