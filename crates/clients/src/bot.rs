//! Bot builder, gateway runner and event waiter utilities.
//!
//! [`Bot`] wires a [`GatewayTransport`] to the gateway lifecycle, updates a
//! [`ClientCache`] from every dispatched event and calls registered handlers.
//! The transport still owns the socket, WebSocket framing and TLS; this module
//! owns the event loop, cache, hooks and shutdown behaviour.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use commands::{
    CommandError, CommandRegistry, Context, InteractionResponder, Responder, SlashCommandRegistry,
};
use data::Value;
use enums::Intents;
use models::{ApplicationCommandOption, CommandOptionValue, Event, Interaction};

use crate::api::DiscordApi;
use crate::cache::{CacheConfig, ClientCache, SharedCache};
use crate::client::Transport;
#[cfg(feature = "tls")]
use crate::client::{HttpClient, HttpsTransport};
use crate::connection::{
    GatewayConnection, GatewayConnectionError, GatewayTransport, GatewayTransportError,
};
use crate::events::EventFilter;
use crate::gateway::GatewayConfig;
#[cfg(feature = "tls")]
use crate::websocket::{TlsConnector, WebSocketClient, WebSocketGateway};

/// An error raised while assembling a [`Bot`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BotBuildError {
    /// No token was supplied.
    MissingToken,
    /// The supplied token was empty after normalization.
    EmptyToken,
}

impl std::fmt::Display for BotBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingToken => f.write_str("a bot token is required"),
            Self::EmptyToken => f.write_str("the bot token is empty"),
        }
    }
}

impl std::error::Error for BotBuildError {}

/// Strips an optional `Bot ` authorization prefix and surrounding whitespace.
fn normalize_token(token: &str) -> &str {
    let token = token.trim();
    let Some(prefix) = token.get(..3) else {
        return token;
    };
    if !prefix.eq_ignore_ascii_case("bot") {
        return token;
    }
    let rest = token.get(3..).unwrap_or_default();
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        rest.trim_start()
    } else {
        token
    }
}

/// Converts the return value of an event handler into an optional error.
///
/// Handlers may return either `()` or a [`Result<(), E>`](Result). A returned
/// error is passed to the configured `on_error` hook.
pub trait IntoHandlerResult {
    /// Returns `Ok(())` for an infallible handler, or the error message.
    fn into_handler_result(self) -> Result<(), String>;
}

impl IntoHandlerResult for () {
    fn into_handler_result(self) -> Result<(), String> {
        Ok(())
    }
}

impl<E: std::fmt::Display> IntoHandlerResult for Result<(), E> {
    fn into_handler_result(self) -> Result<(), String> {
        self.map_err(|error| error.to_string())
    }
}

/// REST operations the bot runtime needs for interactions and command sync.
///
/// This keeps [`Bot`] independent of a concrete [`DiscordApi`] transport so
/// applications can supply a custom HTTP client.
pub trait BotApi: Send + Sync {
    /// Sends an interaction callback.
    ///
    /// # Errors
    ///
    /// Returns an error when the callback cannot be sent.
    fn interaction_callback(
        &self,
        interaction_id: u64,
        token: &str,
        body: &Value,
    ) -> Result<(), CommandError>;

    /// Sends a follow-up interaction message.
    ///
    /// # Errors
    ///
    /// Returns an error when the follow-up cannot be sent.
    fn interaction_followup(
        &self,
        application_id: u64,
        token: &str,
        body: &Value,
    ) -> Result<(), CommandError>;

    /// Replaces the global application commands.
    ///
    /// # Errors
    ///
    /// Returns an error when the commands cannot be registered.
    fn overwrite_global_commands(
        &self,
        application_id: u64,
        body: &Value,
    ) -> Result<(), CommandError>;

    /// Replaces the guild application commands.
    ///
    /// # Errors
    ///
    /// Returns an error when the commands cannot be registered.
    fn overwrite_guild_commands(
        &self,
        application_id: u64,
        guild_id: u64,
        body: &Value,
    ) -> Result<(), CommandError>;
}

impl<T: Transport + Send + Sync + 'static> BotApi for DiscordApi<T> {
    fn interaction_callback(
        &self,
        interaction_id: u64,
        token: &str,
        body: &Value,
    ) -> Result<(), CommandError> {
        DiscordApi::interaction_callback(self, interaction_id, token, body)
            .map(|_| ())
            .map_err(|error| CommandError::new(error.to_string()))
    }

    fn interaction_followup(
        &self,
        application_id: u64,
        token: &str,
        body: &Value,
    ) -> Result<(), CommandError> {
        DiscordApi::interaction_followup(self, application_id, token, body)
            .map(|_| ())
            .map_err(|error| CommandError::new(error.to_string()))
    }

    fn overwrite_global_commands(
        &self,
        application_id: u64,
        body: &Value,
    ) -> Result<(), CommandError> {
        DiscordApi::overwrite_global_commands(self, application_id, body)
            .map(|_| ())
            .map_err(|error| CommandError::new(error.to_string()))
    }

    fn overwrite_guild_commands(
        &self,
        application_id: u64,
        guild_id: u64,
        body: &Value,
    ) -> Result<(), CommandError> {
        DiscordApi::overwrite_guild_commands(self, application_id, guild_id, body)
            .map(|_| ())
            .map_err(|error| CommandError::new(error.to_string()))
    }
}

type EventHook = Arc<dyn Fn(&Event) + Send + Sync>;
type ErrorHook = Arc<dyn Fn(&Event, &str) + Send + Sync>;

/// Hooks called around event dispatch.
#[derive(Clone, Default)]
pub struct Hooks {
    /// Called before every event dispatch.
    pub before_invoke: Option<EventHook>,
    /// Called after every event whose handlers all succeeded.
    pub after_invoke: Option<EventHook>,
    /// Called when an event handler returns an error (passed as a string).
    pub on_error: Option<ErrorHook>,
}

impl std::fmt::Debug for Hooks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hooks").finish_non_exhaustive()
    }
}

type HandlerFn = Arc<dyn Fn(&Event, &Arc<Mutex<ClientCache>>) -> Result<(), String> + Send + Sync>;

#[derive(Clone)]
struct BotHandler {
    filter: EventFilter,
    handler: HandlerFn,
}

impl BotHandler {
    fn new<F, R>(filter: EventFilter, handler: F) -> Self
    where
        F: Fn(&Event) -> R + Send + Sync + 'static,
        R: IntoHandlerResult + 'static,
    {
        Self {
            filter,
            handler: Arc::new(move |event, _cache| handler(event).into_handler_result()),
        }
    }

    fn new_with_cache<F, R>(filter: EventFilter, handler: F) -> Self
    where
        F: Fn(&Event, &Arc<Mutex<ClientCache>>) -> R + Send + Sync + 'static,
        R: IntoHandlerResult + 'static,
    {
        Self {
            filter,
            handler: Arc::new(move |event, cache| handler(event, cache).into_handler_result()),
        }
    }
}

impl std::fmt::Debug for BotHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BotHandler")
            .field("filter", &self.filter)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
struct ResponderHandle(Arc<dyn Responder>);

impl std::fmt::Debug for ResponderHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ResponderHandle")
    }
}

struct ApiResponder<T> {
    api: DiscordApi<T>,
}

impl<T> ApiResponder<T> {
    const fn new(api: DiscordApi<T>) -> Self {
        Self { api }
    }
}

