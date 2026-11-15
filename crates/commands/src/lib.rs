//! Prefix-command registration, invocation and response primitives.
//!
//! [`CommandRegistry`] parses prefixed messages, runs command checks and
//! invokes handlers. [`Context`] carries parsed arguments together with the
//! message metadata commands commonly need, and [`Responder`] lets handlers
//! send replies without depending on a concrete REST client.

#![forbid(unsafe_code)]

pub use macros::command;

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use data::{Object, Value};
use enums::Permissions;
use models::{CommandOptionValue, Embed, Guild, GuildChannel, Interaction, Member, Message, User};

/// The stage that produced a [`CommandError`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum CommandErrorKind {
    /// An error without a more specific classification.
    #[default]
    Other,
    /// An argument could not be parsed.
    Parsing,
    /// A command check rejected the invocation.
    Check,
    /// The command handler failed.
    Handler,
    /// The response could not be delivered.
    Response,
    /// The context is missing information the command needs.
    Context,
    /// An interaction-specific operation failed.
    Interaction,
}

/// An error raised while parsing or invoking a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandError {
    kind: CommandErrorKind,
    message: String,
}

impl CommandError {
    /// Creates a command error with a human-readable message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            kind: CommandErrorKind::Other,
            message: message.into(),
        }
    }

    /// Creates a command error with an explicit category.
    #[must_use]
    pub fn with_kind(kind: CommandErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Creates a parsing error.
    #[must_use]
    pub fn parsing(message: impl Into<String>) -> Self {
        Self::with_kind(CommandErrorKind::Parsing, message)
    }

    /// Creates a check error.
    #[must_use]
    pub fn check(message: impl Into<String>) -> Self {
        Self::with_kind(CommandErrorKind::Check, message)
    }

    /// Creates a response error.
    #[must_use]
    pub fn response(message: impl Into<String>) -> Self {
        Self::with_kind(CommandErrorKind::Response, message)
    }

    /// Returns the error category.
    #[must_use]
    pub const fn kind(&self) -> CommandErrorKind {
        self.kind
    }

    /// Returns the error message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CommandError {}

impl From<String> for CommandError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<&str> for CommandError {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}

/// Converts the return value of a command handler into a [`CommandError`]
/// result.
///
/// Handlers may return either `()` or `Result<(), E>`. A returned error is
/// reported to the command's error handler and to the caller.
pub trait IntoCommandResult {
    /// Returns `Ok(())` for an infallible handler, or the error.
    fn into_command_result(self) -> Result<(), CommandError>;
}

impl IntoCommandResult for () {
    fn into_command_result(self) -> Result<(), CommandError> {
        Ok(())
    }
}

impl<E: fmt::Display> IntoCommandResult for Result<(), E> {
    fn into_command_result(self) -> Result<(), CommandError> {
        self.map_err(|error| CommandError::new(error.to_string()))
    }
}

/// Sends command responses without coupling the command framework to a
/// particular REST client.
pub trait Responder: Send + Sync {
    /// Sends a plain-text message to a channel.
    ///
    /// # Errors
    ///
    /// Returns an error when the message cannot be sent.
    fn send(&self, channel_id: u64, content: &str) -> Result<(), CommandError>;

    /// Sends a plain-text reply to a message.
    ///
    /// # Errors
    ///
    /// Returns an error when the reply cannot be sent.
    fn reply(&self, channel_id: u64, message_id: u64, content: &str) -> Result<(), CommandError>;

    /// Sends an embed to a channel.
    ///
    /// # Errors
    ///
    /// Returns an error when the embed cannot be sent.
    fn send_embed(&self, channel_id: u64, embed: &Value) -> Result<(), CommandError>;

    /// Shows the typing indicator in a channel.
    ///
    /// # Errors
    ///
    /// Returns an error when the indicator cannot be triggered.
    fn typing(&self, channel_id: u64) -> Result<(), CommandError>;
}

/// Read-only cache access available to command handlers.
///
/// The command crate deliberately does not depend on the client cache type.
/// The bot runtime supplies an implementation backed by its live cache, while
/// tests can provide a small in-memory implementation.
pub trait ContextCache: Send + Sync {
    /// Returns a cached guild.
    fn guild(&self, guild_id: u64) -> Option<Guild>;
    /// Returns a cached channel.
    fn channel(&self, channel_id: u64) -> Option<GuildChannel>;
    /// Returns a cached user.
    fn user(&self, user_id: u64) -> Option<User>;
    /// Returns a cached guild member.
    fn member(&self, guild_id: u64, user_id: u64) -> Option<Member>;
    /// Returns a cached message.
    fn message(&self, message_id: u64) -> Option<Message>;
    /// Computes a member's effective permissions in a channel.
    fn permissions_for(&self, guild_id: u64, user_id: u64, channel_id: u64) -> Option<Permissions>;
}

/// Sends interaction responses without coupling the command framework to a
/// concrete REST client.
pub trait InteractionResponder: Send + Sync {
    /// Sends the initial interaction callback body.
    ///
    /// # Errors
    ///
    /// Returns an error when the callback cannot be sent.
    fn callback(&self, body: &Value) -> Result<(), CommandError>;

    /// Sends a follow-up message after the initial response.
    ///
    /// # Errors
    ///
    /// Returns an error when the follow-up cannot be sent.
    fn followup(&self, body: &Value) -> Result<(), CommandError>;
}

/// A parsed command invocation and the message metadata surrounding it.
#[derive(Clone, Default)]
pub struct Context {
    /// The command name.
    pub name: String,
    /// The remaining arguments.
    pub arguments: Vec<String>,
    /// The guild the invoking message belongs to, if any.
    pub guild_id: Option<u64>,
    /// The channel the invoking message was sent in, if any.
    pub channel_id: Option<u64>,
    /// The user who invoked the command, if known.
    pub user_id: Option<u64>,
    /// The invoking message ID, if known.
    pub message_id: Option<u64>,
    /// The invoking user's permissions in the channel, if known.
    pub permissions: Permissions,
    /// Whether the invoking author is a bot.
    pub author_is_bot: bool,
    /// Slash-command options keyed by name, including nested subcommand options.
    pub command_options: Vec<(String, CommandOptionValue)>,
    /// The invoking message, when the command came from a message.
    pub message: Option<Arc<Message>>,
    /// The guild the command was invoked in, when cached.
    pub guild: Option<Arc<Guild>>,
    /// The invoking channel, when cached.
    pub channel: Option<Arc<GuildChannel>>,
    /// The invoking guild member, when available.
    pub member: Option<Arc<Member>>,
    /// The invoking user.
    pub author: Option<Arc<User>>,
    /// The interaction that invoked the command, when it came from an
    /// application command.
    pub interaction: Option<Arc<Interaction>>,
    cache: Option<Arc<dyn ContextCache>>,
    responder: Option<Arc<dyn Responder>>,
    interaction_responder: Option<Arc<dyn InteractionResponder>>,
}

impl fmt::Debug for Context {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Context")
            .field("name", &self.name)
            .field("arguments", &self.arguments)
            .field("guild_id", &self.guild_id)
            .field("channel_id", &self.channel_id)
            .field("user_id", &self.user_id)
            .field("message_id", &self.message_id)
            .field("permissions", &self.permissions)
            .field("author_is_bot", &self.author_is_bot)
            .field("command_options", &self.command_options.len())
            .field("has_message", &self.message.is_some())
            .field("has_guild", &self.guild.is_some())
            .field("has_channel", &self.channel.is_some())
            .field("has_member", &self.member.is_some())
            .field("has_author", &self.author.is_some())
            .field("has_interaction", &self.interaction.is_some())
            .field("has_cache", &self.cache.is_some())
            .field("has_responder", &self.responder.is_some())
            .field(
                "has_interaction_responder",
                &self.interaction_responder.is_some(),
            )
            .finish()
    }
}

