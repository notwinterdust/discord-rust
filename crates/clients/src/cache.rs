//! Bounded identifier-keyed caches for client models.
//!
//! [`Cache`] is the low-level building block: a bounded `HashMap` keyed by
//! `u64` ID.  [`ClientCache`] composes per-type [`Cache`]s and updates them
//! when fed [`models::Event`] values from the gateway.

use std::collections::HashMap;

use enums::Permissions;

/// A simple bounded cache keyed by Discord IDs.
#[derive(Clone, Debug)]
pub struct Cache<T> {
    capacity: usize,
    values: HashMap<u64, T>,
}

impl<T> Cache<T> {
    /// Creates a cache with a maximum entry count.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            values: HashMap::new(),
        }
    }

    /// Inserts a value, evicting an arbitrary entry when full.
    pub fn insert(&mut self, id: u64, value: T) {
        if self.capacity == 0 {
            return;
        }
        if self.values.len() >= self.capacity && !self.values.contains_key(&id) {
            if let Some(key) = self.values.keys().next().copied() {
                self.values.remove(&key);
            }
        }
        self.values.insert(id, value);
    }

    /// Returns a cached value.
    #[must_use]
    pub fn get(&self, id: u64) -> Option<&T> {
        self.values.get(&id)
    }

    /// Returns a mutable reference to a cached value.
    pub fn get_mut(&mut self, id: u64) -> Option<&mut T> {
        self.values.get_mut(&id)
    }

    /// Removes a cached value.
    pub fn remove(&mut self, id: u64) -> Option<T> {
        self.values.remove(&id)
    }

    /// Clears the cache.
    pub fn clear(&mut self) {
        self.values.clear();
    }

    /// Removes all identifiers supplied by an invalidation event.
    pub fn invalidate<I>(&mut self, ids: I)
    where
        I: IntoIterator<Item = u64>,
    {
        for id in ids {
            self.values.remove(&id);
        }
    }

    /// Removes every entry for which `keep` returns `false`.
    pub fn retain<F>(&mut self, mut keep: F)
    where
        F: FnMut(u64, &T) -> bool,
    {
        self.values.retain(|id, value| keep(*id, value));
    }

    /// Returns the current number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns `true` when the cache holds no entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Returns an iterator over all cached values.
    pub fn values(&self) -> impl Iterator<Item = &T> {
        self.values.values()
    }
}

// ---------------------------------------------------------------------------
// Per-guild member sub-cache
// ---------------------------------------------------------------------------

/// Members belonging to a single guild, keyed by user ID.
#[derive(Clone, Debug, Default)]
pub struct MemberCache {
    members: HashMap<u64, models::Member>,
}

impl MemberCache {
    fn insert(&mut self, member: models::Member) {
        self.members.insert(member.user.id.get(), member);
    }

    fn remove(&mut self, user_id: u64) {
        self.members.remove(&user_id);
    }

    /// Returns the cached member for the given user ID.
    #[must_use]
    pub fn get(&self, user_id: u64) -> Option<&models::Member> {
        self.members.get(&user_id)
    }

    /// Returns the number of cached members.
    #[must_use]
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Returns an iterator over the cached members.
    pub fn values(&self) -> impl Iterator<Item = &models::Member> {
        self.members.values()
    }