impl<T: Transport + Send + Sync + 'static> Responder for ApiResponder<T> {
    fn send(&self, channel_id: u64, content: &str) -> Result<(), CommandError> {
        self.api
            .send_message(channel_id, content)
            .map(|_| ())
            .map_err(|error| CommandError::new(error.to_string()))
    }

    fn reply(&self, channel_id: u64, message_id: u64, content: &str) -> Result<(), CommandError> {
        self.api
            .send_reply(channel_id, message_id, content)
            .map(|_| ())
            .map_err(|error| CommandError::new(error.to_string()))
    }

    fn send_embed(&self, channel_id: u64, embed: &Value) -> Result<(), CommandError> {
        self.api
            .send_embed(channel_id, embed)
            .map(|_| ())
            .map_err(|error| CommandError::new(error.to_string()))
    }

    fn typing(&self, channel_id: u64) -> Result<(), CommandError> {
        self.api
            .trigger_typing(channel_id)
            .map(|_| ())
            .map_err(|error| CommandError::new(error.to_string()))
    }
}

struct ApiInteractionResponder {
    api: Arc<dyn BotApi>,
    interaction_id: u64,
    application_id: u64,
    token: String,
}

impl InteractionResponder for ApiInteractionResponder {
    fn callback(&self, body: &Value) -> Result<(), CommandError> {
        self.api
            .interaction_callback(self.interaction_id, &self.token, body)
    }

    fn followup(&self, body: &Value) -> Result<(), CommandError> {
        self.api
            .interaction_followup(self.application_id, &self.token, body)
    }
}

/// An ergonomic bot configuration built with the builder pattern.
///
/// # Example
///
/// ```ignore
/// let bot = Bot::builder()
///     .token("Bot TOKEN")
///     .intents(0x8000)
///     .on_event(|event| println!("{}", event.name()))
///     .build()
///     .unwrap();
/// ```
#[derive(Clone)]
pub struct Bot {
    /// Bot token without the `Bot ` authorization prefix.
    pub token: String,
    /// Gateway intents bitfield.
    pub intents: u64,
    /// Event-dispatch hooks.
    pub hooks: Hooks,
    handlers: Vec<BotHandler>,
    cache: Arc<Mutex<ClientCache>>,
    commands: Option<Arc<Mutex<CommandRegistry>>>,
    slash_commands: Option<Arc<Mutex<SlashCommandRegistry>>>,
    api: Option<Arc<dyn BotApi>>,
    sync_commands: bool,
    command_guild: Option<u64>,
    responder: Option<ResponderHandle>,
}

impl std::fmt::Debug for Bot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bot")
            .field("token", &"<redacted>")
            .field("intents", &self.intents)
            .field("handlers", &self.handlers.len())
            .field("has_commands", &self.commands.is_some())
            .field("has_slash_commands", &self.slash_commands.is_some())
            .field("sync_commands", &self.sync_commands)
            .field("command_guild", &self.command_guild)
            .finish_non_exhaustive()
    }
}

/// Builder for [`Bot`].
pub struct BotBuilder {
    token: Option<String>,
    intents: u64,
    hooks: Hooks,
    handlers: Vec<BotHandler>,
    cache_config: CacheConfig,
    cache: Option<Arc<Mutex<ClientCache>>>,
    commands: Option<Arc<Mutex<CommandRegistry>>>,
    slash_commands: Option<Arc<Mutex<SlashCommandRegistry>>>,
    api: Option<Arc<dyn BotApi>>,
    sync_commands: bool,
    command_guild: Option<u64>,
    responder: Option<ResponderHandle>,
}

impl Default for BotBuilder {
    fn default() -> Self {
        Self {
            token: None,
            intents: 0,
            hooks: Hooks::default(),
            handlers: Vec::new(),
            cache_config: CacheConfig::default(),
            cache: None,
            commands: None,
            slash_commands: None,
            api: None,
            sync_commands: true,
            command_guild: None,
            responder: None,
        }
    }
}

impl std::fmt::Debug for BotBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BotBuilder")
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("intents", &self.intents)
            .field("handlers", &self.handlers.len())
            .field("has_cache", &self.cache.is_some())
            .field("has_commands", &self.commands.is_some())
            .field("has_slash_commands", &self.slash_commands.is_some())
            .field("sync_commands", &self.sync_commands)
            .field("command_guild", &self.command_guild)
            .finish_non_exhaustive()
    }
}

impl BotBuilder {
    /// Creates an empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the bot token.
    ///
    /// Both `TOKEN` and `Bot TOKEN` are accepted. The `Bot ` prefix is removed
    /// during [`BotBuilder::build`]; the REST client adds it back on the
    /// `Authorization` header, while the gateway sends the raw token.
    #[must_use]
    pub fn token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Sets the gateway intents bitfield.
    #[must_use]
    pub const fn intents(mut self, intents: u64) -> Self {
        self.intents = intents;
        self
    }

    /// Sets the gateway intents from an [`Intents`] value.
    #[must_use]
    pub fn intents_flags(mut self, intents: Intents) -> Self {
        self.intents = intents.bits();
        self
    }

    /// Registers a `before_invoke` hook.
    #[must_use]
    pub fn before_invoke<F>(mut self, f: F) -> Self
    where
        F: Fn(&Event) + Send + Sync + 'static,
    {
        self.hooks.before_invoke = Some(Arc::new(f));
        self
    }

    /// Registers an `after_invoke` hook.
    #[must_use]
    pub fn after_invoke<F>(mut self, f: F) -> Self
    where
        F: Fn(&Event) + Send + Sync + 'static,
    {
        self.hooks.after_invoke = Some(Arc::new(f));
        self
    }

    /// Registers an `on_error` hook.
    #[must_use]
    pub fn on_error<F>(mut self, f: F) -> Self
    where
        F: Fn(&Event, &str) + Send + Sync + 'static,
    {
        self.hooks.on_error = Some(Arc::new(f));
        self
    }

    /// Registers a handler for every event.
    ///
    /// Handlers may return `()` or `Result<(), E>`. An error is reported to the
    /// `on_error` hook.
    #[must_use]
    pub fn on_event<F, R>(mut self, handler: F) -> Self
    where
        F: Fn(&Event) -> R + Send + Sync + 'static,
        R: IntoHandlerResult + 'static,
    {
        self.handlers
            .push(BotHandler::new(EventFilter::Any, handler));
        self
    }

    /// Registers a handler that also receives the live cache.
    #[must_use]
    pub fn on_event_with_cache<F, R>(mut self, handler: F) -> Self
    where
        F: Fn(&Event, &Arc<Mutex<ClientCache>>) -> R + Send + Sync + 'static,
        R: IntoHandlerResult + 'static,
    {
        self.handlers
            .push(BotHandler::new_with_cache(EventFilter::Any, handler));
        self
    }

    /// Registers a handler for one gateway event name.
    #[must_use]
    pub fn on_event_name<F, R>(mut self, name: impl Into<String>, handler: F) -> Self
    where
        F: Fn(&Event) -> R + Send + Sync + 'static,
        R: IntoHandlerResult + 'static,
    {
        self.handlers
            .push(BotHandler::new(EventFilter::name(name), handler));
        self
    }

    /// Registers a cache-aware handler for one gateway event name.
    #[must_use]
    pub fn on_event_name_with_cache<F, R>(mut self, name: impl Into<String>, handler: F) -> Self
    where
        F: Fn(&Event, &Arc<Mutex<ClientCache>>) -> R + Send + Sync + 'static,
        R: IntoHandlerResult + 'static,
    {
        self.handlers
            .push(BotHandler::new_with_cache(EventFilter::name(name), handler));
        self
    }

