//! Allowed-mention rules for message creation.
//!
//! Discord parses `@everyone`, role and user mentions from message content by
//! default. [`AllowedMentions`] controls which of those mentions are parsed and
//! can add mentions for specific users or roles.

use abc::Snowflake;
use data::{Object, Value};

/// Which mentions a message is allowed to trigger.
///
/// The default parses every mention type and pings the replied-to user. Use
/// [`AllowedMentions::none`] to suppress all pings, then opt individual kinds
/// back in with the builder methods.
///
/// Parsing and explicit mentions are independent. [`AllowedMentions::with_users`]
/// mentions specific users without allowing user mentions written in the
/// content; use [`AllowedMentions::with_user_mentions`] for the latter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowedMentions {
    /// Whether `@everyone` and `@here` are parsed from the content.
    pub everyone: bool,
    /// Whether user mentions in the content are parsed.
    pub parse_users: bool,
    /// Whether role mentions in the content are parsed.
    pub parse_roles: bool,
    /// Users that should be mentioned even when they are not in the content.
    pub users: Vec<Snowflake>,
    /// Roles that should be mentioned even when they are not in the content.
    pub roles: Vec<Snowflake>,
    /// Whether replying pings the author of the replied-to message.
    pub replied_user: bool,
}

impl AllowedMentions {
    /// Creates a rule set that allows no mentions.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            everyone: false,
            parse_users: false,
            parse_roles: false,
            users: Vec::new(),
            roles: Vec::new(),
            replied_user: false,
        }
    }

    /// Creates a rule set that parses every mention type.
    #[must_use]
    pub const fn all() -> Self {
        Self {
            everyone: true,
            parse_users: true,
            parse_roles: true,
            users: Vec::new(),
            roles: Vec::new(),
            replied_user: true,
        }
    }

    /// Enables or disables `@everyone` and `@here`.
    #[must_use]
    pub const fn with_everyone(mut self, everyone: bool) -> Self {
        self.everyone = everyone;
        self
    }

    /// Enables or disables parsing user mentions from the content.
    #[must_use]
    pub const fn with_user_mentions(mut self, parse_users: bool) -> Self {
        self.parse_users = parse_users;
        self
    }

    /// Enables or disables parsing role mentions from the content.
    #[must_use]
    pub const fn with_role_mentions(mut self, parse_roles: bool) -> Self {
        self.parse_roles = parse_roles;
        self
    }

    /// Replaces the explicitly mentioned users.
    ///
    /// The listed users are mentioned even when they do not appear in the
    /// content. This does not change which mentions are parsed from the content.
    #[must_use]
    pub fn with_users<I, S>(mut self, users: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<Snowflake>,
    {
        self.users = users.into_iter().map(Into::into).collect();
        self
    }

    /// Replaces the explicitly mentioned roles.
    ///
    /// The listed roles are mentioned even when they do not appear in the
    /// content. This does not change which mentions are parsed from the content.
    #[must_use]
    pub fn with_roles<I, S>(mut self, roles: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<Snowflake>,
    {
        self.roles = roles.into_iter().map(Into::into).collect();
        self
    }

    /// Enables or disables pinging the replied-to user.
    #[must_use]
    pub const fn with_replied_user(mut self, replied_user: bool) -> Self {
        self.replied_user = replied_user;
        self
    }

    /// Adds one explicitly mentioned user.
    #[must_use]
    pub fn add_user(mut self, user_id: impl Into<Snowflake>) -> Self {
        self.users.push(user_id.into());
        self
    }

    /// Adds one explicitly mentioned role.
    #[must_use]
    pub fn add_role(mut self, role_id: impl Into<Snowflake>) -> Self {
        self.roles.push(role_id.into());
        self
    }

    /// Returns the mention types Discord should parse from the content.
    #[must_use]
    pub fn parse(&self) -> Vec<&'static str> {
        let mut parse = Vec::new();
        if self.everyone {
            parse.push("everyone");
        }
        if self.parse_users {
            parse.push("users");
        }
        if self.parse_roles {
            parse.push("roles");
        }
        parse
    }

    /// Encodes the rule set as an `allowed_mentions` object.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let parse = self
            .parse()
            .into_iter()
            .map(Value::from)
            .collect::<Vec<_>>();

        let mut object = Object::new();
        object.insert("parse", Value::from(parse));
        if !self.users.is_empty() {
            object.insert(
                "users",
                Value::from(
                    self.users
                        .iter()
                        .map(|id| Value::from(id.get().to_string()))
                        .collect::<Vec<_>>(),
                ),
            );
        }
        if !self.roles.is_empty() {
            object.insert(
                "roles",
                Value::from(
                    self.roles
                        .iter()
                        .map(|id| Value::from(id.get().to_string()))
                        .collect::<Vec<_>>(),
                ),
            );
        }
        object.insert("replied_user", Value::from(self.replied_user));
        Value::from(object)
    }

    /// Returns `true` when the rule set allows no mentions at all.
    #[must_use]
    pub fn is_silent(&self) -> bool {
        !self.everyone
            && !self.parse_users
            && !self.parse_roles
            && self.users.is_empty()
            && self.roles.is_empty()
            && !self.replied_user
    }
}