    /// Returns `true` when no members are cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Message edit and response correlation tracker
// ---------------------------------------------------------------------------

/// Bounded bidirectional tracker mapping command messages to bot response messages.
#[derive(Clone, Debug)]
pub struct MessageEditTracker {
    capacity: usize,
    user_to_response: HashMap<u64, u64>,
    response_to_user: HashMap<u64, u64>,
}

impl MessageEditTracker {
    /// Creates a tracker with a maximum entry count.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            user_to_response: HashMap::new(),
            response_to_user: HashMap::new(),
        }
    }

    /// Registers that a bot response was sent in reply to a user message.
    pub fn register_response(&mut self, user_message_id: u64, bot_response_id: u64) {
        if self.capacity == 0 {
            return;
        }
        if self.user_to_response.len() >= self.capacity
            && !self.user_to_response.contains_key(&user_message_id)
        {
            if let Some(old_user_id) = self.user_to_response.keys().next().copied() {
                if let Some(old_resp_id) = self.user_to_response.remove(&old_user_id) {
                    self.response_to_user.remove(&old_resp_id);
                }
            }
        }
        self.user_to_response
            .insert(user_message_id, bot_response_id);
        self.response_to_user
            .insert(bot_response_id, user_message_id);
    }

    /// Looks up the bot response message ID for a given user command message ID.
    #[must_use]
    pub fn get_response(&self, user_message_id: u64) -> Option<u64> {
        self.user_to_response.get(&user_message_id).copied()
    }

    /// Looks up the user command message ID for a given bot response message ID.
    #[must_use]
    pub fn get_user_message(&self, bot_response_id: u64) -> Option<u64> {
        self.response_to_user.get(&bot_response_id).copied()
    }

    /// Removes tracking for a user message, returning the associated bot response ID if any.
    pub fn remove_user_message(&mut self, user_message_id: u64) -> Option<u64> {
        let resp_id = self.user_to_response.remove(&user_message_id)?;
        self.response_to_user.remove(&resp_id);
        Some(resp_id)
    }

    /// Removes tracking for a bot response, returning the associated user message ID if any.
    pub fn remove_response(&mut self, bot_response_id: u64) -> Option<u64> {
        let user_id = self.response_to_user.remove(&bot_response_id)?;
        self.user_to_response.remove(&user_id);
        Some(user_id)
    }

    /// Clears all tracked associations.
    pub fn clear(&mut self) {
        self.user_to_response.clear();
        self.response_to_user.clear();
    }

    /// Returns the number of tracked message pairs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.user_to_response.len()
    }

    /// Returns whether the tracker is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.user_to_response.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Composite client cache
// ---------------------------------------------------------------------------

/// Configuration for [`ClientCache`] entry limits.
#[derive(Clone, Debug)]
pub struct CacheConfig {
    /// Maximum number of guilds to cache (0 = disabled).
    pub guilds: usize,
    /// Maximum number of channels to cache.
    pub channels: usize,
    /// Maximum number of roles to cache.
    pub roles: usize,
    /// Maximum number of users to cache.
    pub users: usize,
    /// Maximum number of messages to cache.
    pub messages: usize,
    /// Maximum number of presences to cache.
    pub presences: usize,
    /// Maximum number of webhooks to cache.
    pub webhooks: usize,
    /// Maximum number of message edit responses to track.
    pub message_edits: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            guilds: 1_000,
            channels: 10_000,
            roles: 10_000,
            users: 10_000,
            messages: 1_000,
            presences: 10_000,
            webhooks: 1_000,
            message_edits: 1_000,
        }
    }
}

/// Composite cache holding typed per-resource stores updated from gateway events.
///
/// Call [`ClientCache::update`] with every [`models::Event`] received from the
/// gateway to keep the caches consistent.
#[derive(Clone, Debug)]
pub struct ClientCache {
    /// Cached guilds.
    pub guilds: Cache<models::Guild>,
    /// Cached channels (guild channels and threads).
    pub channels: Cache<models::GuildChannel>,
    /// Cached roles, keyed by role ID.
    pub roles: Cache<models::Role>,
    /// Cached users (populated from member/presence events).
    pub users: Cache<models::User>,
    /// Cached messages.
    pub messages: Cache<models::Message>,
    /// Cached presences, keyed by user ID.
    pub presences: Cache<models::PresenceUpdate>,
    /// Cached webhooks, keyed by webhook ID.
    pub webhooks: Cache<models::Webhook>,
    /// Per-guild member caches, keyed by guild ID.
    pub members: HashMap<u64, MemberCache>,
    /// Tracked command invocation and bot response message pairs.
    pub edit_tracker: MessageEditTracker,
}

impl ClientCache {
    /// Creates a new cache with the given configuration.
    #[must_use]
    pub fn new(config: CacheConfig) -> Self {
        Self {
            guilds: Cache::new(config.guilds),
            channels: Cache::new(config.channels),
            roles: Cache::new(config.roles),
            users: Cache::new(config.users),
            messages: Cache::new(config.messages),
            presences: Cache::new(config.presences),
            webhooks: Cache::new(config.webhooks),
            members: HashMap::new(),
            edit_tracker: MessageEditTracker::new(config.message_edits),
        }
    }

    /// Creates a cache with default capacity limits.
    #[must_use]
    pub fn default_config() -> Self {
        Self::new(CacheConfig::default())
    }

