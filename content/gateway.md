# Gateway & Event Dispatch

The Discord gateway is a WebSocket connection that delivers real-time events and
carries heartbeats. `discord-rust` implements the protocol synchronously on top
of a `GatewayTransport`.

For most bots, `Bot` is the gateway entry point. The lower-level
`GatewayConnection`, `GatewayRuntime` and `EventDispatcher` types are available
when you need to own the loop or transport.

---

## Lifecycle

`GatewayConnection` implements Discord Gateway v10:

1. **Connect** to the configured gateway URL.
2. **Hello** (`op 10`) provides `heartbeat_interval`.
3. **Identify** (`op 2`) sends the token, intents and shard identity.
4. **Ready** delivers the bot user, session ID and guild list.
5. **Heartbeat** (`op 1`) is scheduled; each heartbeat expects an ACK (`op 11`).
6. On a resumable disconnect, **Resume** (`op 6`) reconnects to
   `resume_gateway_url` and replays from the last sequence.

`GatewayConnection::run_until` stops when the close code is fatal or the supplied
shutdown predicate becomes `true`. `Bot::run_auto` wires this to a TLS-backed
transport and a default REST client.

---

## Bot dispatch order

For every received dispatch event the bot:

1. Applies the event to `ClientCache` on the gateway thread.
2. Enqueues it to the single dispatch worker.
3. On the worker: runs `before_invoke`, prefix/slash command dispatch, then user
   handlers. `after_invoke` runs only when nothing failed.

Cache updates therefore happen before handlers run, and handlers cannot block
heartbeat processing. Errors returned by handlers or commands go to the
`on_error` hook.

---

## Lower-level dispatchers

`EventDispatcher` is a synchronous filter registry. Handlers may be registered by
name, by filter or for every event:

```rust
use discord_rust::clients::EventDispatcher;
use discord_rust::models::Event;

let mut dispatcher = EventDispatcher::new();

dispatcher.on_name("MESSAGE_CREATE", |event| {
    if let Event::MessageCreate(message) = event {
        println!("{}", message.content);
    }
});

dispatcher.on_any(|event| {
    println!("event: {}", event.name());
});

// `event` is a decoded gateway dispatch.
dispatcher.dispatch(&event);
```

For asynchronous dispatch there are two wrappers:

- `ThreadedEventDispatcher`: one background worker, events processed in order.
- `PooledEventDispatcher`: a bounded `ThreadPool`, matching handlers run
  concurrently.

Both expose `builder().on_name(...).on_any(...).build()` and `attach_to` an
`EventDispatcher`.

```rust
use discord_rust::clients::PooledEventDispatcher;
use discord_rust::models::Event;

let dispatcher = PooledEventDispatcher::builder()
    .workers(4)
    .on_name("MESSAGE_CREATE", |event| {
        if let Event::MessageCreate(message) = event {
            println!("worker: {}", message.content);
        }
    })
    .build();
```

> [!NOTE]
> `ThreadedEventDispatcher` handlers may be stateful (`FnMut`) and run
> sequentially. `PooledEventDispatcher` handlers must be `Fn(&Event) + Send +
> Sync`; they may run in any order.

---

## Waiting for events

`EventWaiter` lets a worker block until a matching event arrives:

```rust
use discord_rust::{Event, EventWaiter};
use std::sync::Arc;
use std::time::Duration;

let waiter = Arc::new(EventWaiter::new());

let response = waiter.wait_for(
    |event| matches!(event, Event::MessageCreate(message) if message.content == "!confirm"),
    Duration::from_secs(30),
);

match response {
    Ok(Event::MessageCreate(message)) => {
        println!("confirmed by {}", message.author.user().name);
    }
    Err(_) => println!("timed out"),
    Ok(_) => {}
}
```

`dispatch` delivers the event to registered listeners and keeps a bounded history
(64 events) for waiters that arrive shortly afterwards. `wait_for` returns
`Err(RecvTimeoutError::Timeout)` on timeout; it does not spin.

To wait for real gateway activity, register a bot handler that forwards events to
the waiter:

```rust
use discord_rust::{BotBuilder, EventWaiter};
use std::sync::Arc;

let waiter = Arc::new(EventWaiter::new());
let feed = Arc::clone(&waiter);

let bot = BotBuilder::new()
    .token(token)
    .on_event(move |event| feed.dispatch(event))
    .build()?;

// `waiter` remains available for `wait_for` calls.
```

`EventWaiter` is `Send + Sync`, so an `Arc<EventWaiter>` can be cloned into a
handler while the original stays with the code waiting for a response. If you
only need the waiter inside the handler, move it directly instead of wrapping it
in an `Arc`.