    /// Registers a handler for a custom [`EventFilter`].
    #[must_use]
    pub fn on_event_filter<F, R>(mut self, filter: EventFilter, handler: F) -> Self
    where
        F: Fn(&Event) -> R + Send + Sync + 'static,
        R: IntoHandlerResult + 'static,
    {
        self.handlers.push(BotHandler::new(filter, handler));
        self
    }

    /// Registers a cache-aware handler for a custom [`EventFilter`].
    #[must_use]
    pub fn on_event_filter_with_cache<F, R>(mut self, filter: EventFilter, handler: F) -> Self
    where
        F: Fn(&Event, &Arc<Mutex<ClientCache>>) -> R + Send + Sync + 'static,
        R: IntoHandlerResult + 'static,
    {
        self.handlers
            .push(BotHandler::new_with_cache(filter, handler));
        self
    }

    /// Sets the cache capacity limits used by the bot.
    ///
    /// This is ignored when a cache is supplied with [`BotBuilder::cache`].
    #[must_use]
    pub const fn cache_config(mut self, config: CacheConfig) -> Self {
        self.cache_config = config;
        self
    }

    /// Uses an existing shared cache instead of creating a private one.
    ///
    /// This lets event and command handlers, and the code that calls
    /// [`Bot::cache`], all observe the same live state.
    #[must_use]
    pub fn cache(mut self, cache: Arc<Mutex<ClientCache>>) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Registers a prefix-command registry.
    ///
    /// The bot parses `MESSAGE_CREATE` events with the registry's prefix and
    /// dispatches matching commands before user event handlers run. Bot
    /// authors are ignored.
    #[must_use]
    pub fn commands(mut self, registry: CommandRegistry) -> Self {
        self.commands = Some(Arc::new(Mutex::new(registry)));
        self
    }

    /// Registers a slash-command registry.
    ///
    /// `INTERACTION_CREATE` events are routed to matching commands before user
    /// event handlers run. When [`BotBuilder::sync_commands`] is enabled the
    /// registry is registered with Discord after `READY`.
    #[must_use]
    pub fn slash_commands(mut self, registry: SlashCommandRegistry) -> Self {
        self.slash_commands = Some(Arc::new(Mutex::new(registry)));
        self
    }

    /// Enables or disables automatic slash-command registration.
    ///
    /// Registration is enabled by default and only runs when both a slash
    /// registry and a REST client are available.
    #[must_use]
    pub const fn sync_commands(mut self, sync: bool) -> Self {
        self.sync_commands = sync;
        self
    }

    /// Registers commands in one guild instead of globally.
    ///
    /// Guild commands propagate instantly and are useful during development.
    #[must_use]
    pub const fn command_guild(mut self, guild_id: Option<u64>) -> Self {
        self.command_guild = guild_id;
        self
    }

    /// Sets a custom command responder.
    #[must_use]
    pub fn responder(mut self, responder: Arc<dyn Responder>) -> Self {
        self.responder = Some(ResponderHandle(responder));
        self
    }

    /// Uses a [`DiscordApi`] REST client for command responses, interaction
    /// callbacks and command registration.
    #[must_use]
    pub fn api<T>(mut self, api: DiscordApi<T>) -> Self
    where
        T: Transport + Send + Sync + Clone + 'static,
    {
        self.responder = Some(ResponderHandle(Arc::new(ApiResponder::new(api.clone()))));
        self.api = Some(Arc::new(api));
        self
    }

    /// Builds the [`Bot`].
    ///
    /// # Errors
    ///
    /// Returns [`BotBuildError::MissingToken`] when no token was provided, or
    /// [`BotBuildError::EmptyToken`] when it is empty after normalization.
    pub fn build(self) -> Result<Bot, BotBuildError> {
        let token = self.token.ok_or(BotBuildError::MissingToken)?;
        let token = normalize_token(&token);
        if token.is_empty() {
            return Err(BotBuildError::EmptyToken);
        }
        Ok(Bot {
            token: token.to_owned(),
            intents: self.intents,
            hooks: self.hooks,
            handlers: self.handlers,
            cache: self
                .cache
                .unwrap_or_else(|| Arc::new(Mutex::new(ClientCache::new(self.cache_config)))),
            commands: self.commands,
            slash_commands: self.slash_commands,
            api: self.api,
            sync_commands: self.sync_commands,
            command_guild: self.command_guild,
            responder: self.responder,
        })
    }
}

impl Bot {
    /// Returns a new builder.
    #[must_use]
    pub fn builder() -> BotBuilder {
        BotBuilder::default()
    }

    /// Returns a shared handle to the cache updated by the bot.
    #[must_use]
    pub fn cache(&self) -> Arc<Mutex<ClientCache>> {
        Arc::clone(&self.cache)
    }

    /// Runs the gateway loop on the calling thread until it closes.
    ///
    /// The transport owns the socket, WebSocket framing and TLS. Every
    /// dispatched event updates the cache before the registered handlers run.
    ///
    /// # Errors
    ///
    /// Returns an error when the gateway cannot connect, reconnect or decode a
    /// received frame.
    pub fn run<T: GatewayTransport>(&self, transport: T) -> Result<(), GatewayConnectionError> {
        self.run_with_shutdown(transport, &AtomicBool::new(false))
    }

    /// Runs the bot with a TLS-enabled gateway and REST client.
    ///
    /// This is the zero-configuration entry point: it connects to Discord over
    /// `wss://` with the bundled web public roots and installs a REST responder
    /// from the same token. It is available when the `tls` feature is enabled.
    ///
    /// # Errors
    ///
    /// Returns an error when the gateway cannot connect, reconnect or decode a
    /// received frame.
    #[cfg(feature = "tls")]
    pub fn run_auto(&self) -> Result<(), GatewayConnectionError> {
        self.run(WebSocketGateway::new(WebSocketClient::new(
            TlsConnector::new(),
        )))
    }

    /// Starts [`Bot::run_auto`] on a background thread.
    #[cfg(feature = "tls")]
    #[must_use]
    pub fn start_auto(&self) -> BotHandle {
        self.start(WebSocketGateway::new(WebSocketClient::new(
            TlsConnector::new(),
        )))
    }

    /// Returns a REST responder with automatic TLS when one was not configured.
    #[cfg(feature = "tls")]
    fn default_responder(&self) -> Option<ResponderHandle> {
        let api = DiscordApi::new(HttpClient::new(
            self.token.clone(),
            HttpsTransport::discord(),
        ));
        Some(ResponderHandle(Arc::new(ApiResponder::new(api))))
    }

    /// Returns `None` when the crate is built without the `tls` feature.
    #[cfg(not(feature = "tls"))]
    const fn default_responder(&self) -> Option<ResponderHandle> {
        None
    }

    /// Returns a REST API with automatic TLS when one was not configured.
    #[cfg(feature = "tls")]
    fn default_api(&self) -> Option<Arc<dyn BotApi>> {
        let api = DiscordApi::new(HttpClient::new(
            self.token.clone(),
            HttpsTransport::discord(),
        ));
        Some(Arc::new(api))
    }

    /// Returns `None` when the crate is built without the `tls` feature.
    #[cfg(not(feature = "tls"))]
    const fn default_api(&self) -> Option<Arc<dyn BotApi>> {
        None
    }