impl PartialEq for Context {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.arguments == other.arguments
            && self.guild_id == other.guild_id
            && self.channel_id == other.channel_id
            && self.user_id == other.user_id
            && self.message_id == other.message_id
            && self.permissions == other.permissions
            && self.author_is_bot == other.author_is_bot
            && self.command_options == other.command_options
    }
}

impl Eq for Context {}

impl Context {
    /// Creates a context from a command name and its arguments.
    #[must_use]
    pub fn new(name: impl Into<String>, arguments: Vec<String>) -> Self {
        Self {
            name: name.into(),
            arguments,
            ..Self::default()
        }
    }

    /// Attaches the message identifiers to the context.
    #[must_use]
    pub fn with_ids(
        mut self,
        guild_id: Option<u64>,
        channel_id: Option<u64>,
        user_id: Option<u64>,
        message_id: Option<u64>,
    ) -> Self {
        self.guild_id = guild_id;
        self.channel_id = channel_id;
        self.user_id = user_id;
        self.message_id = message_id;
        self
    }

    /// Attaches resolved permissions to the context.
    #[must_use]
    pub const fn with_permissions(mut self, permissions: Permissions) -> Self {
        self.permissions = permissions;
        self
    }

    /// Marks whether the invoking author is a bot.
    #[must_use]
    pub const fn with_bot_author(mut self, author_is_bot: bool) -> Self {
        self.author_is_bot = author_is_bot;
        self
    }

    /// Attaches the responder used by the response helpers.
    #[must_use]
    pub fn with_responder(mut self, responder: Arc<dyn Responder>) -> Self {
        self.responder = Some(responder);
        self
    }

    /// Returns the attached responder, if any.
    #[must_use]
    pub fn responder(&self) -> Option<&Arc<dyn Responder>> {
        self.responder.as_ref()
    }

    /// Attaches the interaction responder used by slash-command responses.
    #[must_use]
    pub fn with_interaction_responder(mut self, responder: Arc<dyn InteractionResponder>) -> Self {
        self.interaction_responder = Some(responder);
        self
    }

    /// Attaches the interaction that invoked this command.
    #[must_use]
    pub fn with_interaction(mut self, interaction: Arc<Interaction>) -> Self {
        self.interaction = Some(interaction);
        self
    }

    /// Attaches read-only cache access.
    #[must_use]
    pub fn with_cache(mut self, cache: Arc<dyn ContextCache>) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Attaches the invoking message.
    #[must_use]
    pub fn with_message(mut self, message: Arc<Message>) -> Self {
        self.message = Some(message);
        self
    }

    /// Attaches the invoking guild.
    #[must_use]
    pub fn with_guild(mut self, guild: Arc<Guild>) -> Self {
        self.guild = Some(guild);
        self
    }

    /// Attaches the invoking channel.
    #[must_use]
    pub fn with_channel(mut self, channel: Arc<GuildChannel>) -> Self {
        self.channel = Some(channel);
        self
    }

    /// Attaches the invoking guild member.
    #[must_use]
    pub fn with_member(mut self, member: Arc<Member>) -> Self {
        self.member = Some(member);
        self
    }

    /// Attaches the invoking user.
    #[must_use]
    pub fn with_author(mut self, author: Arc<User>) -> Self {
        self.author = Some(author);
        self
    }

    /// Attaches slash-command options.
    #[must_use]
    pub fn with_command_options(mut self, options: Vec<(String, CommandOptionValue)>) -> Self {
        self.command_options = options;
        self
    }

    /// Returns the attached cache, if any.
    #[must_use]
    pub fn cache(&self) -> Option<&Arc<dyn ContextCache>> {
        self.cache.as_ref()
    }

    /// Returns the interaction that invoked this command, if any.
    #[must_use]
    pub fn interaction(&self) -> Option<&Arc<Interaction>> {
        self.interaction.as_ref()
    }

    /// Returns `true` when this command came from a slash command or context
    /// menu interaction.
    #[must_use]
    pub const fn is_interaction(&self) -> bool {
        self.interaction.is_some() || self.interaction_responder.is_some()
    }

    /// Looks up a slash-command option by name.
    #[must_use]
    pub fn option(&self, name: &str) -> Option<&CommandOptionValue> {
        self.command_options
            .iter()
            .find_map(|(option, value)| (option == name).then_some(value))
    }