impl Default for AllowedMentions {
    fn default() -> Self {
        Self::all()
    }
}

impl From<&AllowedMentions> for Value {
    fn from(value: &AllowedMentions) -> Self {
        value.to_value()
    }
}

impl From<AllowedMentions> for Value {
    fn from(value: AllowedMentions) -> Self {
        value.to_value()
    }
}

#[cfg(test)]
mod tests {
    use super::AllowedMentions;

    #[test]
    fn defaults_allow_everything() {
        let mentions = AllowedMentions::default();
        assert!(mentions.everyone);
        assert!(mentions.parse_users);
        assert!(mentions.parse_roles);
        assert!(mentions.replied_user);
        assert_eq!(mentions.parse(), vec!["everyone", "users", "roles"]);
        assert!(!mentions.is_silent());
    }

    #[test]
    fn none_suppresses_every_mention() {
        let mentions = AllowedMentions::none();
        assert!(mentions.is_silent());
        assert!(mentions.parse().is_empty());

        let value = mentions.to_value();
        let object = value.as_object().expect("object");
        let parse = object.get("parse").and_then(|value| value.as_array());
        assert_eq!(parse.map(<[_]>::len), Some(0));
        assert!(object.get("users").is_none());
        assert!(object.get("roles").is_none());
        assert_eq!(
            object.get("replied_user").and_then(|value| value.as_bool()),
            Some(false)
        );
    }

    #[test]
    fn explicit_users_and_roles_are_encoded_without_parsing() {
        let mentions = AllowedMentions::none()
            .add_user(10_u64)
            .add_role(20_u64)
            .with_replied_user(true);

        assert!(mentions.parse().is_empty());
        let value = mentions.to_value();
        let object = value.as_object().expect("object");
        let users = object
            .get("users")
            .and_then(|value| value.as_array())
            .unwrap();
        let roles = object
            .get("roles")
            .and_then(|value| value.as_array())
            .unwrap();
        assert_eq!(users.len(), 1);
        assert_eq!(roles.len(), 1);
        assert_eq!(users[0].as_str(), Some("10"));
        assert_eq!(roles[0].as_str(), Some("20"));
        assert_eq!(
            object.get("replied_user").and_then(|value| value.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn parse_flags_are_independent_of_explicit_lists() {
        let mentions = AllowedMentions::none()
            .with_users([10_u64, 11])
            .with_roles([20_u64])
            .with_user_mentions(true);

        assert_eq!(mentions.parse(), vec!["users"]);
        assert_eq!(mentions.users.len(), 2);
        assert_eq!(mentions.roles.len(), 1);
        assert!(!mentions.is_silent());

        let value = mentions.to_value();
        let object = value.as_object().expect("object");
        assert_eq!(
            object
                .get("parse")
                .and_then(|value| value.as_array())
                .map(<[_]>::len),
            Some(1)
        );
    }
}
