//! Infrastructure for command cooldowns, modelled on poise's `cooldown` module.
//!
//! Each command carries a [`CooldownTracker`]. The dispatcher asks it whether
//! the command may run, and starts the clocks only after the command's
//! arguments parsed, so a mistyped invocation does not burn a use.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::Jid;

/// The addressing a cooldown decision is made with.
///
/// This is poise's `CooldownContext`, with WhatsApp JIDs in place of Discord
/// snowflakes: the user is the sender, the channel is the chat, and the guild
/// is the group when there is one.
#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct CooldownContext {
    /// The sender associated with this request.
    pub user: Jid,
    /// The group this request originated from, or `None` in a DM.
    pub guild: Option<Jid>,
    /// The chat associated with this request.
    pub channel: Jid,
}

/// How long each kind of cooldown lasts.
///
/// A `None` field is not enforced. This is poise's `CooldownConfig`.
#[derive(Default, Clone, PartialEq, Eq, Debug, Hash)]
pub struct CooldownConfig {
    /// This cooldown operates on a global basis.
    pub global: Option<Duration>,
    /// This cooldown operates on a per-user basis.
    pub user: Option<Duration>,
    /// This cooldown operates on a per-group basis.
    pub guild: Option<Duration>,
    /// This cooldown operates on a per-chat basis.
    pub channel: Option<Duration>,
    /// This cooldown operates on a per-member basis (sender and group).
    pub member: Option<Duration>,
}

impl CooldownConfig {
    /// Whether any duration is set, so the dispatcher has work to do.
    pub fn is_empty(&self) -> bool {
        self.global.is_none()
            && self.user.is_none()
            && self.guild.is_none()
            && self.channel.is_none()
            && self.member.is_none()
    }
}

/// Possible types of command cooldowns, for [`CooldownTracker::set_last_invocation`].
#[non_exhaustive]
pub enum CooldownType {
    /// A global cooldown that applies to all users and chats.
    Global,
    /// A cooldown specific to one sender.
    User(Jid),
    /// A cooldown that applies to an entire group.
    Guild(Jid),
    /// A cooldown specific to one chat.
    Channel(Jid),
    /// A cooldown specific to one member of a group.
    Member((Jid, Jid)),
}

/// Tracks all types of cooldowns for a single command.
///
/// You probably don't need to use this directly. `#[command]` generates a
/// tracker, and the dispatcher consults it.
#[derive(Default, Clone, Debug)]
pub struct CooldownTracker {
    global_invocation: Option<Instant>,
    user_invocations: HashMap<Jid, Instant>,
    guild_invocations: HashMap<Jid, Instant>,
    channel_invocations: HashMap<Jid, Instant>,
    member_invocations: HashMap<(Jid, Jid), Instant>,
}

/// **Renamed to [`CooldownTracker`]**, matching poise's alias.
pub type Cooldowns = CooldownTracker;

/// Drops entries whose cooldown has already elapsed.
///
/// `start_cooldown` calls this before inserting, so a map only ever holds
/// invocations that can still block someone.
fn prune<K>(entries: &mut HashMap<K, Instant>, duration: Duration, now: Instant) {
    entries.retain(|_, instant| now.saturating_duration_since(*instant) < duration);
}

impl CooldownTracker {
    /// Create a new cooldown tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// The longest remaining cooldown across every bucket that applies, if any
    /// has not yet expired.
    pub fn remaining_cooldown(
        &self,
        ctx: CooldownContext,
        cooldown_durations: &CooldownConfig,
    ) -> Option<Duration> {
        // Fold the buckets without allocating: this runs on every command that
        // configures a cooldown, and the five buckets are known up front.
        let now = Instant::now();
        let mut longest: Option<Duration> = None;
        let mut consider = |cooldown: Option<Duration>, last: Option<Instant>| {
            if let Some(left) = cooldown
                .and_then(|duration| duration.checked_sub(now.saturating_duration_since(last?)))
            {
                longest = Some(longest.map_or(left, |current| current.max(left)));
            }
        };

        consider(cooldown_durations.global, self.global_invocation);
        consider(
            cooldown_durations.user,
            self.user_invocations.get(&ctx.user).copied(),
        );
        consider(
            cooldown_durations.channel,
            self.channel_invocations.get(&ctx.channel).copied(),
        );
        if let Some(guild) = ctx.guild {
            consider(
                cooldown_durations.guild,
                self.guild_invocations.get(&guild).copied(),
            );
            consider(
                cooldown_durations.member,
                self.member_invocations.get(&(ctx.user, guild)).copied(),
            );
        }

        longest
    }