    /// Returns a string option by name.
    #[must_use]
    pub fn option_string(&self, name: &str) -> Option<&str> {
        self.option(name).and_then(CommandOptionValue::as_str)
    }

    /// Returns an integer option by name.
    #[must_use]
    pub fn option_i64(&self, name: &str) -> Option<i64> {
        self.option(name).and_then(CommandOptionValue::as_i64)
    }

    /// Returns a boolean option by name.
    #[must_use]
    pub fn option_bool(&self, name: &str) -> Option<bool> {
        self.option(name).and_then(CommandOptionValue::as_bool)
    }

    /// Returns a snowflake option by name.
    #[must_use]
    pub fn option_snowflake(&self, name: &str) -> Option<u64> {
        self.option(name)
            .and_then(CommandOptionValue::as_snowflake)
            .map(|id| id.get())
    }

    /// Attempts to parse argument at `index` into type `T`.
    ///
    /// # Errors
    ///
    /// Returns an error message if the argument is missing or fails to parse.
    pub fn arg<T: FromStr>(&self, index: usize) -> Result<T, CommandError>
    where
        T::Err: fmt::Display,
    {
        let value = self
            .arguments
            .get(index)
            .ok_or_else(|| CommandError::parsing(format!("missing argument at index {index}")))?;
        value.parse::<T>().map_err(|error| {
            CommandError::parsing(format!("invalid argument at index {index}: {error}"))
        })
    }

    /// Parses a user mention argument at `index`.
    ///
    /// # Errors
    ///
    /// Returns an error when the argument is missing or is not a user mention.
    pub fn user_mention(&self, index: usize) -> Result<u64, CommandError> {
        let value = self
            .arguments
            .get(index)
            .ok_or_else(|| CommandError::parsing(format!("missing argument at index {index}")))?;
        Self::parse_user_mention(value).ok_or_else(|| {
            CommandError::parsing(format!("argument at index {index} is not a user mention"))
        })
    }

    /// Parses a channel mention argument at `index`.
    ///
    /// # Errors
    ///
    /// Returns an error when the argument is missing or is not a channel
    /// mention.
    pub fn channel_mention(&self, index: usize) -> Result<u64, CommandError> {
        let value = self
            .arguments
            .get(index)
            .ok_or_else(|| CommandError::parsing(format!("missing argument at index {index}")))?;
        Self::parse_channel_mention(value).ok_or_else(|| {
            CommandError::parsing(format!(
                "argument at index {index} is not a channel mention"
            ))
        })
    }

    /// Parses a role mention argument at `index`.
    ///
    /// # Errors
    ///
    /// Returns an error when the argument is missing or is not a role mention.
    pub fn role_mention(&self, index: usize) -> Result<u64, CommandError> {
        let value = self
            .arguments
            .get(index)
            .ok_or_else(|| CommandError::new(format!("missing argument at index {index}")))?;
        Self::parse_role_mention(value).ok_or_else(|| {
            CommandError::parsing(format!("argument at index {index} is not a role mention"))
        })
    }

    /// Extracts a numeric snowflake ID from a user, channel, or role mention.
    /// Supports `<@123>`, `<@!123>`, `<#123>`, `<@&123>`.
    #[must_use]
    pub fn parse_mention(mention: &str) -> Option<u64> {
        Self::parse_user_mention(mention)
            .or_else(|| Self::parse_channel_mention(mention))
            .or_else(|| Self::parse_role_mention(mention))
    }

    /// Extracts a user ID from `<@123>` or `<@!123>`.
    #[must_use]
    pub fn parse_user_mention(mention: &str) -> Option<u64> {
        let inner = mention.strip_prefix("<@")?.strip_suffix('>')?;
        inner.strip_prefix('!').unwrap_or(inner).parse().ok()
    }

    /// Extracts a channel ID from `<#123>`.
    #[must_use]
    pub fn parse_channel_mention(mention: &str) -> Option<u64> {
        mention.strip_prefix("<#")?.strip_suffix('>')?.parse().ok()
    }

    /// Extracts a role ID from `<@&123>`.
    #[must_use]
    pub fn parse_role_mention(mention: &str) -> Option<u64> {
        mention.strip_prefix("<@&")?.strip_suffix('>')?.parse().ok()
    }

    /// Sends a plain-text response to the invoking channel.
    ///
    /// For interaction commands this sends an immediate interaction callback;
    /// for prefix commands it sends a normal channel message.
    ///
    /// # Errors
    ///
    /// Returns an error when the context has no channel or responder, or the
    /// responder fails.
    pub fn respond(&self, content: impl Into<String>) -> Result<(), CommandError> {
        let content = content.into();
        if let Some(responder) = &self.interaction_responder {
            let mut data = Object::new();
            data.insert("content", Value::from(content));
            return responder.callback(&message_body(4, data));
        }
        let (responder, channel_id) = self.response_target()?;
        responder.send(channel_id, &content)
    }

    /// Sends a reply to the invoking message.
    ///
    /// Interaction responses cannot be edited before they are sent, so this is
    /// equivalent to [`Context::respond`] for slash commands.
    ///
    /// # Errors
    ///
    /// Returns an error when the context has no channel, message or responder,
    /// or the responder fails.
    pub fn reply(&self, content: impl Into<String>) -> Result<(), CommandError> {
        let content = content.into();
        if let Some(responder) = &self.interaction_responder {
            let mut data = Object::new();
            data.insert("content", Value::from(content));
            return responder.callback(&message_body(4, data));
        }
        let (responder, channel_id) = self.response_target()?;
        let message_id = self.message_id.ok_or_else(|| {
            CommandError::with_kind(CommandErrorKind::Context, "command has no message context")
        })?;
        responder.reply(channel_id, message_id, &content)
    }

    /// Sends a typed embed to the invoking channel.
    ///
    /// # Errors
    ///
    /// Returns an error when the context has no channel or responder, or the
    /// responder fails.
    pub fn respond_embed(&self, embed: &Embed) -> Result<(), CommandError> {
        self.respond_embed_value(&embed.to_value())
    }