    /// Updates the cache from a gateway event.
    ///
    /// Returns `true` when the event caused at least one cache mutation.
    pub fn update(&mut self, event: &models::Event) -> bool {
        match event {
            // ---- Guilds ------------------------------------------------
            models::Event::GuildCreate(guild) => {
                self.cache_guild(guild.clone());
                true
            }
            models::Event::GuildUpdate(guild) => {
                self.cache_guild(guild.clone());
                true
            }
            models::Event::GuildDelete(unavailable) => {
                self.guilds.remove(unavailable.id.get());
                self.members.remove(&unavailable.id.get());
                true
            }

            // ---- Channels ----------------------------------------------
            models::Event::ChannelCreate(ch)
            | models::Event::ChannelUpdate(ch)
            | models::Event::ThreadCreate(ch)
            | models::Event::ThreadUpdate(ch) => {
                self.channels.insert(ch.id.get(), ch.clone());
                true
            }
            models::Event::ChannelDelete(ch) => {
                self.channels.remove(ch.id.get());
                true
            }
            models::Event::ThreadDelete(td) => {
                self.channels.remove(td.id.get());
                true
            }

            // ---- Members -----------------------------------------------
            models::Event::GuildMemberAdd(member) | models::Event::GuildMemberUpdate(member) => {
                let guild_id = member.guild_id.get();
                self.members
                    .entry(guild_id)
                    .or_default()
                    .insert(member.clone());
                self.users.insert(member.user.id.get(), member.user.clone());
                true
            }
            models::Event::GuildMemberRemove(remove) => {
                let guild_id = remove.guild_id.get();
                if let Some(mc) = self.members.get_mut(&guild_id) {
                    mc.remove(remove.user.id.get());
                }
                true
            }
            models::Event::GuildMembersChunk(chunk) => {
                let gc = self.members.entry(chunk.guild_id.get()).or_default();
                for member in &chunk.members {
                    gc.insert(member.clone());
                    self.users.insert(member.user.id.get(), member.user.clone());
                }
                true
            }

            // ---- Roles -------------------------------------------------
            models::Event::GuildRoleCreate(ev) | models::Event::GuildRoleUpdate(ev) => {
                self.roles.insert(ev.role.id.get(), ev.role.clone());
                if let Some(guild) = self.guilds.get_mut(ev.guild_id.get()) {
                    if let Some(existing) =
                        guild.roles.iter_mut().find(|role| role.id == ev.role.id)
                    {
                        *existing = ev.role.clone();
                    } else {
                        guild.roles.push(ev.role.clone());
                        guild
                            .roles
                            .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    }
                }
                true
            }
            models::Event::GuildRoleDelete(ev) => {
                self.roles.remove(ev.role_id.get());
                if let Some(guild) = self.guilds.get_mut(ev.guild_id.get()) {
                    guild.roles.retain(|role| role.id != ev.role_id);
                }
                true
            }

            // ---- Messages ----------------------------------------------
            models::Event::MessageCreate(msg) | models::Event::MessageUpdate(msg) => {
                self.messages.insert(msg.id.get(), msg.clone());
                true
            }
            models::Event::MessageDelete(del) => {
                self.messages.remove(del.id.get());
                self.edit_tracker.remove_user_message(del.id.get());
                self.edit_tracker.remove_response(del.id.get());
                true
            }
            models::Event::MessageDeleteBulk(bulk) => {
                for id in &bulk.ids {
                    self.messages.remove(id.get());
                    self.edit_tracker.remove_user_message(id.get());
                    self.edit_tracker.remove_response(id.get());
                }
                true
            }

            // ---- Presences ---------------------------------------------
            models::Event::PresenceUpdate(presence) => {
                self.presences
                    .insert(presence.user_id.get(), presence.clone());
                true
            }

            // ---- Webhooks ----------------------------------------------
            models::Event::WebhooksUpdate(update) => {
                let channel_id = update.channel_id.get();
                self.webhooks
                    .retain(|_, webhook| webhook.channel_id.map(|id| id.get()) != Some(channel_id));
                true
            }

            // ---- User update -------------------------------------------
            models::Event::UserUpdate(user) => {
                self.users.insert(user.id.get(), user.clone());
                true
            }

            // All other events do not mutate the cache.
            _ => false,
        }
    }