    /// Indicates that a command has been executed and all associated cooldowns
    /// should start running.
    ///
    /// Only the buckets `cooldown_durations` configures are recorded, so a
    /// command with a user cooldown does not also grow the channel and guild
    /// maps. Expired entries are pruned at the same time, so a long-lived bot's
    /// maps hold only invocations still inside their window.
    pub fn start_cooldown(&mut self, ctx: CooldownContext, cooldown_durations: &CooldownConfig) {
        let now = Instant::now();

        if cooldown_durations.global.is_some() {
            self.global_invocation = Some(now);
        }
        if let Some(duration) = cooldown_durations.user {
            prune(&mut self.user_invocations, duration, now);
            self.user_invocations.insert(ctx.user.clone(), now);
        }
        if let Some(duration) = cooldown_durations.channel {
            prune(&mut self.channel_invocations, duration, now);
            self.channel_invocations.insert(ctx.channel, now);
        }
        let guild = ctx.guild;
        if let (Some(duration), Some(guild)) = (cooldown_durations.guild, guild.clone()) {
            prune(&mut self.guild_invocations, duration, now);
            self.guild_invocations.insert(guild, now);
        }
        if let (Some(duration), Some(guild)) = (cooldown_durations.member, guild) {
            prune(&mut self.member_invocations, duration, now);
            self.member_invocations.insert((ctx.user, guild), now);
        }
    }

    /// Sets the last invocation for the specified cooldown bucket.
    ///
    /// This is not needed for regular usage. It exists so a command can shorten
    /// or lengthen a cooldown after it has run.
    pub fn set_last_invocation(&mut self, cooldown_type: CooldownType, instant: Instant) {
        match cooldown_type {
            CooldownType::Global => self.global_invocation = Some(instant),
            CooldownType::User(user) => {
                self.user_invocations.insert(user, instant);
            }
            CooldownType::Guild(guild) => {
                self.guild_invocations.insert(guild, instant);
            }
            CooldownType::Channel(channel) => {
                self.channel_invocations.insert(channel, instant);
            }
            CooldownType::Member(member) => {
                self.member_invocations.insert(member, instant);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(user: &str, chat: &str, group: Option<&str>) -> CooldownContext {
        CooldownContext {
            user: user.parse().unwrap(),
            channel: chat.parse().unwrap(),
            guild: group.map(|value| value.parse().unwrap()),
        }
    }

    #[test]
    fn a_user_cooldown_blocks_the_same_sender() {
        let mut tracker = CooldownTracker::new();
        let config = CooldownConfig {
            user: Some(Duration::from_secs(10)),
            ..CooldownConfig::default()
        };
        let alice = ctx("1@s.whatsapp.net", "1@s.whatsapp.net", None);
        let bob = ctx("2@s.whatsapp.net", "2@s.whatsapp.net", None);

        assert_eq!(tracker.remaining_cooldown(alice.clone(), &config), None);
        tracker.start_cooldown(alice.clone(), &config);
        assert!(tracker.remaining_cooldown(alice, &config).unwrap() > Duration::from_secs(0));
        assert_eq!(tracker.remaining_cooldown(bob, &config), None);
    }

    #[test]
    fn a_member_cooldown_is_per_group() {
        let mut tracker = CooldownTracker::new();
        let config = CooldownConfig {
            member: Some(Duration::from_secs(10)),
            ..CooldownConfig::default()
        };
        let in_a = ctx("1@s.whatsapp.net", "g1@g.us", Some("g1@g.us"));
        let in_b = ctx("1@s.whatsapp.net", "g2@g.us", Some("g2@g.us"));

        tracker.start_cooldown(in_a.clone(), &config);
        assert!(tracker.remaining_cooldown(in_a, &config).is_some());
        assert_eq!(tracker.remaining_cooldown(in_b, &config), None);
    }

    #[test]
    fn only_configured_buckets_are_recorded() {
        let mut tracker = CooldownTracker::new();
        let config = CooldownConfig {
            user: Some(Duration::from_secs(10)),
            ..CooldownConfig::default()
        };
        let in_group = ctx("1@s.whatsapp.net", "g1@g.us", Some("g1@g.us"));

        tracker.start_cooldown(in_group, &config);

        // The user bucket is live, but the unconfigured channel/guild/member
        // buckets stayed empty rather than being filled with a useless instant.
        assert!(
            tracker
                .remaining_cooldown(ctx("1@s.whatsapp.net", "g1@g.us", Some("g1@g.us")), &config)
                .is_some()
        );
        assert!(
            tracker
                .user_invocations
                .contains_key(&"1@s.whatsapp.net".parse().unwrap())
        );
        assert!(tracker.channel_invocations.is_empty());
        assert!(tracker.guild_invocations.is_empty());
        assert!(tracker.member_invocations.is_empty());
    }

    #[test]
    fn starting_a_cooldown_prunes_expired_entries() {
        let mut tracker = CooldownTracker::new();
        let config = CooldownConfig {
            user: Some(Duration::from_secs(10)),
            ..CooldownConfig::default()
        };

        // A stale entry from long ago, and a fresh one that will replace it.
        tracker.user_invocations.insert(
            "9@s.whatsapp.net".parse().unwrap(),
            Instant::now() - Duration::from_secs(60),
        );
        tracker.start_cooldown(ctx("1@s.whatsapp.net", "1@s.whatsapp.net", None), &config);

        assert!(
            !tracker
                .user_invocations
                .contains_key(&"9@s.whatsapp.net".parse().unwrap()),
            "an entry past its window is pruned"
        );
    }
}