    /// Sends an already encoded embed to the invoking channel.
    ///
    /// # Errors
    ///
    /// Returns an error when the context has no channel or responder, or the
    /// responder fails.
    pub fn respond_embed_value(&self, embed: &Value) -> Result<(), CommandError> {
        if let Some(responder) = &self.interaction_responder {
            let mut data = Object::new();
            data.insert("embeds", Value::Array(vec![embed.clone()]));
            return responder.callback(&message_body(4, data));
        }
        let (responder, channel_id) = self.response_target()?;
        responder.send_embed(channel_id, embed)
    }

    /// Acknowledges an interaction so a follow-up can be sent later.
    ///
    /// # Errors
    ///
    /// Returns an error when this is not an interaction or the callback fails.
    pub fn defer(&self) -> Result<(), CommandError> {
        let responder = self.interaction_responder.as_ref().ok_or_else(|| {
            CommandError::with_kind(
                CommandErrorKind::Interaction,
                "defer is only available for interactions",
            )
        })?;
        let mut body = Object::new();
        body.insert("type", Value::from(5_u64));
        responder.callback(&Value::Object(body))
    }

    /// Sends a follow-up message after an interaction response.
    ///
    /// # Errors
    ///
    /// Returns an error when this is not an interaction or the request fails.
    pub fn followup(&self, content: impl Into<String>) -> Result<(), CommandError> {
        let responder = self.interaction_responder.as_ref().ok_or_else(|| {
            CommandError::with_kind(
                CommandErrorKind::Interaction,
                "follow-up messages are only available for interactions",
            )
        })?;
        let mut body = Object::new();
        body.insert("content", Value::from(content.into()));
        responder.followup(&Value::Object(body))
    }

    /// Shows the typing indicator in the invoking channel.
    ///
    /// # Errors
    ///
    /// Returns an error when the context has no channel or responder, when this
    /// is an interaction, or when the responder fails.
    pub fn typing(&self) -> Result<(), CommandError> {
        if self.interaction_responder.is_some() {
            return Err(CommandError::with_kind(
                CommandErrorKind::Interaction,
                "typing indicators are not available for interaction responses",
            ));
        }
        let (responder, channel_id) = self.response_target()?;
        responder.typing(channel_id)
    }

    /// Returns `true` when the invoking user has every given permission.
    #[must_use]
    pub const fn has_permissions(&self, permissions: Permissions) -> bool {
        self.permissions.contains(Permissions::ADMINISTRATOR)
            || self.permissions.contains(permissions)
    }

    fn response_target(&self) -> Result<(&dyn Responder, u64), CommandError> {
        let channel_id = self.channel_id.ok_or_else(|| {
            CommandError::with_kind(CommandErrorKind::Context, "command has no channel context")
        })?;
        let responder = self.responder.as_deref().ok_or_else(|| {
            CommandError::with_kind(CommandErrorKind::Context, "no responder configured")
        })?;
        Ok((responder, channel_id))
    }

    fn child(&self, name: String, arguments: Vec<String>) -> Self {
        let mut child = self.clone();
        child.name = name;
        child.arguments = arguments;
        child
    }
}

fn message_body(kind: u64, data: Object) -> Value {
    let mut body = Object::new();
    body.insert("type", Value::from(kind));
    body.insert("data", Value::Object(data));
    Value::Object(body)
}

/// A command group organizing subcommands.
pub struct CommandGroup {
    pub name: String,
    pub description: Option<String>,
    subcommands: Vec<CommandEntry>,
}

impl CommandGroup {
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: None,
            subcommands: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn add_subcommand(
        &mut self,
        name: impl Into<String>,
        aliases: impl IntoIterator<Item = impl Into<String>>,
        handler: impl Handler + 'static,
    ) {
        self.subcommands.push(CommandEntry {
            name: name.into(),
            aliases: aliases.into_iter().map(Into::into).collect(),
            description: None,
            handler: Box::new(handler),
            checks: Vec::new(),
            error_handler: None,
        });
    }

    /// Mounts a subcommand from a [`CommandDefinition`].
    pub fn add_subcommand_definition(&mut self, definition: CommandDefinition) {
        self.subcommands.push(CommandEntry {
            name: definition.name,
            aliases: definition.aliases,
            description: definition.description,
            handler: definition.handler,
            checks: definition.checks,
            error_handler: definition.error_handler,
        });
    }

    /// Dispatches a subcommand.
    ///
    /// Returns `Ok(false)` when no subcommand matches.
    ///
    /// # Errors
    ///
    /// Returns the error produced by a check or handler.
    pub fn dispatch(&mut self, context: &Context) -> Result<bool, CommandError> {
        let Some(sub_name) = context.arguments.first() else {
            return Ok(false);
        };
        let Some(command) = self
            .subcommands
            .iter_mut()
            .find(|command| command.matches(sub_name))
        else {
            return Ok(false);
        };
        let sub_context = context.child(
            format!("{} {sub_name}", context.name),
            context.arguments[1..].to_vec(),
        );
        for check in &command.checks {
            check(&sub_context)?;
        }
        match command.handler.call(&sub_context) {
            Ok(()) => Ok(true),
            Err(error) => {
                if let Some(handler) = &command.error_handler {
                    handler(&sub_context, &error);
                }
                Err(error)
            }
        }
    }

    #[must_use]
    pub fn subcommand_names(&self) -> Vec<&str> {
        self.subcommands
            .iter()
            .map(|command| command.name.as_str())
            .collect()
    }
}

/// A recurring background task loop schedule description.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskLoop {
    interval: Duration,
}

impl TaskLoop {
    #[must_use]
    pub const fn new(interval: Duration) -> Self {
        Self { interval }
    }

    #[must_use]
    pub const fn interval(self) -> Duration {
        self.interval
    }

    /// Spawns a background thread executing `action` every `interval` until `shutdown` is true.
    pub fn start<F>(self, shutdown: Arc<AtomicBool>, mut action: F) -> JoinHandle<()>
    where
        F: FnMut() + Send + 'static,
    {
        thread::spawn(move || {
            while !shutdown.load(Ordering::Relaxed) {
                action();
                thread::sleep(self.interval);
            }
        })
    }
}

