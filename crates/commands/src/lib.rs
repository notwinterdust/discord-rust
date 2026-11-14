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

use data::Value;
use enums::Permissions;

/// An error raised while parsing or invoking a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandError {
    message: String,
}

impl CommandError {
    /// Creates a command error with a human-readable message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
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
    responder: Option<Arc<dyn Responder>>,
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
            .field("has_responder", &self.responder.is_some())
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
            .ok_or_else(|| CommandError::new(format!("missing argument at index {index}")))?;
        value.parse::<T>().map_err(|error| {
            CommandError::new(format!("invalid argument at index {index}: {error}"))
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
            .ok_or_else(|| CommandError::new(format!("missing argument at index {index}")))?;
        Self::parse_user_mention(value).ok_or_else(|| {
            CommandError::new(format!("argument at index {index} is not a user mention"))
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
            .ok_or_else(|| CommandError::new(format!("missing argument at index {index}")))?;
        Self::parse_channel_mention(value).ok_or_else(|| {
            CommandError::new(format!(
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
            CommandError::new(format!("argument at index {index} is not a role mention"))
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
    /// # Errors
    ///
    /// Returns an error when the context has no channel or responder, or the
    /// responder fails.
    pub fn respond(&self, content: impl Into<String>) -> Result<(), CommandError> {
        let (responder, channel_id) = self.response_target()?;
        responder.send(channel_id, &content.into())
    }

    /// Sends a reply to the invoking message.
    ///
    /// # Errors
    ///
    /// Returns an error when the context has no channel, message or responder,
    /// or the responder fails.
    pub fn reply(&self, content: impl Into<String>) -> Result<(), CommandError> {
        let (responder, channel_id) = self.response_target()?;
        let message_id = self
            .message_id
            .ok_or_else(|| CommandError::new("command has no message context"))?;
        responder.reply(channel_id, message_id, &content.into())
    }

    /// Sends an embed to the invoking channel.
    ///
    /// # Errors
    ///
    /// Returns an error when the context has no channel or responder, or the
    /// responder fails.
    pub fn respond_embed(&self, embed: &Value) -> Result<(), CommandError> {
        let (responder, channel_id) = self.response_target()?;
        responder.send_embed(channel_id, embed)
    }

    /// Shows the typing indicator in the invoking channel.
    ///
    /// # Errors
    ///
    /// Returns an error when the context has no channel or responder, or the
    /// responder fails.
    pub fn typing(&self) -> Result<(), CommandError> {
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
        let channel_id = self
            .channel_id
            .ok_or_else(|| CommandError::new("command has no channel context"))?;
        let responder = self
            .responder
            .as_deref()
            .ok_or_else(|| CommandError::new("no responder configured"))?;
        Ok((responder, channel_id))
    }

    fn child(&self, name: String, arguments: Vec<String>) -> Self {
        let mut child = self.clone();
        child.name = name;
        child.arguments = arguments;
        child
    }
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
}