    /// Updates the cache from a gateway event, capturing previous message state if edited.
    ///
    /// Returns a tuple `(mutated, edit_info)` where `edit_info` is `Some((old_msg, new_msg))`
    /// if the event was a [`models::Event::MessageUpdate`].
    pub fn update_tracked(
        &mut self,
        event: &models::Event,
    ) -> (bool, Option<(Option<models::Message>, models::Message)>) {
        if let models::Event::MessageUpdate(msg) = event {
            let previous = self.messages.get(msg.id.get()).cloned();
            self.messages.insert(msg.id.get(), msg.clone());
            (true, Some((previous, msg.clone())))
        } else {
            (self.update(event), None)
        }
    }

    /// Looks up a member in the per-guild cache.
    #[must_use]
    pub fn member(&self, guild_id: u64, user_id: u64) -> Option<&models::Member> {
        self.members.get(&guild_id)?.get(user_id)
    }

    /// Returns all cached members for a guild.
    #[must_use]
    pub fn guild_members(&self, guild_id: u64) -> Option<&MemberCache> {
        self.members.get(&guild_id)
    }

    // ---- cross-reference helpers ----------------------------------------

    /// Looks up the guild that owns a channel (by scanning guild caches).
    ///
    /// This is an O(n) scan over guild IDs. For large bots prefer storing
    /// the mapping explicitly.
    #[must_use]
    pub fn guild_of_channel(&self, channel_id: u64) -> Option<&models::Guild> {
        // Try to find the channel in cache, then look up its guild.
        let ch = self.channels.get(channel_id)?;
        let guild_id = ch.guild_id.get();
        self.guilds.get(guild_id)
    }

    /// Returns the roles that a member currently holds, resolved from the cache.
    #[must_use]
    pub fn member_roles(&self, guild_id: u64, user_id: u64) -> Option<Vec<&models::Role>> {
        let member = self.member(guild_id, user_id)?;
        Some(
            member
                .role_ids()
                .filter_map(|rid| self.roles.get(rid.get()))
                .collect(),
        )
    }

    /// Resolves the permissions a member has in a guild channel.
    ///
    /// Returns `None` when the guild, channel, or member is not cached. The
    /// guild owner and administrators receive every permission.
    #[must_use]
    pub fn permissions_for(
        &self,
        guild_id: u64,
        user_id: u64,
        channel_id: u64,
    ) -> Option<Permissions> {
        let guild = self.guilds.get(guild_id)?;
        let channel = self.channels.get(channel_id)?;
        let member = self.member(guild_id, user_id).or_else(|| {
            guild
                .members()
                .iter()
                .find(|member| member.user.id.get() == user_id)
        })?;
        Some(channel.permissions_for(guild, member))
    }

    /// Returns the presence for a user, if cached.
    #[must_use]
    pub fn presence(&self, user_id: u64) -> Option<&models::PresenceUpdate> {
        self.presences.get(user_id)
    }

    // ---- resource lookup helpers ----------------------------------------

    /// Returns a cached guild.
    #[must_use]
    pub fn guild(&self, guild_id: u64) -> Option<&models::Guild> {
        self.guilds.get(guild_id)
    }

    /// Returns a cached channel, whether guild channel or thread.
    #[must_use]
    pub fn channel(&self, channel_id: u64) -> Option<&models::GuildChannel> {
        self.channels.get(channel_id)
    }

    /// Returns a cached user.
    #[must_use]
    pub fn user(&self, user_id: u64) -> Option<&models::User> {
        self.users.get(user_id)
    }

    /// Returns a cached role.
    #[must_use]
    pub fn role(&self, role_id: u64) -> Option<&models::Role> {
        self.roles.get(role_id)
    }

    /// Returns a cached message.
    #[must_use]
    pub fn message(&self, message_id: u64) -> Option<&models::Message> {
        self.messages.get(message_id)
    }

    /// Returns a cached webhook.
    #[must_use]
    pub fn webhook(&self, webhook_id: u64) -> Option<&models::Webhook> {
        self.webhooks.get(webhook_id)
    }

    /// Returns a cached guild channel, excluding threads.
    ///
    /// Threads are cached alongside guild channels; use [`ClientCache::channel`]
    /// when either is acceptable.
    #[must_use]
    pub fn guild_channel(&self, channel_id: u64) -> Option<&models::GuildChannel> {
        self.channels
            .get(channel_id)
            .filter(|channel| !channel.is_thread())
    }