/// A command predicate.
pub type Check = Box<dyn Fn(&Context) -> Result<(), CommandError> + Send + Sync>;

/// A handler called when a command returns an error.
pub type ErrorHandler = Box<dyn Fn(&Context, &CommandError) + Send + Sync>;

/// A simple command cooldown.
#[derive(Clone, Debug)]
pub struct Cooldown {
    limit: usize,
    uses: usize,
}

impl Cooldown {
    /// Creates a cooldown with a maximum invocation count.
    #[must_use]
    pub const fn new(limit: usize) -> Self {
        Self { limit, uses: 0 }
    }

    /// Attempts to consume one invocation.
    pub fn allow(&mut self) -> bool {
        if self.uses >= self.limit {
            return false;
        }
        self.uses += 1;
        true
    }

    /// Resets the cooldown.
    pub fn reset(&mut self) {
        self.uses = 0;
    }
}

/// A command handler.
pub trait Handler: Send {
    /// Handles an invocation.
    ///
    /// # Errors
    ///
    /// Returns an error when the command cannot complete.
    fn call(&mut self, context: &Context) -> Result<(), CommandError>;
}

impl<F, R> Handler for F
where
    F: FnMut(&Context) -> R + Send,
    R: IntoCommandResult,
{
    fn call(&mut self, context: &Context) -> Result<(), CommandError> {
        self(context).into_command_result()
    }
}

impl Handler for Box<dyn Handler> {
    fn call(&mut self, context: &Context) -> Result<(), CommandError> {
        (**self).call(context)
    }
}

struct CommandEntry {
    name: String,
    aliases: Vec<String>,
    description: Option<String>,
    handler: Box<dyn Handler>,
    checks: Vec<Check>,
    error_handler: Option<ErrorHandler>,
}

impl CommandEntry {
    fn matches(&self, name: &str) -> bool {
        self.name == name || self.aliases.iter().any(|alias| alias == name)
    }
}

/// A command definition bundling name, aliases, description, checks and
/// handler.
pub struct CommandDefinition {
    pub name: String,
    pub aliases: Vec<String>,
    pub description: Option<String>,
    pub handler: Box<dyn Handler>,
    pub checks: Vec<Check>,
    pub error_handler: Option<ErrorHandler>,
}

impl CommandDefinition {
    /// Creates a new command definition.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        aliases: impl IntoIterator<Item = impl Into<String>>,
        handler: impl Handler + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            aliases: aliases.into_iter().map(Into::into).collect(),
            description: None,
            handler: Box::new(handler),
            checks: Vec::new(),
            error_handler: None,
        }
    }

    /// Sets the command description.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Adds a check that runs before the handler.
    #[must_use]
    pub fn with_check<F>(mut self, check: F) -> Self
    where
        F: Fn(&Context) -> Result<(), CommandError> + Send + Sync + 'static,
    {
        self.checks.push(Box::new(check));
        self
    }

    /// Replaces the command's error handler.
    #[must_use]
    pub fn with_error_handler<F>(mut self, handler: F) -> Self
    where
        F: Fn(&Context, &CommandError) + Send + Sync + 'static,
    {
        self.error_handler = Some(Box::new(handler));
        self
    }
}

/// An extension module for modular bot organization.
pub trait Extension {
    /// The unique name of the extension.
    fn name(&self) -> &str;
    /// Mounts commands and handlers onto the registry.
    ///
    /// # Errors
    ///
    /// Returns an error when a command cannot be registered.
    fn load(&self, registry: &mut CommandRegistry) -> Result<(), &'static str>;
    /// Unmounts commands from the registry.
    fn unload(&self, registry: &mut CommandRegistry) {
        let _ = registry.remove(self.name());
    }
}

/// A command registry with prefix parsing and aliases.
#[derive(Default)]
pub struct CommandRegistry {
    prefix: String,
    commands: Vec<CommandEntry>,
    checks: Vec<Check>,
    error_handler: Option<ErrorHandler>,
}

impl fmt::Debug for CommandRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self
            .commands
            .iter()
            .map(|command| command.name.as_str())
            .collect();
        f.debug_struct("CommandRegistry")
            .field("prefix", &self.prefix)
            .field("commands", &names)
            .finish_non_exhaustive()
    }
}