    /// Runs the gateway loop on the calling thread until it closes or
    /// `shutdown` becomes `true`.
    ///
    /// # Errors
    ///
    /// Returns an error when the gateway cannot connect, reconnect or decode a
    /// received frame.
    pub fn run_with_shutdown<T: GatewayTransport>(
        &self,
        transport: T,
        shutdown: &AtomicBool,
    ) -> Result<(), GatewayConnectionError> {
        let config = GatewayConfig::new(self.token.clone(), Intents::from(self.intents));
        let mut connection = GatewayConnection::new(config, transport);

        let state = DispatchState {
            cache: Arc::clone(&self.cache),
            hooks: self.hooks.clone(),
            handlers: self.handlers.clone(),
            commands: self.commands.clone(),
            slash_commands: self.slash_commands.clone(),
            api: self.api.clone().or_else(|| self.default_api()),
            sync_commands: self.sync_commands,
            command_guild: self.command_guild,
            synced_commands: Arc::new(AtomicBool::new(false)),
            responder: self.responder.clone().or_else(|| self.default_responder()),
        };

        let worker_state = state.clone();
        let (sender, receiver) = mpsc::channel::<Event>();
        let worker = thread::Builder::new()
            .name(String::from("discord-event-dispatch"))
            .spawn(move || {
                while let Ok(event) = receiver.recv() {
                    dispatch_event(&event, &worker_state);
                }
            })
            .map_err(|error| {
                GatewayConnectionError::Transport(GatewayTransportError::new(format!(
                    "could not start the event dispatcher: {error}"
                )))
            })?;

        let cache = Arc::clone(&self.cache);
        connection.on_any(move |event| {
            {
                let mut cache = cache
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                cache.update(event);
            }
            // Handlers run on the worker; cache updates stay on the gateway
            // thread so handlers always observe the event they were given.
            let _ = sender.send(event.clone());
        });

        let result = connection.run_until(|| shutdown.load(Ordering::SeqCst));
        // Dropping the connection drops the sender; the worker then drains the
        // queue and exits.
        drop(connection);
        let worker_result = worker.join().map_err(|_| {
            GatewayConnectionError::Transport(GatewayTransportError::new(
                "event dispatcher thread panicked",
            ))
        });
        result.and(worker_result)
    }

    /// Starts the gateway loop on a background thread.
    ///
    /// The returned [`BotHandle`] signals shutdown and joins the worker when it
    /// is dropped, or when [`BotHandle::shutdown`] is called.
    #[must_use]
    pub fn start<T>(&self, transport: T) -> BotHandle
    where
        T: GatewayTransport + Send + 'static,
    {
        let bot = self.clone();
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&shutdown);
        let thread = thread::spawn(move || bot.run_with_shutdown(transport, &flag));
        BotHandle {
            shutdown,
            thread: Some(thread),
        }
    }
}

#[derive(Clone)]
struct DispatchState {
    cache: Arc<Mutex<ClientCache>>,
    hooks: Hooks,
    handlers: Vec<BotHandler>,
    commands: Option<Arc<Mutex<CommandRegistry>>>,
    slash_commands: Option<Arc<Mutex<SlashCommandRegistry>>>,
    api: Option<Arc<dyn BotApi>>,
    sync_commands: bool,
    command_guild: Option<u64>,
    synced_commands: Arc<AtomicBool>,
    responder: Option<ResponderHandle>,
}

fn dispatch_event(event: &Event, state: &DispatchState) {
    if let Some(before_invoke) = &state.hooks.before_invoke {
        before_invoke(event);
    }

    let mut failed = false;

    if let Some(error) = command_error_for_event(event, state) {
        failed = true;
        if let Some(on_error) = &state.hooks.on_error {
            on_error(event, error.message());
        }
    }

    for entry in &state.handlers {
        if !entry.filter.matches(event) {
            continue;
        }
        if let Err(message) = (entry.handler)(event, &state.cache) {
            failed = true;
            if let Some(on_error) = &state.hooks.on_error {
                on_error(event, &message);
            }
        }
    }

    if failed {
        return;
    }
    if let Some(after_invoke) = &state.hooks.after_invoke {
        after_invoke(event);
    }
}

fn command_error_for_event(event: &Event, state: &DispatchState) -> Option<CommandError> {
    match event {
        Event::MessageCreate(message) if !message.author.user().bot => {
            let commands = state.commands.as_deref()?;
            dispatch_commands(message, state, commands).err()
        }
        Event::InteractionCreate(interaction) => dispatch_interaction(interaction, state).err(),
        Event::Ready(_) => sync_slash_commands(event, state),
        _ => None,
    }
}

fn dispatch_interaction(
    interaction: &Interaction,
    state: &DispatchState,
) -> Result<bool, CommandError> {
    if interaction.is_ping() {
        if let Some(api) = &state.api {
            let mut body = data::Object::new();
            body.insert("type", Value::from(1_u64));
            api.interaction_callback(
                interaction.id.get(),
                &interaction.token,
                &Value::Object(body),
            )?;
            return Ok(true);
        }
        return Ok(false);
    }

    let Some(registry) = &state.slash_commands else {
        return Ok(false);
    };
    let Some(name) = interaction.command_name() else {
        return Ok(false);
    };

    let guild_id = interaction.guild_id.map(|id| id.get());
    let channel_id = interaction.channel_id.map(|id| id.get());
    let user_id = interaction.user().map(|author| author.id().get());

    let (permissions, guild, channel, member, author) = {
        let cache = state
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let permissions =
            interaction
                .app_permissions
                .unwrap_or_else(|| match (guild_id, user_id, channel_id) {
                    (Some(guild_id), Some(user_id), Some(channel_id)) => cache
                        .permissions_for(guild_id, user_id, channel_id)
                        .unwrap_or_default(),
                    _ => enums::Permissions::none(),
                });
        let author = interaction.user().map(|author| author.user().clone());
        let member = interaction
            .user()
            .and_then(models::MessageAuthor::as_member)
            .cloned();
        (
            permissions,
            guild_id.and_then(|id| cache.guild(id).cloned()),
            channel_id.and_then(|id| cache.channel(id).cloned()),
            member,
            author,
        )
    };

    let mut option_values = Vec::new();
    if let Some(data) = interaction
        .data()
        .and_then(|data| data.application_command())
    {
        collect_options(&data.options, &mut option_values);
    }
    let arguments = option_values
        .iter()
        .map(|(_, value)| option_argument(value))
        .collect();

    let author_is_bot = author.as_ref().is_some_and(|author| author.bot);
    let mut context = Context::new(name, arguments)
        .with_ids(guild_id, channel_id, user_id, None)
        .with_permissions(permissions)
        .with_bot_author(author_is_bot)
        .with_command_options(option_values)
        .with_interaction(Arc::new(interaction.clone()))
        .with_cache(Arc::new(SharedCache::new(Arc::clone(&state.cache))));

    if let Some(author) = author {
        context = context.with_author(Arc::new(author));
    }
    if let Some(guild) = guild {
        context = context.with_guild(Arc::new(guild));
    }
    if let Some(channel) = channel {
        context = context.with_channel(Arc::new(channel));
    }
    if let Some(member) = member {
        context = context.with_member(Arc::new(member));
    }
    if let Some(api) = &state.api {
        context = context.with_interaction_responder(Arc::new(ApiInteractionResponder {
            api: Arc::clone(api),
            interaction_id: interaction.id.get(),
            application_id: interaction.application_id.get(),
            token: interaction.token.clone(),
        }));
    }

    let mut registry = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.try_dispatch_with(context)
}

fn collect_options(
    options: &[ApplicationCommandOption],
    out: &mut Vec<(String, CommandOptionValue)>,
) {
    for option in options {
        if let Some(value) = &option.value {
            out.push((option.name.clone(), value.clone()));
        }
        collect_options(&option.options, out);
    }
}

