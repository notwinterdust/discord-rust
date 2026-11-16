# Commands & Interactions

`discord-rust` has two command systems that share the same `Context`:

- **Prefix commands** parse `MESSAGE_CREATE` content against a prefix such as `!`.
- **Slash commands** are registered with Discord and invoked through
  `INTERACTION_CREATE`.

Both are dispatched before user event handlers, and both ignore bot authors.
`Context::respond` works for either source.

---

## Slash commands

Register a name, description and handler:

```rust
use discord_rust::{Context, SlashCommandRegistry};

let mut slash = SlashCommandRegistry::new();
slash
    .add("ping", "Check that the bot is online", |context: &Context| {
        context.respond("Pong!")
    })
    .expect("register /ping");
```

Attach the registry to the bot:

```rust
let bot = BotBuilder::new()
    .token(token)
    .slash_commands(slash)
    .build()?;
```

When the bot receives `READY`, it registers the definitions with Discord. This is
enabled by default; use `.sync_commands(false)` to manage registration yourself.
Guild commands register instantly and are useful during development:

```rust
.command_guild(Some(123456789012345678))
```

### Definitions and options

Use `app::CommandDefinition` and `app::CommandOption` for typed options:

```rust
use discord_rust::app::{CommandDefinition, CommandOption};
use discord_rust::{Context, SlashCommandRegistry};

let definition = CommandDefinition::new("echo", "Repeat a message")
    .with_typed_option(
        CommandOption::string("text", "Text to echo").required(true),
    );

let mut slash = SlashCommandRegistry::new();
slash
    .register_definition(definition, |context: &Context| {
        context.respond(context.option_string("text").unwrap_or_default())
    })
    .expect("register /echo");
```

Available `CommandOption` constructors: `string`, `integer`, `boolean`, `user`,
`channel`, `role`, `mentionable`, `number`, `attachment`, `subcommand` and
`subcommand_group`. Nested options use `CommandOption::with_option`.

### Reading options

`Context` stores flattened options by name, including nested subcommand options:

```rust
let text = context.option_string("text");
let count = context.option_i64("count");
let enabled = context.option_bool("enabled");
let user_id = context.option_snowflake("target");

if let Some(value) = context.option("text") {
    println!("{value:?}");
}
```

Prefix-command arguments remain positional and are read with
`context.arg::<T>(index)`.

### Context menus

Context menu commands use the same registry:

```rust
use discord_rust::app::CommandDefinition;
use discord_rust::models::InteractionData;

let report = CommandDefinition::user_context_menu("Report User");
let bookmark = CommandDefinition::message_context_menu("Bookmark");

slash.register_definition(report, |context: &Context| {
    if let Some(data) = context.interaction().and_then(|interaction| interaction.data.as_ref()) {
        if let InteractionData::ApplicationCommand(command) = data {
            println!("target: {:?}", command.target_id);
        }
    }
    context.respond("Thanks, the report was recorded.")
})?;
```

---

## Prefix commands

Prefix commands are registered on a `CommandRegistry`:

```rust
use discord_rust::{CommandRegistry, Context};

let mut registry = CommandRegistry::new("!");
registry
    .add_command("ping", |context: &Context| context.respond("Pong!"))
    .expect("register !ping");
```

Aliases are available through the lower-level `add`:

```rust
registry.add(
    "ping",
    ["p", "latency"],
    |context: &Context| context.respond("Pong!"),
)?;
```

Attach the registry with `.commands(registry)`. The bot parses messages with the
registry prefix and ignores bot authors before dispatching.

### Command groups

```rust
use discord_rust::{CommandGroup, CommandRegistry, Context};

let mut config = CommandGroup::new("config").with_description("Server settings");
config.add_subcommand("view", Vec::<String>::new(), |context: &Context| {
    context.respond("current settings")
});
config.add_subcommand("set", Vec::<String>::new(), |context: &Context| {
    context.respond("updated")
});

let mut registry = CommandRegistry::new("!");
registry.add_group(config)?;
```

`!config view` and `!config set` now dispatch to their handlers.

### Checks and error handlers

```rust
use discord_rust::{CommandError, CommandErrorKind, CommandRegistry, Context};

let mut registry = CommandRegistry::new("!");
registry.add_check(|context: &Context| {
    if context.has_permissions(discord_rust::enums::Permissions::MANAGE_GUILD) {
        Ok(())
    } else {
        Err(CommandError::check("missing Manage Server"))
    }
});
registry.set_error_handler(|context: &Context, error: &CommandError| {
    eprintln!("/{} failed ({:?}): {}", context.name, error.kind(), error.message());
});
```

`CommandErrorKind` distinguishes `Parsing`, `Check`, `Handler`, `Response`,
`Context` and `Interaction` failures.

---

## The Context

`Context` carries everything a handler commonly needs:

| Field / method | Meaning |
|---|---|
| `name`, `arguments` | Invoked command and positional arguments |
| `guild_id`, `channel_id`, `user_id`, `message_id` | Snowflake IDs, when available |
| `permissions` | Resolved member permissions (or interaction permissions) |
| `message`, `author`, `member` | Invoking message and author models |
| `guild`, `channel` | Cached guild and channel models |
| `interaction` | The raw interaction for slash and context-menu commands |
| `cache()` | Read-only cache access through `ContextCache` |
| `is_interaction()` | Whether the command came from an interaction |

`ContextCache` exposes the common lookups used by command handlers without
making `commands` depend on the concrete cache type:

```rust
if let Some(cache) = context.cache() {
    if let Some(guild) = cache.guild(context.guild_id.unwrap_or_default()) {
        println!("guild: {}", guild.name);
    }
}
```

`permissions` is computed from the cache when the member is known. For
interactions, Discord's `app_permissions` value is preferred.

---

## Responding

| Method | Prefix command | Slash command |
|---|---|---|
| `respond(content)` | Sends a channel message | Sends a channel-message interaction callback |
| `reply(content)` | Replies to the invoking message | Same as `respond` |
| `respond_embed(&Embed)` | Sends an embed | Sends an embed callback |
| `defer()` | Error | Acknowledges the interaction for a later follow-up |
| `followup(content)` | Error | Sends a follow-up webhook message |
| `typing()` | Shows typing | Error |

```rust
use discord_rust::models::Embed;

fn announce(context: &Context) -> Result<(), discord_rust::CommandError> {
    if context.is_interaction() {
        context.defer()?;
        context.followup("Working on it...")?;
    } else {
        context.typing()?;
    }

    let embed = Embed {
        title: Some("Done".to_owned()),
        description: Some("The operation finished.".to_owned()),
        ..Embed::default()
    };
    context.respond_embed(&embed)
}
```

Responses require either an interaction responder (installed automatically for
slash commands) or a `Responder` (`BotBuilder::responder`). With the `tls`
feature, the bot installs a REST responder automatically.

---

## Interaction lifecycle

When `INTERACTION_CREATE` arrives:

1. Ping interactions are acknowledged with a type-1 callback.
2. Command interactions are matched against the slash registry by name.
3. The context is built with flattened options, resolved permissions, cached
   guild/channel/member models and the live cache.
4. An interaction responder is attached, so `respond`, `defer` and `followup`
   target the correct interaction token.
5. Command checks and the handler run on the dispatch worker.

A failed handler or check is passed to the command error handler, then reported
to `on_error` along with other handler failures.