impl CommandRegistry {
    /// Creates a registry with a prefix.
    #[must_use]
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            commands: Vec::new(),
            checks: Vec::new(),
            error_handler: None,
        }
    }

    /// Registers a command.
    ///
    /// # Errors
    ///
    /// Returns an error when the name or an alias is already registered.
    pub fn add(
        &mut self,
        name: impl Into<String>,
        aliases: impl IntoIterator<Item = impl Into<String>>,
        handler: impl Handler + 'static,
    ) -> Result<(), &'static str> {
        self.add_with_checks(name, aliases, Vec::new(), handler)
    }

    /// Registers a command without aliases.
    ///
    /// This is the common case; [`CommandRegistry::add`] remains available when
    /// aliases are needed.
    ///
    /// # Errors
    ///
    /// Returns an error when the name is already registered.
    pub fn add_command(
        &mut self,
        name: impl Into<String>,
        handler: impl Handler + 'static,
    ) -> Result<(), &'static str> {
        self.add(name, Vec::<String>::new(), handler)
    }

    /// Registers a command with checks.
    ///
    /// # Errors
    ///
    /// Returns an error when the name or an alias is already registered.
    pub fn add_with_checks(
        &mut self,
        name: impl Into<String>,
        aliases: impl IntoIterator<Item = impl Into<String>>,
        checks: Vec<Check>,
        handler: impl Handler + 'static,
    ) -> Result<(), &'static str> {
        let name = name.into();
        let aliases: Vec<String> = aliases.into_iter().map(Into::into).collect();
        if self.name_taken(&name) || aliases.iter().any(|alias| self.name_taken(alias)) {
            return Err("command name already exists");
        }
        self.commands.push(CommandEntry {
            name,
            aliases,
            description: None,
            handler: Box::new(handler),
            checks,
            error_handler: None,
        });
        Ok(())
    }

    /// Mounts a subcommand group.
    ///
    /// # Errors
    ///
    /// Returns an error when the group name is already registered.
    pub fn add_group(&mut self, mut group: CommandGroup) -> Result<(), &'static str> {
        let name = group.name.clone();
        self.add(
            name,
            Vec::<String>::new(),
            move |ctx: &Context| -> Result<(), CommandError> {
                group.dispatch(ctx)?;
                Ok(())
            },
        )
    }

    /// Registers a command from a [`CommandDefinition`].
    ///
    /// # Errors
    ///
    /// Returns an error when the name or an alias is already registered.
    pub fn register(&mut self, definition: CommandDefinition) -> Result<(), &'static str> {
        if self.name_taken(&definition.name)
            || definition
                .aliases
                .iter()
                .any(|alias| self.name_taken(alias))
        {
            return Err("command name already exists");
        }
        self.commands.push(CommandEntry {
            name: definition.name,
            aliases: definition.aliases,
            description: definition.description,
            handler: definition.handler,
            checks: definition.checks,
            error_handler: definition.error_handler,
        });
        Ok(())
    }

    /// Adds a registry-wide check that runs before every command handler.
    pub fn add_check<F>(&mut self, check: F)
    where
        F: Fn(&Context) -> Result<(), CommandError> + Send + Sync + 'static,
    {
        self.checks.push(Box::new(check));
    }

    /// Sets a fallback error handler for commands without their own handler.
    pub fn set_error_handler<F>(&mut self, handler: F)
    where
        F: Fn(&Context, &CommandError) + Send + Sync + 'static,
    {
        self.error_handler = Some(Box::new(handler));
    }

    /// Loads an extension module.
    ///
    /// # Errors
    ///
    /// Returns an error when the extension cannot register its commands.
    pub fn load_extension(&mut self, extension: &dyn Extension) -> Result<(), &'static str> {
        extension.load(self)
    }

    /// Removes a command by name.
    pub fn remove(&mut self, name: &str) -> bool {
        let initial = self.commands.len();
        self.commands.retain(|command| command.name != name);
        self.commands.len() < initial
    }

    /// Generates a human-readable help listing.
    #[must_use]
    pub fn generate_help(&self) -> String {
        let mut help = String::from("Commands:\n");
        for command in &self.commands {
            help.push_str(&format!("  {}{}", self.prefix, command.name));
            if !command.aliases.is_empty() {
                help.push_str(&format!(" (aliases: {})", command.aliases.join(", ")));
            }
            if let Some(description) = &command.description {
                help.push_str(&format!(" - {description}"));
            }
            help.push('\n');
        }
        help
    }

    /// Parses a prefixed message into a [`Context`] without dispatching it.
    ///
    /// Returns `None` when the message does not start with the configured
    /// prefix or has no command name.
    #[must_use]
    pub fn parse_context(&self, message: &str) -> Option<Context> {
        let rest = message.strip_prefix(&self.prefix)?;
        let mut words = Self::tokenize(rest);
        let name = words.next()?;
        Some(Context::new(name, words.collect()))
    }

    /// Parses and dispatches a message.
    ///
    /// # Errors
    ///
    /// Returns an error when a check or handler fails. Returns `Ok(false)` when
    /// the message is not a command.
    pub fn try_dispatch(&mut self, message: &str) -> Result<bool, CommandError> {
        let Some(context) = self.parse_context(message) else {
            return Ok(false);
        };
        self.try_dispatch_with(context)
    }

    /// Parses and dispatches a message.
    ///
    /// This compatibility wrapper returns `true` when the message matched a
    /// command, even if a check or handler failed. Use [`CommandRegistry::try_dispatch`]
    /// to observe errors.
    pub fn dispatch(&mut self, message: &str) -> bool {
        matches!(self.try_dispatch(message), Ok(true) | Err(_))
    }

    /// Parses and dispatches a message only when a predicate accepts it.
    pub fn dispatch_checked(&mut self, message: &str, check: impl Fn(&Context) -> bool) -> bool {
        let Some(context) = self.parse_context(message) else {
            return false;
        };
        if !check(&context) {
            return false;
        }
        matches!(self.try_dispatch_with(context), Ok(true) | Err(_))
    }

    /// Dispatches a context built by the caller.
    ///
    /// This is the entry point used by a bot runtime, which fills in message
    /// metadata before invoking the command.
    ///
    /// # Errors
    ///
    /// Returns an error when a check or handler fails. Returns `Ok(false)` when
    /// no command matches the context name.
    pub fn try_dispatch_with(&mut self, context: Context) -> Result<bool, CommandError> {
        let Some(index) = self
            .commands
            .iter()
            .position(|command| command.matches(&context.name))
        else {
            return Ok(false);
        };

        for check in &self.checks {
            check(&context)?;
        }

        let result = {
            let command = &mut self.commands[index];
            for check in &command.checks {
                check(&context)?;
            }
            command.handler.call(&context)
        };

        match result {
            Ok(()) => Ok(true),
            Err(error) => {
                if let Some(handler) = &self.commands[index].error_handler {
                    handler(&context, &error);
                } else if let Some(handler) = &self.error_handler {
                    handler(&context, &error);
                }
                Err(error)
            }
        }
    }

    fn name_taken(&self, name: &str) -> bool {
        self.commands.iter().any(|command| {
            command.name == name || command.aliases.iter().any(|alias| alias == name)
        })
    }

    fn tokenize(input: &str) -> impl Iterator<Item = String> + '_ {
        let mut values = Vec::new();
        let mut current = String::new();
        let mut quoted = false;
        for character in input.chars() {
            match character {
                '"' => quoted = !quoted,
                character if character.is_whitespace() && !quoted => {
                    if !current.is_empty() {
                        values.push(std::mem::take(&mut current));
                    }
                }
                character => current.push(character),
            }
        }
        if !current.is_empty() {
            values.push(current);
        }
        values.into_iter()
    }

    /// Returns the configured prefix.
    #[must_use]
    pub fn prefix(&self) -> &str {
        &self.prefix
    }
}