fn option_argument(value: &CommandOptionValue) -> String {
    match value {
        CommandOptionValue::String(value) => value.clone(),
        CommandOptionValue::Integer(value) => value.to_string(),
        CommandOptionValue::Boolean(value) => value.to_string(),
        CommandOptionValue::Snowflake(value) => value.get().to_string(),
        CommandOptionValue::Number(value) => value.to_string(),
    }
}

fn sync_slash_commands(event: &Event, state: &DispatchState) -> Option<CommandError> {
    if !state.sync_commands || state.synced_commands.load(Ordering::SeqCst) {
        return None;
    }
    let Event::Ready(ready) = event else {
        return None;
    };
    let application = ready.application.as_ref()?;
    let registry = state.slash_commands.as_ref()?;
    let api = state.api.as_ref()?;
    let body = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .to_value();
    let result = if let Some(guild_id) = state.command_guild {
        api.overwrite_guild_commands(application.id.get(), guild_id, &body)
    } else {
        api.overwrite_global_commands(application.id.get(), &body)
    };
    match result {
        Ok(()) => {
            state.synced_commands.store(true, Ordering::SeqCst);
            None
        }
        Err(error) => Some(error),
    }
}

fn dispatch_commands(
    message: &models::Message,
    state: &DispatchState,
    commands: &Mutex<CommandRegistry>,
) -> Result<bool, CommandError> {
    let mut registry = commands
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(mut context) = registry.parse_context(&message.content) else {
        return Ok(false);
    };

    let guild_id = message.guild_id.map(|id| id.get());
    let channel_id = Some(message.channel_id.get());
    let user_id = Some(message.author.id().get());

    let (permissions, guild, channel, member) = {
        let cache = state
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let permissions = match (guild_id, user_id) {
            (Some(guild_id), Some(user_id)) => cache
                .permissions_for(guild_id, user_id, message.channel_id.get())
                .unwrap_or_default(),
            _ => enums::Permissions::none(),
        };
        (
            permissions,
            guild_id.and_then(|id| cache.guild(id).cloned()),
            cache.channel(message.channel_id.get()).cloned(),
            guild_id
                .zip(user_id)
                .and_then(|(guild_id, user_id)| cache.member(guild_id, user_id).cloned()),
        )
    };

    context = context
        .with_ids(guild_id, channel_id, user_id, Some(message.id.get()))
        .with_permissions(permissions)
        .with_bot_author(message.author.user().bot)
        .with_message(Arc::new(message.clone()))
        .with_author(Arc::new(message.author.user().clone()))
        .with_cache(Arc::new(SharedCache::new(Arc::clone(&state.cache))));

    if let Some(guild) = guild {
        context = context.with_guild(Arc::new(guild));
    }
    if let Some(channel) = channel {
        context = context.with_channel(Arc::new(channel));
    }
    if let Some(member) = member {
        context = context.with_member(Arc::new(member));
    }

    if let Some(responder) = &state.responder {
        context = context.with_responder(Arc::clone(&responder.0));
    }

    registry.try_dispatch_with(context)
}

/// A handle to a bot running on a background thread.
pub struct BotHandle {
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<Result<(), GatewayConnectionError>>>,
}

impl BotHandle {
    /// Returns `true` if the worker thread is still running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.thread
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
    }

    /// Requests shutdown and waits for the worker thread to finish.
    ///
    /// # Errors
    ///
    /// Returns an error when the gateway failed or the worker thread panicked.
    pub fn shutdown(self) -> Result<(), GatewayConnectionError> {
        self.shutdown.store(true, Ordering::SeqCst);
        self.join()
    }

    /// Waits for the worker thread to finish without requesting shutdown.
    ///
    /// # Errors
    ///
    /// Returns an error when the gateway failed or the worker thread panicked.
    pub fn join(mut self) -> Result<(), GatewayConnectionError> {
        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        thread.join().map_err(|_| {
            GatewayConnectionError::Transport(GatewayTransportError::new(
                "bot worker thread panicked",
            ))
        })?
    }
}

impl Drop for BotHandle {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl std::fmt::Debug for BotHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BotHandle")
            .field("is_running", &self.is_running())
            .finish()
    }
}

/// The number of recently dispatched events kept for late waiters.
const HISTORY_LIMIT: usize = 64;

/// The listeners and the recent events that no listener received.
#[derive(Default)]
struct WaiterState {
    listeners: Vec<Sender<Event>>,
    history: VecDeque<Event>,
}

/// Subscribes to events matching a predicate, blocking until one arrives or the
/// timeout elapses.
///
/// Each call to [`EventWaiter::wait_for`] registers a one-shot listener on the
/// shared sender that feeds the waiter's internal channel. Events dispatched
/// while no listener was registered are kept in a small bounded history, so a
/// waiter started right after the dispatch still observes them.
pub struct EventWaiter {
    state: Arc<Mutex<WaiterState>>,
}

impl Default for EventWaiter {
    fn default() -> Self {
        Self::new()
    }
}