    /// Returns a cached member, checking the per-guild cache before the guild
    /// snapshot.
    #[must_use]
    pub fn guild_member(&self, guild_id: u64, user_id: u64) -> Option<&models::Member> {
        self.member(guild_id, user_id).or_else(|| {
            self.guilds
                .get(guild_id)?
                .members()
                .iter()
                .find(|member| member.user.id.get() == user_id)
        })
    }

    /// Finds a channel in a guild by its exact name.
    ///
    /// Names are matched case-sensitively, matching Discord's uniqueness rules
    /// within a guild.
    #[must_use]
    pub fn channel_by_name(&self, guild_id: u64, name: &str) -> Option<&models::GuildChannel> {
        self.channels
            .values()
            .find(|channel| channel.guild_id.get() == guild_id && channel.name == name)
    }

    /// Finds a member in a guild by username or display name.
    ///
    /// The username and global display name are compared case-insensitively.
    /// Guild nicknames are compared as well but do not take precedence when
    /// several members match.
    #[must_use]
    pub fn member_by_name(&self, guild_id: u64, name: &str) -> Option<&models::Member> {
        self.members.get(&guild_id)?.values().find(|member| {
            member.name().eq_ignore_ascii_case(name)
                || member.display_name().eq_ignore_ascii_case(name)
                || member
                    .user
                    .global_name
                    .as_deref()
                    .is_some_and(|global| global.eq_ignore_ascii_case(name))
        })
    }

    // ---- webhook helpers -------------------------------------------------

    /// Stores a webhook in the cache.
    pub fn cache_webhook(&mut self, webhook: models::Webhook) {
        self.webhooks.insert(webhook.id.get(), webhook);
    }

    /// Stores several webhooks in the cache.
    pub fn cache_webhooks<I>(&mut self, webhooks: I)
    where
        I: IntoIterator<Item = models::Webhook>,
    {
        for webhook in webhooks {
            self.cache_webhook(webhook);
        }
    }

    /// Removes a webhook from the cache.
    pub fn remove_webhook(&mut self, webhook_id: u64) -> Option<models::Webhook> {
        self.webhooks.remove(webhook_id)
    }

    /// Returns every cached webhook that posts to a channel.
    pub fn webhooks_for_channel(&self, channel_id: u64) -> impl Iterator<Item = &models::Webhook> {
        self.webhooks
            .values()
            .filter(move |webhook| webhook.channel_id.map(|id| id.get()) == Some(channel_id))
    }

    /// Returns every cached webhook that belongs to a guild.
    pub fn webhooks_for_guild(&self, guild_id: u64) -> impl Iterator<Item = &models::Webhook> {
        self.webhooks
            .values()
            .filter(move |webhook| webhook.guild_id.map(|id| id.get()) == Some(guild_id))
    }

    // ---- private helpers -----------------------------------------------

    fn cache_guild(&mut self, guild: models::Guild) {
        // Seed per-guild caches from the GUILD_CREATE payload.
        for ch in guild.channels.iter().chain(guild.threads()) {
            self.channels.insert(ch.id.get(), ch.clone());
        }
        for role in &guild.roles {
            self.roles.insert(role.id.get(), role.clone());
        }
        let mc = self.members.entry(guild.id.get()).or_default();
        for member in &guild.members {
            mc.insert(member.clone());
            self.users.insert(member.user.id.get(), member.user.clone());
        }
        self.guilds.insert(guild.id.get(), guild);
    }
}

/// A shared, read-only view of a [`ClientCache`] for command contexts.
///
/// The bot runtime attaches this wrapper to every command context so handlers
/// can look up guilds, channels, members and messages without locking the cache
/// themselves.
#[derive(Clone, Debug)]
pub struct SharedCache {
    inner: std::sync::Arc<std::sync::Mutex<ClientCache>>,
}

impl SharedCache {
    /// Wraps a shared cache handle.
    #[must_use]
    pub const fn new(inner: std::sync::Arc<std::sync::Mutex<ClientCache>>) -> Self {
        Self { inner }
    }

    fn with<R>(&self, f: impl FnOnce(&ClientCache) -> R) -> R {
        let cache = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&cache)
    }
}

impl commands::ContextCache for SharedCache {
    fn guild(&self, guild_id: u64) -> Option<models::Guild> {
        self.with(|cache| cache.guild(guild_id).cloned())
    }

    fn channel(&self, channel_id: u64) -> Option<models::GuildChannel> {
        self.with(|cache| cache.channel(channel_id).cloned())
    }

    fn user(&self, user_id: u64) -> Option<models::User> {
        self.with(|cache| cache.users.get(user_id).cloned())
    }

