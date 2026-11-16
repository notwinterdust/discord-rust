# Sharding

A shard is one gateway WebSocket connection. Large bots split guilds across
shards so each connection handles a subset of events.

`discord-rust` provides two pieces:

- `ShardManager` assigns shard IDs and identify delays.
- `ConcurrentShardRuntime` runs one thread per shard and merges their raw event
  payloads into one channel.

---

## Shard assignments

Each shard is a `Shard { id, count }`, where `id` is zero-based and `count` is
the total number of shards. Discord maps a guild to a shard with:

```text
shard_id = (guild_id >> 22) % shard_count
```

```rust
use discord_rust::clients::shard::Shard;

let shard = Shard::new(0, 4)?;
assert_eq!(shard.id, 0);
assert_eq!(shard.count, 4);
```

`Shard::new` rejects `count == 0` and `id >= count`.

---

## Building a manager

`/gateway/bot` returns a recommended shard count. Pass it directly:

```rust
use discord_rust::clients::shard::ShardManager;

// 10 recommended shards, 1 identify per 5-second bucket.
let manager = ShardManager::from_gateway_recommendation(10, 1)?;

println!("shards: {}", manager.shards().len());
for shard in manager.shards() {
    println!("shard {} of {}", shard.id, shard.count);
}
```

`ShardManager::new(count, max_concurrency)` builds a fixed set.
`from_gateway_recommendation` raises both values to at least 1; `new` uses the
count it is given.

Identify delays respect Discord's rate limit: shard `id` waits
`(id % max_concurrency) * 5` seconds before identifying.

```rust
let delay = manager.identify_delay(manager.shards()[3]);
println!("wait {:?}", delay);
```

---

## Running shards concurrently

`ConcurrentShardRuntime::spawn` starts one OS thread per shard. Each thread
sleeps for its identify delay, then calls your closure with the `Shard` and a
`SyncSender` for raw event payloads:

```rust
use discord_rust::clients::shard::{ConcurrentShardRuntime, Shard, ShardManager};
use std::sync::mpsc::SyncSender;

let manager = ShardManager::from_gateway_recommendation(4, 1)?;
let runtime = ConcurrentShardRuntime::new(manager);

let handles = runtime.spawn(|shard: Shard, events: SyncSender<(Shard, String)>| {
    // Run the gateway loop for this shard and forward raw payloads.
    // `events.send((shard, payload))` feeds the shared receiver.
    println!("shard {} started", shard.id);
});

for handle in handles {
    let shard = handle.shard;
    let _ = handle.join();
    println!("shard {} stopped", shard.id);
}
```

The runtime owns the receiving end:

```rust
for (shard, payload) in runtime.event_rx.iter() {
    println!("shard {}: {}", shard.id, payload);
}
```

`event_rx` is a bounded channel (capacity 256). If a consumer falls behind, shard
threads block on `send`, which applies backpressure instead of growing memory
without bound.

> [!TIP]
> Shards run on separate OS threads, so a slow handler or heavy traffic on one
> shard does not stall the others.

---

## Shard identity in the gateway

`GatewayConfig` carries the shard identity sent in `IDENTIFY`. Set it per
connection so Discord knows which slice of guilds the session owns:

```rust
use discord_rust::clients::GatewayConfig;
use discord_rust::data::ShardId;
use discord_rust::enums::Intents;

let mut config = GatewayConfig::new(token, Intents::default());
config.shard = Some(ShardId::new(shard.id, shard.count));
```

Use `[0, 1]` for an unsharded bot. `data::ShardId` is the wire type sent in
`IDENTIFY`; `clients::shard::Shard` is the assignment type used by
`ShardManager`.
