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

use commands::{CommandError, CommandRegistry, Responder};
use data::Value;
use enums::Intents;
use models::Event;

use crate::api::DiscordApi;
use crate::cache::{CacheConfig, ClientCache};
use crate::client::Transport;
use crate::connection::{
    GatewayConnection, GatewayConnectionError, GatewayTransport, GatewayTransportError,
};
use crate::events::EventFilter;
use crate::gateway::GatewayConfig;

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

type HandlerFn = Arc<dyn Fn(&Event) -> Result<(), String> + Send + Sync>;

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
            handler: Arc::new(move |event| handler(event).into_handler_result()),
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
#[derive(Clone, Debug)]
pub struct Bot {
    /// Bot token (including the `Bot ` prefix).
    pub token: String,
    /// Gateway intents bitfield.
    pub intents: u64,
    /// Event-dispatch hooks.
    pub hooks: Hooks,
    handlers: Vec<BotHandler>,
    cache: Arc<Mutex<ClientCache>>,
    commands: Option<Arc<Mutex<CommandRegistry>>>,
    responder: Option<ResponderHandle>,
}

/// Builder for [`Bot`].
#[derive(Debug, Default)]
pub struct BotBuilder {
    token: Option<String>,
    intents: u64,
    hooks: Hooks,
    handlers: Vec<BotHandler>,
    cache_config: CacheConfig,
    commands: Option<Arc<Mutex<CommandRegistry>>>,
    responder: Option<ResponderHandle>,
}

impl BotBuilder {
    /// Creates an empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the bot token.
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

    /// Sets the cache capacity limits used by the bot.
    #[must_use]
    pub const fn cache_config(mut self, config: CacheConfig) -> Self {
        self.cache_config = config;
        self
    }

    /// Registers a command registry.
    ///
    /// The bot parses `MessageCreate` events with the registry's prefix and
    /// dispatches matching commands before user event handlers run. Bot
    /// authors are ignored.
    #[must_use]
    pub fn commands(mut self, registry: CommandRegistry) -> Self {
        self.commands = Some(Arc::new(Mutex::new(registry)));
        self
    }

    /// Sets a custom command responder.
    #[must_use]
    pub fn responder(mut self, responder: Arc<dyn Responder>) -> Self {
        self.responder = Some(ResponderHandle(responder));
        self
    }

    /// Uses a [`DiscordApi`] REST client for command responses.
    #[must_use]
    pub fn api<T>(mut self, api: DiscordApi<T>) -> Self
    where
        T: Transport + Send + Sync + 'static,
    {
        self.responder = Some(ResponderHandle(Arc::new(ApiResponder::new(api))));
        self
    }

    /// Builds the [`Bot`].
    ///
    /// # Errors
    ///
    /// Returns an error when no token was provided.
    pub fn build(self) -> Result<Bot, &'static str> {
        let token = self.token.ok_or("token is required")?;
        Ok(Bot {
            token,
            intents: self.intents,
            hooks: self.hooks,
            handlers: self.handlers,
            cache: Arc::new(Mutex::new(ClientCache::new(self.cache_config))),
            commands: self.commands,
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

        let cache = Arc::clone(&self.cache);
        let hooks = self.hooks.clone();
        let handlers = self.handlers.clone();
        let commands = self.commands.clone();
        let responder = self.responder.clone();
        connection.on_any(move |event| {
            dispatch_event(
                event,
                &cache,
                &hooks,
                &handlers,
                commands.as_deref(),
                responder.as_ref(),
            );
        });

        connection.run_until(|| shutdown.load(Ordering::SeqCst))
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

fn dispatch_event(
    event: &Event,
    cache: &Mutex<ClientCache>,
    hooks: &Hooks,
    handlers: &[BotHandler],
    commands: Option<&Mutex<CommandRegistry>>,
    responder: Option<&ResponderHandle>,
) {
    {
        let mut cache = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.update(event);
    }

    if let Some(before_invoke) = &hooks.before_invoke {
        before_invoke(event);
    }

    let mut failed = false;

    if let Some(error) = command_error_for_event(event, cache, commands, responder) {
        failed = true;
        if let Some(on_error) = &hooks.on_error {
            on_error(event, error.message());
        }
    }

    for entry in handlers {
        if !entry.filter.matches(event) {
            continue;
        }
        if let Err(message) = (entry.handler)(event) {
            failed = true;
            if let Some(on_error) = &hooks.on_error {
                on_error(event, &message);
            }
        }
    }

    if failed {
        return;
    }
    if let Some(after_invoke) = &hooks.after_invoke {
        after_invoke(event);
    }
}

fn command_error_for_event(
    event: &Event,
    cache: &Mutex<ClientCache>,
    commands: Option<&Mutex<CommandRegistry>>,
    responder: Option<&ResponderHandle>,
) -> Option<CommandError> {
    let Event::MessageCreate(message) = event else {
        return None;
    };
    if message.author.user().bot {
        return None;
    }
    let commands = commands?;
    dispatch_commands(message, cache, commands, responder).err()
}

fn dispatch_commands(
    message: &models::Message,
    cache: &Mutex<ClientCache>,
    commands: &Mutex<CommandRegistry>,
    responder: Option<&ResponderHandle>,
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
    let permissions = {
        let cache = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match (guild_id, user_id) {
            (Some(guild_id), Some(user_id)) => cache
                .permissions_for(guild_id, user_id, message.channel_id.get())
                .unwrap_or_default(),
            _ => enums::Permissions::none(),
        }
    };

    context = context
        .with_ids(guild_id, channel_id, user_id, Some(message.id.get()))
        .with_permissions(permissions)
        .with_bot_author(message.author.user().bot);

    if let Some(responder) = responder {
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
        assert_eq!(bot.token, "Bot TOKEN");
        assert_eq!(bot.intents, 0x8000);
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
}