    fn member(&self, guild_id: u64, user_id: u64) -> Option<models::Member> {
        self.with(|cache| cache.member(guild_id, user_id).cloned())
    }

    fn message(&self, message_id: u64) -> Option<models::Message> {
        self.with(|cache| cache.messages.get(message_id).cloned())
    }

    fn permissions_for(&self, guild_id: u64, user_id: u64, channel_id: u64) -> Option<Permissions> {
        self.with(|cache| cache.permissions_for(guild_id, user_id, channel_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cache() -> ClientCache {
        ClientCache::new(CacheConfig {
            guilds: 10,
            channels: 100,
            roles: 100,
            users: 100,
            messages: 50,
            presences: 100,
            webhooks: 50,
            message_edits: 50,
        })
    }

    #[test]
    fn cache_insert_get_remove() {
        let mut c: Cache<String> = Cache::new(3);
        c.insert(1, "a".into());
        c.insert(2, "b".into());
        assert_eq!(c.get(1), Some(&"a".to_owned()));
        assert_eq!(c.len(), 2);
        c.remove(1);
        assert_eq!(c.get(1), None);
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn cache_capacity_evicts_on_overflow() {
        let mut c: Cache<u32> = Cache::new(2);
        c.insert(1, 1);
        c.insert(2, 2);
        c.insert(3, 3); // should evict one of 1 or 2
        assert_eq!(c.len(), 2);
        assert!(c.get(3).is_some());
    }

    #[test]
    fn cache_zero_capacity_accepts_nothing() {
        let mut c: Cache<u32> = Cache::new(0);
        c.insert(1, 1);
        assert!(c.is_empty());
    }

    #[test]
    fn cache_invalidate_removes_entries() {
        let mut c: Cache<u32> = Cache::new(10);
        c.insert(1, 1);
        c.insert(2, 2);
        c.insert(3, 3);
        c.invalidate([1_u64, 3]);
        assert_eq!(c.len(), 1);
        assert!(c.get(2).is_some());
        assert!(c.get(1).is_none());
    }

    #[test]
    fn cache_retain_keeps_matching_entries() {
        let mut c: Cache<u32> = Cache::new(10);
        c.insert(1, 10);
        c.insert(2, 20);
        c.insert(3, 30);
        c.retain(|id, value| id % 2 == 1 && *value >= 30);
        assert_eq!(c.len(), 1);
        assert_eq!(c.get(3), Some(&30));
    }

    // ---- ClientCache event-driven tests --------------------------------

    // Build minimal model objects from JSON strings using the models parser.
    fn value(raw: &str) -> data::Value {
        data::json::from_str(raw).unwrap()
    }

    fn guild_event(name: &str, raw: &str) -> models::Event {
        models::Event::from_value(name, &value(raw)).unwrap()
    }

    /// A minimal GUILD_CREATE JSON (no members/channels/roles needed for unit
    /// testing the guild-level cache paths).
    fn minimal_guild(id: u64, name: &str) -> String {
        format!(
            r#"{{
                "id": "{id}",
                "name": "{name}",
                "icon": null,
                "splash": null,
                "discovery_splash": null,
                "owner_id": "1",
                "afk_timeout": 300,
                "verification_level": 0,
                "default_message_notifications": 0,
                "explicit_content_filter": 0,
                "roles": [],
                "emojis": [],
                "features": [],
                "mfa_level": 0,
                "system_channel_flags": 0,
                "premium_tier": 0,
                "nsfw_level": 0,
                "premium_progress_bar_enabled": false,
                "safety_alerts_channel_id": null,
                "members": [],
                "channels": [],
                "presences": [],
                "unavailable": false
            }}"#
        )
    }

    #[test]
    fn guild_lifecycle_populates_and_evicts_cache() {
        let mut cache = make_cache();

        // GUILD_CREATE populates
        let ev = guild_event("GUILD_CREATE", &minimal_guild(10, "Test Guild"));
        assert!(cache.update(&ev));
        assert!(cache.guilds.get(10).is_some());

        // GUILD_UPDATE replaces
        let ev = guild_event("GUILD_UPDATE", &minimal_guild(10, "Updated Guild"));
        assert!(cache.update(&ev));
        assert_eq!(cache.guilds.get(10).unwrap().name, "Updated Guild");

        // GUILD_DELETE evicts
        let del_raw = r#"{"id":"10","unavailable":true}"#;
        let ev = guild_event("GUILD_DELETE", del_raw);
        assert!(cache.update(&ev));
        assert!(cache.guilds.get(10).is_none());
    }

    #[test]
    fn message_lifecycle_populates_and_evicts_cache() {
        let mut cache = make_cache();

        let msg_raw = r#"{
            "id": "99",
            "channel_id": "5",
            "author": {"id": "1", "username": "u", "discriminator": "0", "global_name": null},
            "content": "hello",
            "timestamp": "2024-01-01T00:00:00Z",
            "edited_timestamp": null,
            "tts": false,
            "mention_everyone": false,
            "mentions": [],
            "mention_roles": [],
            "attachments": [],
            "embeds": [],
            "pinned": false,
            "type": 0
        }"#;

        // MESSAGE_CREATE
        let ev = guild_event("MESSAGE_CREATE", msg_raw);
        assert!(cache.update(&ev));
        assert!(cache.messages.get(99).is_some());

        // MESSAGE_DELETE
        let del_raw = r#"{"id":"99","channel_id":"5"}"#;
        let ev = guild_event("MESSAGE_DELETE", del_raw);
        assert!(cache.update(&ev));
        assert!(cache.messages.get(99).is_none());
    }

    #[test]
    fn unrecognised_events_do_not_mutate_cache() {
        let mut cache = make_cache();
        let ev = models::Event::Resumed;
        assert!(!cache.update(&ev));
        assert!(cache.guilds.is_empty());
    }

    #[test]
    fn message_edit_tracker_registers_and_evicts() {
        let mut tracker = MessageEditTracker::new(2);
        tracker.register_response(101, 201);
        tracker.register_response(102, 202);

        assert_eq!(tracker.get_response(101), Some(201));
        assert_eq!(tracker.get_user_message(201), Some(101));
        assert_eq!(tracker.get_response(102), Some(202));

        // Overflow evicts oldest
        tracker.register_response(103, 203);
        assert_eq!(tracker.len(), 2);
        assert_eq!(tracker.get_response(103), Some(203));

        // Removal
        assert_eq!(tracker.remove_user_message(103), Some(203));
        assert_eq!(tracker.get_response(103), None);
        assert_eq!(tracker.get_user_message(203), None);
    }

    #[test]
    fn message_edit_tracked_update_captures_history() {
        let mut cache = make_cache();
        let msg_raw = r#"{
            "id": "123",
            "channel_id": "1",
            "author": {"id": "1", "username": "u", "discriminator": "0", "global_name": null},
            "content": "original",
            "timestamp": "2024-01-01T00:00:00Z",
            "edited_timestamp": null,
            "tts": false,
            "mention_everyone": false,
            "mentions": [],
            "mention_roles": [],
            "attachments": [],
            "embeds": [],
            "pinned": false,
            "type": 0
        }"#;

        let create_ev = guild_event("MESSAGE_CREATE", msg_raw);
        cache.update(&create_ev);
        cache.edit_tracker.register_response(123, 999);

        let edit_raw = r#"{
            "id": "123",
            "channel_id": "1",
            "author": {"id": "1", "username": "u", "discriminator": "0", "global_name": null},
            "content": "edited",
            "timestamp": "2024-01-01T00:00:00Z",
            "edited_timestamp": "2024-01-01T00:01:00Z",
            "tts": false,
            "mention_everyone": false,
            "mentions": [],
            "mention_roles": [],
            "attachments": [],
            "embeds": [],
            "pinned": false,
            "type": 0
        }"#;