impl EventWaiter {
    /// Creates a new waiter.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(WaiterState::default())),
        }
    }

    /// Call this from your event-dispatch loop to fan out to any waiting
    /// listeners.
    ///
    /// An event that reaches no listener is remembered in a bounded history,
    /// so a waiter that starts immediately after the dispatch can still match
    /// it.
    pub fn dispatch(&self, event: &Event) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut delivered = false;
        state.listeners.retain(|tx| match tx.send(event.clone()) {
            Ok(()) => {
                delivered = true;
                true
            }
            Err(_) => false,
        });
        if !delivered {
            if state.history.len() == HISTORY_LIMIT {
                state.history.pop_front();
            }
            state.history.push_back(event.clone());
        }
    }

    /// Returns the number of active listeners.
    #[must_use]
    pub fn listener_count(&self) -> usize {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.listeners.len()
    }

    /// Blocks the calling thread until an event matching `predicate` arrives or
    /// `timeout` elapses.
    ///
    /// # Errors
    ///
    /// Returns [`RecvTimeoutError::Timeout`] when the deadline is reached before
    /// a matching event arrives, or [`RecvTimeoutError::Disconnected`] when the
    /// waiter is dropped.
    pub fn wait_for<F>(&self, predicate: F, timeout: Duration) -> Result<Event, RecvTimeoutError>
    where
        F: Fn(&Event) -> bool,
    {
        let (tx, rx): (Sender<Event>, Receiver<Event>) = mpsc::channel();
        {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(index) = state.history.iter().rposition(&predicate) {
                return Ok(state
                    .history
                    .remove(index)
                    .expect("history index came from the same deque"));
            }
            state.listeners.push(tx);
        }
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(RecvTimeoutError::Timeout);
            }
            match rx.recv_timeout(remaining) {
                Ok(event) if predicate(&event) => return Ok(event),
                Ok(_) => continue,
                Err(e) => return Err(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use commands::{CommandError, CommandRegistry, Context, Responder};

    #[derive(Default)]
    struct RecordingResponder {
        messages: Mutex<Vec<String>>,
    }

    impl Responder for RecordingResponder {
        fn send(&self, channel_id: u64, content: &str) -> Result<(), CommandError> {
            self.messages
                .lock()
                .unwrap()
                .push(format!("send {channel_id}: {content}"));
            Ok(())
        }

        fn reply(
            &self,
            channel_id: u64,
            message_id: u64,
            content: &str,
        ) -> Result<(), CommandError> {
            self.messages
                .lock()
                .unwrap()
                .push(format!("reply {channel_id}/{message_id}: {content}"));
            Ok(())
        }

        fn send_embed(&self, channel_id: u64, _embed: &data::Value) -> Result<(), CommandError> {
            self.messages
                .lock()
                .unwrap()
                .push(format!("embed {channel_id}"));
            Ok(())
        }

        fn typing(&self, channel_id: u64) -> Result<(), CommandError> {
            self.messages
                .lock()
                .unwrap()
                .push(format!("typing {channel_id}"));
            Ok(())
        }
    }

    #[test]
    fn bot_builder_requires_token() {
        assert!(Bot::builder().intents(0x8000).build().is_err());
    }

    #[test]
    fn bot_builder_produces_bot() {
        let bot = Bot::builder()
            .token("Bot TOKEN")
            .intents(0x8000)
            .build()
            .unwrap();
        assert_eq!(bot.token, "TOKEN");
        assert_eq!(bot.intents, 0x8000);
    }

    #[test]
    fn bot_builder_normalizes_and_validates_tokens() {
        let bot = Bot::builder().token("  Bot   TOKEN  ").build().unwrap();
        assert_eq!(bot.token, "TOKEN");

        let bot = Bot::builder().token("raw-token").build().unwrap();
        assert_eq!(bot.token, "raw-token");

        assert_eq!(
            Bot::builder().token("Bot ").build().unwrap_err(),
            BotBuildError::EmptyToken
        );
    }

    #[test]
    fn event_waiter_dispatches_to_listener() {
        use std::sync::Arc;
        use std::thread;

        let waiter = Arc::new(EventWaiter::new());
        let waiter2 = Arc::clone(&waiter);

        let handle = thread::spawn(move || {
            waiter2.wait_for(|e| matches!(e, Event::Resumed), Duration::from_secs(2))
        });

        // Wait until the listener is registered.
        while waiter.listener_count() == 0 {
            thread::sleep(Duration::from_millis(5));
        }

        // Dispatch a non-matching event first (e.g. unknown), then the matching one.
        waiter.dispatch(&Event::Unknown {
            name: "OTHER".into(),
            data: data::Value::Null,
        });
        waiter.dispatch(&Event::Resumed);

        let result = handle.join().unwrap();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), Event::Resumed);
    }

    #[test]
    fn event_waiter_times_out() {
        let waiter = EventWaiter::new();
        let result = waiter.wait_for(|_| false, Duration::from_millis(10));
        assert_eq!(result, Err(RecvTimeoutError::Timeout));
    }

    #[test]
    fn event_waiter_replays_undelivered_events_once() {
        let waiter = EventWaiter::new();
        waiter.dispatch(&Event::Resumed);

        let result = waiter.wait_for(|e| matches!(e, Event::Resumed), Duration::from_millis(10));
        assert_eq!(result, Ok(Event::Resumed));

        let result = waiter.wait_for(|e| matches!(e, Event::Resumed), Duration::from_millis(10));
        assert_eq!(result, Err(RecvTimeoutError::Timeout));
    }

    #[test]
    fn event_waiter_does_not_replay_delivered_events() {
        use std::sync::Arc;
        use std::thread;

        let waiter = Arc::new(EventWaiter::new());
        let listener = Arc::clone(&waiter);
        let handle = thread::spawn(move || listener.wait_for(|_| true, Duration::from_secs(2)));

        while waiter.listener_count() == 0 {
            thread::sleep(Duration::from_millis(1));
        }
        waiter.dispatch(&Event::Resumed);

        assert_eq!(handle.join().unwrap(), Ok(Event::Resumed));
        assert_eq!(
            waiter.wait_for(|_| true, Duration::from_millis(10)),
            Err(RecvTimeoutError::Timeout)
        );
    }

    #[test]
    fn event_waiter_history_is_bounded() {
        let waiter = EventWaiter::new();
        for index in 0..=HISTORY_LIMIT {
            waiter.dispatch(&Event::Unknown {
                name: format!("EVENT_{index}"),
                data: data::Value::Null,
            });
        }

        assert_eq!(
            waiter.wait_for(|e| e.name() == "EVENT_0", Duration::from_millis(10)),
            Err(RecvTimeoutError::Timeout)
        );
        let newest = format!("EVENT_{HISTORY_LIMIT}");
        let event = waiter
            .wait_for(|e| e.name() == newest, Duration::from_millis(10))
            .unwrap();
        assert_eq!(event.name(), newest);
    }

    #[test]
    fn bot_builder_hooks_are_stored() {
        let bot = Bot::builder()
            .token("Bot T")
            .before_invoke(|_| {})
            .after_invoke(|_| {})
            .on_error(|_, _| {})
            .build()
            .unwrap();
        assert!(bot.hooks.before_invoke.is_some());
        assert!(bot.hooks.after_invoke.is_some());
        assert!(bot.hooks.on_error.is_some());
    }

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    use data::GatewayCloseCode;
    use data::gateway::{GatewayOpcode, GatewayPayload, Hello};

    use crate::connection::{GatewayFrame, GatewayTransportError};

    #[derive(Debug, Default)]
    struct ScriptedTransport {
        frames: VecDeque<GatewayFrame>,
        sent: Vec<GatewayPayload>,
        closes: usize,
    }

    impl GatewayTransport for ScriptedTransport {
        fn connect(&mut self, _url: &str) -> Result<(), GatewayTransportError> {
            Ok(())
        }

        fn recv(
            &mut self,
            _timeout: Option<Duration>,
        ) -> Result<GatewayFrame, GatewayTransportError> {
            match self.frames.pop_front() {
                Some(frame) => Ok(frame),
                None => {
                    thread::sleep(Duration::from_millis(1));
                    Ok(GatewayFrame::Timeout)
                }
            }
        }

        fn send(&mut self, text: &str) -> Result<(), GatewayTransportError> {
            self.sent.push(GatewayPayload::from_json(text).unwrap());
            Ok(())
        }

        fn close(&mut self) {
            self.closes += 1;
        }
    }

    fn hello_frame() -> GatewayFrame {
        GatewayFrame::Text(
            GatewayPayload::new(
                GatewayOpcode::Hello,
                Hello {
                    heartbeat_interval: 60_000,
                }
                .to_value(),
            )
            .to_json(),
        )
    }

    fn ready_frame() -> GatewayFrame {
        GatewayFrame::Text(
            GatewayPayload::from_json(
                r#"{
                    "op": 0,
                    "s": 1,
                    "t": "READY",
                    "d": {
                        "user": {"id": "1", "username": "winter"},
                        "session_id": "session",
                        "guilds": []
                    }
                }"#,
            )
            .unwrap()
            .to_json(),
        )
    }

    fn message_frame() -> GatewayFrame {
        message_frame_with("hello", false)
    }

    fn message_frame_with(content: &str, bot_author: bool) -> GatewayFrame {
        let bot_field = if bot_author { r#", "bot": true"# } else { "" };
        let payload = format!(
            r#"{{
                "op": 0,
                "s": 2,
                "t": "MESSAGE_CREATE",
                "d": {{
                    "id": "10",
                    "channel_id": "4",
                    "guild_id": "2",
                    "author": {{"id": "6", "username": "winter"{bot_field}}},
                    "content": "{content}",
                    "timestamp": "2021-08-01T12:34:56Z"
                }}
            }}"#
        );
        GatewayFrame::Text(GatewayPayload::from_json(&payload).unwrap().to_json())
    }

    fn close_frame() -> GatewayFrame {
        GatewayFrame::Close(GatewayCloseCode::AUTHENTICATION_FAILED)
    }

    #[test]
    fn bot_run_dispatches_events_updates_cache_and_hooks() {
        let before = Arc::new(AtomicUsize::new(0));
        let after = Arc::new(AtomicUsize::new(0));
        let messages = Arc::new(AtomicUsize::new(0));
        let any = Arc::new(AtomicUsize::new(0));

        let before_hook = Arc::clone(&before);
        let after_hook = Arc::clone(&after);
        let message_handler = Arc::clone(&messages);
        let any_handler = Arc::clone(&any);

        let bot = Bot::builder()
            .token("Bot TOKEN")
            .intents_flags(Intents::none())
            .on_event_name("MESSAGE_CREATE", move |event| {
                if let Event::MessageCreate(message) = event {
                    assert_eq!(message.content, "hello");
                    message_handler.fetch_add(1, Ordering::SeqCst);
                }
            })
            .on_event(move |_| {
                any_handler.fetch_add(1, Ordering::SeqCst);
            })
            .before_invoke(move |_| {
                before_hook.fetch_add(1, Ordering::SeqCst);
            })
            .after_invoke(move |_| {
                after_hook.fetch_add(1, Ordering::SeqCst);
            })
            .build()
            .unwrap();

        let transport = ScriptedTransport {
            frames: VecDeque::from([hello_frame(), ready_frame(), message_frame(), close_frame()]),
            ..ScriptedTransport::default()
        };

        bot.run(transport).unwrap();

        assert_eq!(messages.load(Ordering::SeqCst), 1);
        assert_eq!(any.load(Ordering::SeqCst), 2);
        assert_eq!(before.load(Ordering::SeqCst), 2);
        assert_eq!(after.load(Ordering::SeqCst), 2);

        let cache = bot.cache();
        let cache = cache.lock().unwrap();
        assert_eq!(cache.messages.get(10).unwrap().content, "hello");
    }

    #[test]
    fn bot_handler_errors_call_on_error_and_skip_after_invoke() {
        let after = Arc::new(AtomicUsize::new(0));
        let errors = Arc::new(Mutex::new(Vec::new()));

        let after_hook = Arc::clone(&after);
        let error_hook = Arc::clone(&errors);

        let bot = Bot::builder()
            .token("Bot TOKEN")
            .on_event(|_| -> Result<(), &'static str> { Err("handler failed") })
            .after_invoke(move |_| {
                after_hook.fetch_add(1, Ordering::SeqCst);
            })
            .on_error(move |_, message| {
                error_hook.lock().unwrap().push(message.to_owned());
            })
            .build()
            .unwrap();

        let transport = ScriptedTransport {
            frames: VecDeque::from([hello_frame(), ready_frame(), close_frame()]),
            ..ScriptedTransport::default()
        };

        bot.run(transport).unwrap();

        assert_eq!(after.load(Ordering::SeqCst), 0);
        assert_eq!(*errors.lock().unwrap(), vec!["handler failed"]);
    }

    #[test]
    fn bot_dispatches_commands_with_metadata_and_responder() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let output = Arc::clone(&seen);

        let mut registry = CommandRegistry::new("!");
        registry
            .add("ping", Vec::<String>::new(), move |context: &Context| {
                output.lock().unwrap().push(format!(
                    "{}:{}:{}:{}",
                    context.name,
                    context.user_id.unwrap_or_default(),
                    context.channel_id.unwrap_or_default(),
                    context.arguments.join(",")
                ));
                context.respond("pong")
            })
            .unwrap();

        let responder = Arc::new(RecordingResponder::default());
        let responses = Arc::clone(&responder);
        let handler_calls = Arc::new(AtomicUsize::new(0));
        let message_handlers = Arc::clone(&handler_calls);

        let bot = Bot::builder()
            .token("Bot TOKEN")
            .intents_flags(Intents::none())
            .commands(registry)
            .responder(responder)
            .on_event_name("MESSAGE_CREATE", move |_| {
                message_handlers.fetch_add(1, Ordering::SeqCst);
            })
            .build()
            .unwrap();

        let transport = ScriptedTransport {
            frames: VecDeque::from([
                hello_frame(),
                ready_frame(),
                message_frame_with("!ping hello world", false),
                close_frame(),
            ]),
            ..ScriptedTransport::default()
        };

        bot.run(transport).unwrap();

        assert_eq!(*seen.lock().unwrap(), vec!["ping:6:4:hello,world"]);
        assert_eq!(*responses.messages.lock().unwrap(), vec!["send 4: pong"]);
        assert_eq!(handler_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn bot_ignores_commands_from_bot_authors() {
        let called = Arc::new(AtomicBool::new(false));
        let command_calls = Arc::clone(&called);

        let mut registry = CommandRegistry::new("!");
        registry
            .add("ping", Vec::<String>::new(), move |_: &Context| {
                command_calls.store(true, Ordering::SeqCst);
            })
            .unwrap();

        let bot = Bot::builder()
            .token("Bot TOKEN")
            .commands(registry)
            .build()
            .unwrap();

        let transport = ScriptedTransport {
            frames: VecDeque::from([
                hello_frame(),
                ready_frame(),
                message_frame_with("!ping", true),
                close_frame(),
            ]),
            ..ScriptedTransport::default()
        };

        bot.run(transport).unwrap();
        assert!(!called.load(Ordering::SeqCst));
    }

    #[test]
    fn bot_reports_command_errors_and_skips_after_invoke() {
        let errors = Arc::new(Mutex::new(Vec::new()));
        let after = Arc::new(AtomicUsize::new(0));

        let mut registry = CommandRegistry::new("!");
        registry
            .add(
                "fail",
                Vec::<String>::new(),
                |_: &Context| -> Result<(), &'static str> { Err("command failed") },
            )
            .unwrap();

        let error_hook = Arc::clone(&errors);
        let after_hook = Arc::clone(&after);
        let bot = Bot::builder()
            .token("Bot TOKEN")
            .commands(registry)
            .after_invoke(move |_| {
                after_hook.fetch_add(1, Ordering::SeqCst);
            })
            .on_error(move |_, message| {
                error_hook.lock().unwrap().push(message.to_owned());
            })
            .build()
            .unwrap();

        let transport = ScriptedTransport {
            frames: VecDeque::from([
                hello_frame(),
                ready_frame(),
                message_frame_with("!fail", false),
                close_frame(),
            ]),
            ..ScriptedTransport::default()
        };

        bot.run(transport).unwrap();

        assert_eq!(*errors.lock().unwrap(), vec!["command failed"]);
        assert_eq!(after.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn bot_starts_and_shuts_down_cleanly() {
        let bot = Bot::builder().token("Bot TOKEN").build().unwrap();

        let handle = bot.start(ScriptedTransport::default());
        assert!(handle.is_running());
        handle.shutdown().unwrap();
    }

    use enums::{ApplicationCommandOptionType, ApplicationCommandType, InteractionType};
    use models::{
        ApplicationCommandData, ClientUser, InteractionData, PartialApplication, ReadyEvent, User,
    };

    #[derive(Default)]
    struct RecordingApi {
        callbacks: Mutex<Vec<(u64, String, Value)>>,
        followups: Mutex<Vec<(u64, String, Value)>>,
        global_commands: Mutex<Vec<(u64, Value)>>,
        guild_commands: Mutex<Vec<(u64, u64, Value)>>,
    }

    impl BotApi for RecordingApi {
        fn interaction_callback(
            &self,
            interaction_id: u64,
            token: &str,
            body: &Value,
        ) -> Result<(), CommandError> {
            self.callbacks
                .lock()
                .unwrap()
                .push((interaction_id, token.to_owned(), body.clone()));
            Ok(())
        }

        fn interaction_followup(
            &self,
            application_id: u64,
            token: &str,
            body: &Value,
        ) -> Result<(), CommandError> {
            self.followups
                .lock()
                .unwrap()
                .push((application_id, token.to_owned(), body.clone()));
            Ok(())
        }

        fn overwrite_global_commands(
            &self,
            application_id: u64,
            body: &Value,
        ) -> Result<(), CommandError> {
            self.global_commands
                .lock()
                .unwrap()
                .push((application_id, body.clone()));
            Ok(())
        }

        fn overwrite_guild_commands(
            &self,
            application_id: u64,
            guild_id: u64,
            body: &Value,
        ) -> Result<(), CommandError> {
            self.guild_commands
                .lock()
                .unwrap()
                .push((application_id, guild_id, body.clone()));
            Ok(())
        }
    }

    fn dispatch_state(registry: SlashCommandRegistry, api: Arc<dyn BotApi>) -> DispatchState {
        DispatchState {
            cache: Arc::new(Mutex::new(ClientCache::new(CacheConfig::default()))),
            hooks: Hooks::default(),
            handlers: Vec::new(),
            commands: None,
            slash_commands: Some(Arc::new(Mutex::new(registry))),
            api: Some(api),
            sync_commands: true,
            command_guild: None,
            synced_commands: Arc::new(AtomicBool::new(false)),
            responder: None,
        }
    }

    fn greet_interaction() -> Interaction {
        let option = ApplicationCommandOption::new("name", ApplicationCommandOptionType::String)
            .with_value("winter");
        let data = ApplicationCommandData::new(1_u64, "greet", ApplicationCommandType::ChatInput)
            .with_options(vec![option]);

        Interaction::new(
            20_u64,
            InteractionType::ApplicationCommand,
            21_u64,
            "interaction-token",
        )
        .with_data(InteractionData::ApplicationCommand(Box::new(data)))
        .with_guild_id(2_u64)
        .with_channel_id(4_u64)
        .with_user(User::new(6_u64, "winter"))
    }

    fn ready_event() -> Event {
        Event::Ready(ReadyEvent {
            version: 10,
            user: ClientUser::new(User::new(1_u64, "winter")),
            guilds: Vec::new(),
            session_id: String::from("session"),
            resume_gateway_url: None,
            application: Some(PartialApplication::new(21_u64, "app")),
        })
    }

    fn message_content(body: &Value) -> Option<&str> {
        body.as_object()?
            .get("data")?
            .as_object()?
            .get("content")?
            .as_str()
    }

    #[test]
    fn interaction_dispatch_routes_to_slash_commands() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let output = Arc::clone(&seen);

        let mut registry = SlashCommandRegistry::new();
        registry
            .add("greet", "Greets someone", move |context: &Context| {
                let name = context.option_string("name").unwrap_or_default().to_owned();
                output.lock().unwrap().push(name.clone());
                context.respond(format!("hello {name}"))?;
                context.followup("again")
            })
            .unwrap();

        let recorder = Arc::new(RecordingApi::default());
        let api: Arc<dyn BotApi> = recorder.clone();
        let state = dispatch_state(registry, api);

        let event = Event::InteractionCreate(greet_interaction());
        assert_eq!(command_error_for_event(&event, &state), None);
        assert_eq!(*seen.lock().unwrap(), vec!["winter"]);

        let callbacks = recorder.callbacks.lock().unwrap();
        assert_eq!(callbacks.len(), 1);
        assert_eq!(callbacks[0].0, 20);
        assert_eq!(callbacks[0].1, "interaction-token");
        assert_eq!(
            callbacks[0].2.as_object().unwrap().get("type"),
            Some(&Value::from(4_u64))
        );
        assert_eq!(message_content(&callbacks[0].2), Some("hello winter"));

        let followups = recorder.followups.lock().unwrap();
        assert_eq!(followups.len(), 1);
        assert_eq!(followups[0].0, 21);
        assert_eq!(
            followups[0]
                .2
                .as_object()
                .unwrap()
                .get("content")
                .and_then(Value::as_str),
            Some("again")
        );
    }

    #[test]
    fn ping_interactions_are_acknowledged() {
        let recorder = Arc::new(RecordingApi::default());
        let api: Arc<dyn BotApi> = recorder.clone();
        let state = dispatch_state(SlashCommandRegistry::new(), api);

        let ping = Interaction::new(20_u64, InteractionType::Ping, 21_u64, "token");
        let event = Event::InteractionCreate(ping);

        assert_eq!(command_error_for_event(&event, &state), None);
        let callbacks = recorder.callbacks.lock().unwrap();
        assert_eq!(callbacks.len(), 1);
        assert_eq!(
            callbacks[0].2.as_object().unwrap().get("type"),
            Some(&Value::from(1_u64))
        );
    }

    #[test]
    fn slash_commands_sync_once_after_ready() {
        let recorder = Arc::new(RecordingApi::default());
        let api: Arc<dyn BotApi> = recorder.clone();

        let mut registry = SlashCommandRegistry::new();
        registry
            .add("greet", "Greets someone", |_: &Context| {})
            .unwrap();
        let state = dispatch_state(registry, api);

        let ready = ready_event();
        assert_eq!(command_error_for_event(&ready, &state), None);

        let global = recorder.global_commands.lock().unwrap();
        assert_eq!(global.len(), 1);
        assert_eq!(global[0].0, 21);
        assert_eq!(global[0].1.as_array().unwrap().len(), 1);
        drop(global);

        // A second READY must not re-register the commands.
        assert_eq!(command_error_for_event(&ready, &state), None);
        assert_eq!(recorder.global_commands.lock().unwrap().len(), 1);
    }

    #[test]
    fn slash_commands_can_target_one_guild() {
        let recorder = Arc::new(RecordingApi::default());
        let api: Arc<dyn BotApi> = recorder.clone();

        let mut registry = SlashCommandRegistry::new();
        registry
            .add("greet", "Greets someone", |_: &Context| {})
            .unwrap();
        let mut state = dispatch_state(registry, api);
        state.command_guild = Some(99);

        assert_eq!(command_error_for_event(&ready_event(), &state), None);

        let guild = recorder.guild_commands.lock().unwrap();
        assert_eq!(guild.len(), 1);
        assert_eq!(guild[0].0, 21);
        assert_eq!(guild[0].1, 99);
        assert!(recorder.global_commands.lock().unwrap().is_empty());
    }

    #[test]
    fn cache_aware_handlers_run_on_the_dispatch_worker() {
        let observed = Arc::new(AtomicBool::new(false));
        let handler_observed = Arc::clone(&observed);
        let handler_thread = Arc::new(Mutex::new(None));
        let thread_slot = Arc::clone(&handler_thread);

        let bot = Bot::builder()
            .token("Bot TOKEN")
            .intents_flags(Intents::none())
            .on_event_name_with_cache("MESSAGE_CREATE", move |event, cache| {
                if let Event::MessageCreate(message) = event {
                    let cache = cache.lock().unwrap();
                    if cache.messages.get(message.id.get()).is_some() {
                        handler_observed.store(true, Ordering::SeqCst);
                    }
                }
                *thread_slot.lock().unwrap() = Some(thread::current().id());
            })
            .build()
            .unwrap();

        let transport = ScriptedTransport {
            frames: VecDeque::from([hello_frame(), ready_frame(), message_frame(), close_frame()]),
            ..ScriptedTransport::default()
        };

        bot.run(transport).unwrap();

        assert!(observed.load(Ordering::SeqCst));
        assert_ne!(
            *handler_thread.lock().unwrap(),
            Some(thread::current().id())
        );
    }
}