/// A slash command registration and its handler.
pub struct SlashCommand {
    /// The command definition sent to Discord.
    pub definition: app::CommandDefinition,
    handler: Box<dyn Handler>,
    error_handler: Option<ErrorHandler>,
}

impl SlashCommand {
    /// Creates a slash command with a name, description and handler.
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        handler: impl Handler + 'static,
    ) -> Self {
        Self::from_definition(app::CommandDefinition::new(name, description), handler)
    }

    /// Creates a registration from an application command definition.
    #[must_use]
    pub fn from_definition(
        definition: app::CommandDefinition,
        handler: impl Handler + 'static,
    ) -> Self {
        Self {
            definition,
            handler: Box::new(handler),
            error_handler: None,
        }
    }

    /// Replaces the command's error handler.
    #[must_use]
    pub fn with_error_handler<F>(mut self, handler: F) -> Self
    where
        F: Fn(&Context, &CommandError) + Send + Sync + 'static,
    {
        self.error_handler = Some(Box::new(handler));
        self
    }
}

/// A registry of application (slash) commands.
///
/// The registry owns command handlers and produces the JSON payload used to
/// register commands with Discord. The bot runtime routes `INTERACTION_CREATE`
/// events to it and supplies an interaction responder through [`Context`].
#[derive(Default)]
pub struct SlashCommandRegistry {
    commands: Vec<SlashCommand>,
    checks: Vec<Check>,
    error_handler: Option<ErrorHandler>,
}

impl fmt::Debug for SlashCommandRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self
            .commands
            .iter()
            .map(|command| command.definition.name.as_str())
            .collect();
        f.debug_struct("SlashCommandRegistry")
            .field("commands", &names)
            .finish_non_exhaustive()
    }
}