        let update_ev = guild_event("MESSAGE_UPDATE", edit_raw);
        let (mutated, edit_info) = cache.update_tracked(&update_ev);
        assert!(mutated);
        let (prev, updated) = edit_info.expect("must capture edit");
        assert_eq!(prev.as_ref().map(|m| m.content.as_str()), Some("original"));
        assert_eq!(updated.content, "edited");
        assert_eq!(cache.edit_tracker.get_response(123), Some(999));

        // When message deleted, tracking is cleaned up
        let del_raw = r#"{"id":"123","channel_id":"1"}"#;
        let del_ev = guild_event("MESSAGE_DELETE", del_raw);
        let (mutated, _) = cache.update_tracked(&del_ev);
        assert!(mutated);
        assert_eq!(cache.edit_tracker.get_response(123), None);
    }

    #[test]
    fn permissions_for_resolves_member_roles() {
        let guild_id = 10_u64;
        let user_id = 42_u64;
        let channel_id = 7_u64;

        let everyone = models::Role::new(guild_id, guild_id, "@everyone")
            .with_permissions(Permissions::VIEW_CHANNEL);
        let member_role = models::Role::new(guild_id, 20_u64, "member")
            .with_permissions(Permissions::SEND_MESSAGES);

        let user = models::User {
            id: user_id.into(),
            ..models::User::default()
        };
        let member = models::Member::new(user, guild_id).with_roles(vec![member_role.clone()]);
        let channel =
            models::GuildChannel::new(channel_id, guild_id, "general", enums::ChannelType::Text);

        let mut guild = models::Guild::new(guild_id, "Guild");
        guild.roles = vec![everyone, member_role.clone()];
        guild.members = vec![member];
        guild.channels = vec![channel];

        let mut cache = make_cache();
        assert!(cache.update(&models::Event::GuildCreate(guild)));

        let permissions = cache
            .permissions_for(guild_id, user_id, channel_id)
            .expect("cached guild, channel and member");
        assert!(permissions.contains(Permissions::VIEW_CHANNEL));
        assert!(permissions.contains(Permissions::SEND_MESSAGES));
        assert!(cache.permissions_for(guild_id, 99, channel_id).is_none());

        let updated_role = member_role.with_permissions(Permissions::ADMINISTRATOR);
        assert!(
            cache.update(&models::Event::GuildRoleUpdate(models::RoleEvent {
                guild_id: guild_id.into(),
                role: updated_role,
            }))
        );
        let permissions = cache
            .permissions_for(guild_id, user_id, channel_id)
            .expect("updated permissions");
        assert!(permissions.contains(Permissions::ADMINISTRATOR));
    }

    #[test]
    fn lookup_helpers_and_webhook_invalidation() {
        let guild_id = 10_u64;
        let channel_id = 7_u64;
        let user_id = 42_u64;

        let user = models::User::new(user_id, "Alice");
        let member = models::Member::new(user, guild_id);
        let channel =
            models::GuildChannel::new(channel_id, guild_id, "general", enums::ChannelType::Text);
        let guild = models::Guild::new(guild_id, "Guild")
            .with_members(vec![member])
            .with_channels(vec![channel]);

        let mut cache = make_cache();
        assert!(cache.update(&models::Event::GuildCreate(guild)));

        assert_eq!(
            cache.guild(guild_id).map(|guild| guild.name.as_str()),
            Some("Guild")
        );
        assert_eq!(
            cache
                .channel(channel_id)
                .map(|channel| channel.name.as_str()),
            Some("general")
        );
        assert!(cache.guild_channel(channel_id).is_some());
        assert_eq!(
            cache.user(user_id).map(|user| user.name.as_str()),
            Some("Alice")
        );
        assert_eq!(
            cache
                .channel_by_name(guild_id, "general")
                .map(|channel| channel.id.get()),
            Some(channel_id)
        );
        assert!(cache.channel_by_name(guild_id, "missing").is_none());
        assert_eq!(
            cache
                .guild_member(guild_id, user_id)
                .map(|member| member.name()),
            Some("Alice")
        );
        assert_eq!(
            cache
                .member_by_name(guild_id, "alice")
                .map(|member| member.user.id.get()),
            Some(user_id)
        );
        assert!(cache.member_by_name(guild_id, "bob").is_none());

        let mut webhook = models::Webhook::new(99_u64);
        webhook.guild_id = Some(guild_id.into());
        webhook.channel_id = Some(channel_id.into());
        cache.cache_webhook(webhook);

        let other = models::Webhook::new(100_u64);
        cache.cache_webhooks([other]);

        assert!(cache.webhook(99).is_some());
        assert_eq!(cache.webhooks_for_channel(channel_id).count(), 1);
        assert_eq!(cache.webhooks_for_guild(guild_id).count(), 1);

        let event = guild_event("WEBHOOKS_UPDATE", r#"{"guild_id":"10","channel_id":"7"}"#);
        assert!(cache.update(&event));
        assert!(cache.webhook(99).is_none());
        assert!(cache.webhook(100).is_some());
        assert_eq!(
            cache.remove_webhook(100).map(|webhook| webhook.id.get()),
            Some(100)
        );
        assert!(cache.webhooks.is_empty());
    }
}