impl SlashCommandRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a slash command.
    ///
    /// # Errors
    ///
    /// Returns an error when the name is already registered.
    pub fn add(
        &mut self,
        name: impl Into<String>,
        description: impl Into<String>,
        handler: impl Handler + 'static,
    ) -> Result<(), &'static str> {
        self.register(SlashCommand::new(name, description, handler))
    }

    /// Registers a slash command from a [`SlashCommand`].
    ///
    /// # Errors
    ///
    /// Returns an error when the name is empty or already registered.
    pub fn register(&mut self, command: SlashCommand) -> Result<(), &'static str> {
        let name = &command.definition.name;
        if name.is_empty() {
            return Err("command name cannot be empty");
        }
        if self.name_taken(name) {
            return Err("command name already exists");
        }
        self.commands.push(command);
        Ok(())
    }

    /// Registers a command definition with a handler.
    ///
    /// # Errors
    ///
    /// Returns an error when the name is empty or already registered.
    pub fn register_definition(
        &mut self,
        definition: app::CommandDefinition,
        handler: impl Handler + 'static,
    ) -> Result<(), &'static str> {
        self.register(SlashCommand::from_definition(definition, handler))
    }

    /// Adds a check that runs before every slash command.
    pub fn add_check<F>(&mut self, check: F)
    where
        F: Fn(&Context) -> Result<(), CommandError> + Send + Sync + 'static,
    {
        self.checks.push(Box::new(check));
    }

    /// Sets the fallback error handler for commands without their own.
    pub fn set_error_handler<F>(&mut self, handler: F)
    where
        F: Fn(&Context, &CommandError) + Send + Sync + 'static,
    {
        self.error_handler = Some(Box::new(handler));
    }

    /// Returns the registered command definitions.
    pub fn definitions(&self) -> impl Iterator<Item = &app::CommandDefinition> {
        self.commands.iter().map(|command| &command.definition)
    }

    /// Returns the number of registered commands.
    #[must_use]
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// Returns `true` when no commands are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// Encodes every command as the JSON array Discord expects.
    #[must_use]
    pub fn to_value(&self) -> Value {
        Value::Array(
            self.commands
                .iter()
                .map(|command| command.definition.to_value())
                .collect(),
        )
    }

    /// Dispatches an interaction context by command name.
    ///
    /// # Errors
    ///
    /// Returns an error when a check or handler fails. Returns `Ok(false)` when
    /// no command matches.
    pub fn try_dispatch_with(&mut self, context: Context) -> Result<bool, CommandError> {
        let Some(index) = self
            .commands
            .iter()
            .position(|command| command.definition.name == context.name)
        else {
            return Ok(false);
        };

        for check in &self.checks {
            check(&context)?;
        }

        let result = {
            let command = &mut self.commands[index];
            command.handler.call(&context)
        };

        match result {
            Ok(()) => Ok(true),
            Err(error) => {
                if let Some(handler) = &self.commands[index].error_handler {
                    handler(&context, &error);
                } else if let Some(handler) = &self.error_handler {
                    handler(&context, &error);
                }
                Err(error)
            }
        }
    }

    fn name_taken(&self, name: &str) -> bool {
        self.commands
            .iter()
            .any(|command| command.definition.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_argument_parsing_and_mentions() {
        let ctx = Context::new(
            "test",
            vec!["42".into(), "<@!123456789>".into(), "hello".into()],
        );
        assert_eq!(ctx.arg::<i32>(0), Ok(42));
        assert!(ctx.arg::<i32>(1).is_err());
        assert_eq!(ctx.arg::<String>(2), Ok("hello".into()));
        assert!(ctx.arg::<String>(3).is_err());

        assert_eq!(Context::parse_mention("<@123>"), Some(123));
        assert_eq!(Context::parse_mention("<@!456>"), Some(456));
        assert_eq!(Context::parse_mention("<#789>"), Some(789));
        assert_eq!(Context::parse_mention("<@&1011>"), Some(1011));
        assert_eq!(Context::parse_mention("not_a_mention"), None);

        assert_eq!(ctx.user_mention(1), Ok(123_456_789));
        assert!(ctx.channel_mention(1).is_err());
        assert!(ctx.role_mention(1).is_err());
    }

    #[test]
    fn command_groups_dispatch_subcommands() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let counter = Arc::new(AtomicU32::new(0));
        let c = Arc::clone(&counter);

        let mut group = CommandGroup::new("git").with_description("Git operations");
        group.add_subcommand("commit", vec!["ci"], move |_: &Context| {
            c.fetch_add(1, Ordering::SeqCst);
        });

        assert_eq!(group.subcommand_names(), vec!["commit"]);

        let mut registry = CommandRegistry::new("!");
        assert!(registry.add_group(group).is_ok());

        assert!(registry.dispatch("!git commit"));
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert!(registry.dispatch("!git ci"));
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn extension_modules_load_and_unload() {
        struct MathCog;
        impl Extension for MathCog {
            fn name(&self) -> &str {
                "math"
            }
            fn load(&self, registry: &mut CommandRegistry) -> Result<(), &'static str> {
                registry.add("add", Vec::<String>::new(), |_: &Context| {})
            }
        }

        let mut registry = CommandRegistry::new("!");
        let cog = MathCog;
        assert!(registry.load_extension(&cog).is_ok());
        assert!(registry.remove("add"));
        assert!(!registry.remove("add"));
    }

    #[test]
    fn help_generation_lists_commands() {
        let mut registry = CommandRegistry::new("!");
        registry.add("ping", vec!["p"], |_: &Context| {}).unwrap();
        let help = registry.generate_help();
        assert!(help.contains("!ping (aliases: p)"));
    }

    #[test]
    fn checks_and_errors_are_observable() {
        let mut registry = CommandRegistry::new("!");
        registry
            .add(
                "fail",
                Vec::<String>::new(),
                |_: &Context| -> Result<(), &'static str> { Err("handler failed") },
            )
            .unwrap();
        registry.add_check(|_: &Context| Err(CommandError::new("blocked")));

        assert_eq!(
            registry.try_dispatch("!fail"),
            Err(CommandError::new("blocked"))
        );

        let mut registry = CommandRegistry::new("!");
        registry
            .add(
                "fail",
                Vec::<String>::new(),
                |_: &Context| -> Result<(), &'static str> { Err("handler failed") },
            )
            .unwrap();
        assert_eq!(
            registry.try_dispatch("!fail"),
            Err(CommandError::new("handler failed"))
        );
        assert!(!registry.try_dispatch("not a command").unwrap_or(false));
    }

    #[test]
    fn task_loop_executes_until_shutdown() {
        let task = TaskLoop::new(Duration::from_millis(5));
        let shutdown = Arc::new(AtomicBool::new(false));
        let counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = Arc::clone(&counter);

        let handle = task.start(Arc::clone(&shutdown), move || {
            c.fetch_add(1, Ordering::SeqCst);
        });

        thread::sleep(Duration::from_millis(25));
        shutdown.store(true, Ordering::Relaxed);
        handle.join().unwrap();
        assert!(counter.load(Ordering::SeqCst) >= 2);
    }

    #[test]
    fn proc_macro_command_registration_and_typed_args() {
        use crate as commands;
        use std::sync::atomic::AtomicI32;

        static INVOKED_RESULT: AtomicI32 = AtomicI32::new(0);

        #[command(description = "Adds two numbers", aliases = ["plus", "sum"])]
        fn add(_ctx: &Context, a: i32, b: i32) {
            INVOKED_RESULT.store(a + b, Ordering::SeqCst);
        }

        let mut registry = CommandRegistry::new("!");
        assert!(registry.register(add_command()).is_ok());

        assert!(registry.dispatch("!add 15 27"));
        assert_eq!(INVOKED_RESULT.load(Ordering::SeqCst), 42);

        assert!(registry.dispatch("!plus 100 50"));
        assert_eq!(INVOKED_RESULT.load(Ordering::SeqCst), 150);

        assert!(registry.dispatch("!sum 5 3"));
        assert_eq!(INVOKED_RESULT.load(Ordering::SeqCst), 8);
    }

    #[test]
    fn slash_commands_register_encode_and_dispatch() {
        use std::sync::Mutex;

        let seen = Arc::new(Mutex::new(Vec::new()));
        let output = Arc::clone(&seen);

        let mut registry = SlashCommandRegistry::new();
        registry
            .add("greet", "Greets a member", move |context: &Context| {
                output.lock().unwrap().push(format!(
                    "{}:{}",
                    context.name,
                    context.arguments.join(",")
                ));
            })
            .unwrap();

        assert!(
            registry
                .add("greet", "Duplicate", |_: &Context| {})
                .is_err()
        );
        assert_eq!(registry.len(), 1);

        let encoded = registry.to_value();
        let commands = encoded.as_array().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(
            commands[0]
                .as_object()
                .unwrap()
                .get("name")
                .and_then(Value::as_str),
            Some("greet")
        );

        assert_eq!(
            registry.try_dispatch_with(Context::new("greet", vec!["winter".into()])),
            Ok(true)
        );
        assert_eq!(*seen.lock().unwrap(), vec!["greet:winter"]);
        assert_eq!(
            registry.try_dispatch_with(Context::new("missing", Vec::new())),
            Ok(false)
        );
    }

    #[test]
    fn slash_command_errors_reach_the_error_handler() {
        use std::sync::Mutex;

        let errors = Arc::new(Mutex::new(Vec::new()));
        let output = Arc::clone(&errors);

        let mut registry = SlashCommandRegistry::new();
        registry
            .add("fail", "Fails", |_: &Context| -> Result<(), &'static str> {
                Err("failed")
            })
            .unwrap();
        registry.set_error_handler(move |_, error| {
            output.lock().unwrap().push(error.message().to_owned());
        });

        assert_eq!(
            registry.try_dispatch_with(Context::new("fail", Vec::new())),
            Err(CommandError::new("failed"))
        );
        assert_eq!(*errors.lock().unwrap(), vec!["failed"]);
    }

    #[test]
    fn command_errors_report_their_category() {
        assert_eq!(CommandError::new("x").kind(), CommandErrorKind::Other);
        assert_eq!(
            CommandError::with_kind(CommandErrorKind::Context, "x").kind(),
            CommandErrorKind::Context
        );

        let context = Context::new("test", Vec::new());
        let error = context.respond("hello").unwrap_err();
        assert_eq!(error.kind(), CommandErrorKind::Context);
    }
}
