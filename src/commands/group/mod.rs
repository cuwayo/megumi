//! `!group`: WhatsApp group administration, one subcommand per setting.
//!
//! The parent declares `guild_only` and `permission = GroupAdmin` once; the
//! framework inherits both onto every subcommand, so the children carry only
//! their name and description. The parent body never runs:
//! `subcommand_required` turns a bare `!group` into an error before dispatch.

use megumi::{ChoiceParameter, Error, Jid, MessageExt, Permission, command, participant_matches};

use crate::Context;
use whatsapp_rust::prelude::wa;
use whatsapp_rust::{
    GroupDescription, GroupEphemeralSettings, GroupMetadata, GroupSubject, MemberAddMode,
    MemberLinkMode, MembershipApprovalMode, ParticipantChangeResponse, PreviousDescription,
};

/// Manages this group: its name, description, settings, and members.
#[command(
    name = "group",
    permission = Permission::GroupAdmin,
    guild_only,
    subcommand_required,
    subcommands(
        info,
        subject,
        description,
        announce,
        lock,
        ephemeral,
        addmode,
        linkmode,
        approval,
        news,
        price,
        link,
        resetlink,
        promote,
        demote,
        add,
        kick,
        requests,
        approve,
        reject
    )
)]
async fn group(ctx: Context) -> Result<(), Error> {
    let _ = ctx;
    Ok(())
}

/// Shows this group's name, description, and settings.
#[command(name = "info")]
async fn info(ctx: Context) -> Result<(), Error> {
    let metadata = fetch_metadata(&ctx).await?;
    // A failure to read the setting should not hide the rest of the group's info.
    let news = ctx.data().news.is_enabled(&chat(&ctx).to_string()).ok();
    ctx.say(render_info_with(&metadata, news)).await
}

/// Renames the group.
#[command(name = "subject")]
async fn subject(ctx: Context, #[rest] name: &str) -> Result<(), Error> {
    let name = name.trim();
    if name.is_empty() {
        return ctx.say("Usage: `!group subject <new name>`").await;
    }

    let subject = GroupSubject::new(name)
        .map_err(|_| format!("A group name can be at most {SUBJECT_MAX} characters."))?;
    ctx.message
        .client
        .groups()
        .set_subject(chat(&ctx), subject)
        .await
        .map_err(|error| format!("Failed to rename the group: {error}"))?;

    ctx.say(format!("Group renamed to *{name}*.")).await
}

/// Sets the group description, or clears it when given nothing.
#[command(name = "description")]
async fn description(ctx: Context, #[rest] text: &str) -> Result<(), Error> {
    let text = text.trim();
    let description =
        if text.is_empty() {
            None
        } else {
            Some(GroupDescription::new(text).map_err(|_| {
                format!("A description can be at most {DESCRIPTION_MAX} characters.")
            })?)
        };

    // `Resolve` reads the current description id first, so the update replaces
    // exactly what the group shows now instead of racing another edit.
    ctx.message
        .client
        .groups()
        .set_description(chat(&ctx), description, PreviousDescription::Resolve)
        .await
        .map_err(|error| format!("Failed to update the description: {error}"))?;

    let reply = if text.is_empty() {
        "Group description cleared.".to_string()
    } else {
        "Group description updated.".to_string()
    };
    ctx.say(reply).await
}

/// Sets whether only admins can send messages.
#[command(name = "announce")]
async fn announce(ctx: Context, setting: Toggle) -> Result<(), Error> {
    let enabled = setting.enabled();

    ctx.message
        .client
        .groups()
        .set_announce(chat(&ctx), enabled)
        .await
        .map_err(|error| format!("Failed to change announcement mode: {error}"))?;

    let reply = if enabled {
        "Only admins can send messages now."
    } else {
        "Everyone can send messages again."
    };
    ctx.say(reply).await
}

/// Sets whether only admins can edit the group's info.
#[command(name = "lock")]
async fn lock(ctx: Context, setting: Toggle) -> Result<(), Error> {
    let locked = setting.enabled();

    ctx.message
        .client
        .groups()
        .set_locked(chat(&ctx), locked)
        .await
        .map_err(|error| format!("Failed to change the group lock: {error}"))?;

    let reply = if locked {
        "Only admins can edit the group's info now."
    } else {
        "Everyone can edit the group's info again."
    };
    ctx.say(reply).await
}

/// Sets how long messages stay before disappearing.
#[command(name = "ephemeral")]
async fn ephemeral(ctx: Context, duration: Ephemeral) -> Result<(), Error> {
    let seconds = duration.seconds();

    ctx.message
        .client
        .groups()
        .set_ephemeral(chat(&ctx), seconds)
        .await
        .map_err(|error| format!("Failed to change disappearing messages: {error}"))?;

    ctx.say(format!(
        "Disappearing messages set to {}.",
        ephemeral_label(seconds)
    ))
    .await
}

/// Sets who can add new members.
#[command(name = "addmode")]
async fn addmode(ctx: Context, audience: Audience) -> Result<(), Error> {
    ctx.message
        .client
        .groups()
        .set_member_add_mode(chat(&ctx), audience.add_mode())
        .await
        .map_err(|error| format!("Failed to change who can add members: {error}"))?;

    ctx.say(format!("{} can add members now.", audience.label()))
        .await
}

/// Sets who can share the group's invite link.
#[command(name = "linkmode")]
async fn linkmode(ctx: Context, audience: Audience) -> Result<(), Error> {
    ctx.message
        .client
        .groups()
        .set_member_link_mode(chat(&ctx), audience.link_mode())
        .await
        .map_err(|error| format!("Failed to change who can share the link: {error}"))?;

    ctx.say(format!(
        "{} can share the invite link now.",
        audience.label()
    ))
    .await
}

/// Sets whether an admin must approve new members.
#[command(name = "approval")]
async fn approval(ctx: Context, setting: Toggle) -> Result<(), Error> {
    let enabled = setting.enabled();

    let mode = if enabled {
        MembershipApprovalMode::On
    } else {
        MembershipApprovalMode::Off
    };
    ctx.message
        .client
        .groups()
        .set_membership_approval(chat(&ctx), mode)
        .await
        .map_err(|error| format!("Failed to change join approval: {error}"))?;

    let reply = if enabled {
        "New members now need an admin's approval."
    } else {
        "New members can join without approval."
    };
    ctx.say(reply).await
}

/// Sets whether this group gets a news digest each morning.
#[command(name = "news")]
async fn news(ctx: Context, setting: Toggle) -> Result<(), Error> {
    let enabled = setting.enabled();
    let chat = chat(&ctx).to_string();

    let changed = ctx
        .data()
        .news
        .set_enabled(&chat, enabled)
        .map_err(|error| format!("Failed to save the news setting: {error}"))?;

    let reply = match (enabled, changed) {
        (true, true) => "Morning news is on. This group will get a digest each morning.",
        (true, false) => "Morning news is already on for this group.",
        (false, true) => "Morning news is off. This group will no longer get a digest.",
        (false, false) => "Morning news is already off for this group.",
    };
    ctx.say(reply).await
}

/// Sets whether this group gets a weekly price update, and which symbols it charts.
#[command(name = "price")]
async fn price(ctx: Context, action: PriceAction, symbol: Option<String>) -> Result<(), Error> {
    let chat = chat(&ctx).to_string();
    let store = &ctx.data().price;

    let reply = match action {
        PriceAction::On => {
            let changed = store
                .set_enabled(&chat, true)
                .map_err(|error| format!("Failed to save the price setting: {error}"))?;
            let symbols = store.symbols(&chat).unwrap_or_default();
            if changed {
                format!(
                    "Weekly price update is on. This group will get {} each week.",
                    list_symbols(&symbols)
                )
            } else {
                format!(
                    "The weekly price update is already on for this group ({}).",
                    list_symbols(&symbols)
                )
            }
        }
        PriceAction::Off => {
            let changed = store
                .set_enabled(&chat, false)
                .map_err(|error| format!("Failed to save the price setting: {error}"))?;
            if changed {
                "Weekly price update is off. This group will no longer get a chart.".to_string()
            } else {
                "The weekly price update is already off for this group.".to_string()
            }
        }
        PriceAction::Add => {
            let Some(symbol) = symbol else {
                return ctx.say("Usage: `!group price add <symbol>`").await;
            };
            store
                .add_symbol(&chat, &symbol)
                .map_err(|error| format!("Failed to save the price symbol: {error}"))?;
            format!(
                "Now charting `{}`. This group watches {}.",
                symbol.to_uppercase(),
                list_symbols(&store.symbols(&chat).unwrap_or_default())
            )
        }
        PriceAction::Remove => {
            let Some(symbol) = symbol else {
                return ctx.say("Usage: `!group price remove <symbol>`").await;
            };
            let removed = store
                .remove_symbol(&chat, &symbol)
                .map_err(|error| format!("Failed to save the price symbol: {error}"))?;
            if removed {
                let symbols = store.symbols(&chat).unwrap_or_default();
                if symbols.is_empty() {
                    "Removed the last symbol, so the weekly update is off.".to_string()
                } else {
                    format!("Stopped charting `{}`.", symbol.to_uppercase())
                }
            } else {
                format!("`{}` was not being charted.", symbol.to_uppercase())
            }
        }
        PriceAction::List => {
            let symbols = store.symbols(&chat).unwrap_or_default();
            if symbols.is_empty() {
                "The weekly price update is off. Turn it on with `!group price on`.".to_string()
            } else {
                format!(
                    "Weekly price update: on.\nSymbols: {}\nUse `!price <symbol>` for a one-off chart.",
                    list_symbols(&symbols)
                )
            }
        }
    };
    ctx.say(reply).await
}

/// "`CL=F`", "`CL=F` and `GC=F`", or "`A`, `B`, and `C`".
fn list_symbols(symbols: &[String]) -> String {
    let quoted: Vec<String> = symbols.iter().map(|symbol| format!("`{symbol}`")).collect();
    match quoted.as_slice() {
        [] => "no symbols".to_string(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        _ => {
            let (last, rest) = quoted.split_last().expect("non-empty");
            format!("{}, and {last}", rest.join(", "))
        }
    }
}

/// Shows the group's invite link.
#[command(name = "link")]
async fn link(ctx: Context) -> Result<(), Error> {
    invite_link(&ctx, false).await
}

/// Revokes the current invite link and shows the new one.
#[command(name = "resetlink")]
async fn resetlink(ctx: Context) -> Result<(), Error> {
    invite_link(&ctx, true).await
}

async fn invite_link(ctx: &Context, reset: bool) -> Result<(), Error> {
    let code = ctx
        .message
        .client
        .groups()
        .get_invite_link(chat(ctx), reset)
        .await
        .map_err(|error| format!("Failed to get the invite link: {error}"))?;

    let heading = if reset {
        "Old invite link revoked. The new one is:"
    } else {
        "Invite link:"
    };
    ctx.say(format!("{heading}\n{code}")).await
}

/// Makes members group admins. Tag them or reply to one.
#[command(name = "promote")]
async fn promote(ctx: Context, #[rest] members: &str) -> Result<(), Error> {
    change_members(&ctx, members, MemberAction::Promote).await
}

/// Removes members' admin role. Tag them or reply to one.
#[command(name = "demote")]
async fn demote(ctx: Context, #[rest] members: &str) -> Result<(), Error> {
    change_members(&ctx, members, MemberAction::Demote).await
}

/// Adds members to the group. Tag them, reply, or type a number.
#[command(name = "add")]
async fn add(ctx: Context, #[rest] members: &str) -> Result<(), Error> {
    change_members(&ctx, members, MemberAction::Add).await
}

/// Removes members from the group. Tag them or reply to one.
#[command(name = "kick")]
async fn kick(ctx: Context, #[rest] members: &str) -> Result<(), Error> {
    change_members(&ctx, members, MemberAction::Kick).await
}

/// Approves pending requests to join. Tag them or reply.
#[command(name = "approve")]
async fn approve(ctx: Context, #[rest] members: &str) -> Result<(), Error> {
    change_members(&ctx, members, MemberAction::Approve).await
}

/// Rejects pending requests to join. Tag them or reply.
#[command(name = "reject")]
async fn reject(ctx: Context, #[rest] members: &str) -> Result<(), Error> {
    change_members(&ctx, members, MemberAction::Reject).await
}

/// Lists who is waiting for approval to join.
#[command(name = "requests")]
async fn requests(ctx: Context) -> Result<(), Error> {
    let pending = ctx
        .message
        .client
        .groups()
        .get_membership_requests(chat(&ctx))
        .await
        .map_err(|error| format!("Failed to fetch join requests: {error}"))?;

    if pending.is_empty() {
        return ctx.say("Nobody is waiting to join.").await;
    }

    let mut reply = format!(
        "{} {} waiting to join:",
        pending.len(),
        if pending.len() == 1 {
            "person is"
        } else {
            "people are"
        }
    );
    for request in &pending {
        reply.push_str("\n• ");
        reply.push_str(&request.jid.to_string());
    }
    ctx.say(reply).await
}

/// What a member-targeting subcommand does, so they share one body.
enum MemberAction {
    Promote,
    Demote,
    Add,
    Kick,
    Approve,
    Reject,
}

impl MemberAction {
    /// Whether the target has to already be in the group.
    fn requires_membership(&self) -> bool {
        matches!(self, Self::Promote | Self::Demote | Self::Kick)
    }

    fn usage(&self) -> &'static str {
        match self {
            Self::Promote => "Usage: `!group promote @member`",
            Self::Demote => "Usage: `!group demote @member`",
            Self::Add => "Usage: `!group add @member`",
            Self::Kick => "Usage: `!group kick @member`",
            Self::Approve => "Usage: `!group approve @member`",
            Self::Reject => "Usage: `!group reject @member`",
        }
    }

    fn verb(&self) -> (&'static str, &'static str) {
        match self {
            Self::Promote => ("Promoted", "promote"),
            Self::Demote => ("Demoted", "demote"),
            Self::Add => ("Added", "add"),
            Self::Kick => ("Removed", "kick"),
            Self::Approve => ("Approved", "approve"),
            Self::Reject => ("Rejected", "reject"),
        }
    }

    async fn apply(
        &self,
        ctx: &Context,
        targets: &[Jid],
    ) -> Result<Vec<ParticipantChangeResponse>, String> {
        let groups = ctx.message.client.groups();
        let chat = chat(ctx);
        let result = match self {
            Self::Promote => groups.promote_participants(chat, targets).await,
            Self::Demote => groups.demote_participants(chat, targets).await,
            Self::Add => groups.add_participants(chat, targets).await,
            Self::Kick => groups.remove_participants(chat, targets).await,
            Self::Approve => groups.approve_membership_requests(chat, targets).await,
            Self::Reject => groups.reject_membership_requests(chat, targets).await,
        };
        result.map_err(|error| format!("Failed to {} member: {error}", self.verb().1))
    }
}

/// Resolves who the command addresses, checks they belong where the action
/// needs them to, applies it, and reports how many succeeded.
async fn change_members(ctx: &Context, members: &str, action: MemberAction) -> Result<(), Error> {
    let Some(targets) = collect_targets(&ctx.message.message, members) else {
        return ctx.say(action.usage()).await;
    };

    let targets = if action.requires_membership() {
        let metadata = fetch_metadata(ctx).await?;
        let members: Vec<Jid> = targets
            .into_iter()
            .filter(|target| {
                metadata
                    .participants
                    .iter()
                    .any(|participant| participant_matches(participant, target))
            })
            .collect();
        if members.is_empty() {
            return ctx.say("That member is not in this group.").await;
        }
        members
    } else {
        targets
    };

    let result = action.apply(ctx, &targets).await?;
    ctx.say(summarize(&action, &result)).await
}

/// "Promoted 2 members." with the first refusal WhatsApp gave, if any.
fn summarize(action: &MemberAction, result: &[ParticipantChangeResponse]) -> String {
    let (done, verb) = action.verb();
    let succeeded = result.iter().filter(|change| change.is_ok()).count();
    let reason = result
        .iter()
        .find_map(|change| change.error.as_deref())
        .map(|reason| format!(" (WhatsApp said: {reason})"))
        .unwrap_or_default();

    if succeeded == 0 {
        return format!("Could not {verb} that member{reason}.");
    }

    let mut message = format!(
        "{done} {succeeded} member{}.",
        if succeeded == 1 { "" } else { "s" }
    );
    if succeeded < result.len() {
        message.push_str(&format!(
            " {} of them failed{reason}.",
            result.len() - succeeded
        ));
    }
    message
}

fn chat(ctx: &Context) -> &Jid {
    &ctx.message.info.source.chat
}

async fn fetch_metadata(ctx: &Context) -> Result<GroupMetadata, Error> {
    ctx.message
        .client
        .groups()
        .fetch_metadata(chat(ctx))
        .await
        .map_err(|error| format!("Failed to fetch group metadata: {error}").into())
}

/// WhatsApp's own limits, mirrored so a rejection names the number.
const SUBJECT_MAX: usize = 100;
const DESCRIPTION_MAX: usize = 2048;

/// A switch: `on` or `off`, plus the words people actually type for one.
#[derive(ChoiceParameter, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Toggle {
    #[name = "on"]
    #[name = "yes"]
    #[name = "true"]
    #[name = "enable"]
    #[name = "enabled"]
    On,
    #[name = "off"]
    #[name = "no"]
    #[name = "false"]
    #[name = "disable"]
    #[name = "disabled"]
    Off,
}

impl Toggle {
    fn enabled(self) -> bool {
        matches!(self, Self::On)
    }
}

/// What `!group price` does to this group's weekly update.
#[derive(ChoiceParameter, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PriceAction {
    #[name = "on"]
    #[name = "enable"]
    On,
    #[name = "off"]
    #[name = "disable"]
    Off,
    #[name = "add"]
    #[name = "watch"]
    Add,
    #[name = "remove"]
    #[name = "rm"]
    #[name = "unwatch"]
    Remove,
    #[name = "list"]
    #[name = "show"]
    List,
}

/// The disappearing-message durations WhatsApp offers.
#[derive(ChoiceParameter, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ephemeral {
    #[name = "off"]
    #[name = "0"]
    #[name = "none"]
    #[name = "disable"]
    #[name = "disabled"]
    Off,
    #[name = "24h"]
    #[name = "1d"]
    #[name = "day"]
    #[description = "24 hours"]
    Day,
    #[name = "7d"]
    #[name = "week"]
    #[description = "7 days"]
    Week,
    #[name = "90d"]
    #[description = "90 days"]
    Quarter,
}

impl Ephemeral {
    /// The duration in seconds, which is what the groups API stores.
    fn seconds(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::Day => 86_400,
            Self::Week => 604_800,
            Self::Quarter => 7_776_000,
        }
    }
}

fn ephemeral_label(seconds: u32) -> &'static str {
    match seconds {
        0 => "off",
        86_400 => "24 hours",
        604_800 => "7 days",
        7_776_000 => "90 days",
        _ => "a custom duration",
    }
}

/// Who a permission applies to: the admins, or every member.
#[derive(ChoiceParameter, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Audience {
    #[name = "admin"]
    #[name = "admins"]
    Admin,
    #[name = "all"]
    #[name = "everyone"]
    #[name = "members"]
    All,
}

impl Audience {
    fn add_mode(self) -> MemberAddMode {
        match self {
            Self::Admin => MemberAddMode::AdminAdd,
            Self::All => MemberAddMode::AllMemberAdd,
        }
    }

    fn link_mode(self) -> MemberLinkMode {
        match self {
            Self::Admin => MemberLinkMode::AdminLink,
            Self::All => MemberLinkMode::AllMemberLink,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Admin => "Only admins",
            Self::All => "Everyone",
        }
    }
}

/// The settings an admin checks first, one line each.
///
/// `news` is this bot's own setting rather than one WhatsApp stores, so it is
/// reported separately from the metadata.
pub fn render_info(metadata: &GroupMetadata) -> String {
    render_info_with(metadata, None)
}

/// [`render_info`] plus whether the morning digest is on, when that is known.
pub fn render_info_with(metadata: &GroupMetadata, news: Option<bool>) -> String {
    let mut lines = Vec::new();

    lines.push(format!(
        "*{}*",
        metadata.subject.as_deref().unwrap_or("(no name)")
    ));
    if let Some(description) = metadata.description.as_deref() {
        lines.push(description.to_string());
    }

    let members = metadata.size.unwrap_or(metadata.participants.len() as u32);
    lines.push(format!("{members} members"));

    lines.push(format!(
        "Messages: {}",
        if metadata.is_announcement {
            "admins only"
        } else {
            "everyone"
        }
    ));
    lines.push(format!(
        "Group info: {}",
        if metadata.is_locked {
            "admins only"
        } else {
            "everyone"
        }
    ));
    lines.push(format!(
        "New members: {}",
        if metadata.membership_approval {
            "need approval"
        } else {
            "join freely"
        }
    ));
    lines.push(format!(
        "Who can add members: {}",
        match metadata.member_add_mode {
            Some(MemberAddMode::AdminAdd) => "admins",
            Some(MemberAddMode::AllMemberAdd) => "everyone",
            None => "unknown",
        }
    ));
    lines.push(format!(
        "Disappearing messages: {}",
        ephemeral_label(metadata.ephemeral.as_ref().map(expiration).unwrap_or(0))
    ));
    if let Some(enabled) = news {
        lines.push(format!(
            "Morning news: {}",
            if enabled { "on" } else { "off" }
        ));
    }

    lines.join("\n")
}

fn expiration(settings: &GroupEphemeralSettings) -> u32 {
    settings.expiration.unwrap_or(0)
}

/// Mentions first, then the quoted sender, then any typed numbers.
pub fn collect_targets(message: &wa::Message, members: &str) -> Option<Vec<Jid>> {
    let mut targets = mentioned_jids(message);
    if !targets.is_empty() {
        return Some(targets);
    }

    if let Some(target) = replied_sender(message) {
        return Some(vec![target]);
    }

    for value in members.split_whitespace() {
        if let Some(target) = parse_jid(value) {
            targets.push(target);
        }
    }
    (!targets.is_empty()).then_some(targets)
}

fn mentioned_jids(message: &wa::Message) -> Vec<Jid> {
    message
        .get_base_message()
        .extended_text_message
        .as_option()
        .and_then(|message| message.context_info.as_option())
        .map(|context| {
            context
                .mentioned_jid
                .iter()
                .filter_map(|jid| jid.parse().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn replied_sender(message: &wa::Message) -> Option<Jid> {
    message
        .get_base_message()
        .extended_text_message
        .as_option()
        .and_then(|message| message.context_info.as_option())
        .and_then(|context| context.participant.as_deref())
        .and_then(|participant| participant.parse().ok())
}

/// A full JID (`62812@s.whatsapp.net`) or a bare number, for people who type
/// one instead of tapping a mention.
fn parse_jid(value: &str) -> Option<Jid> {
    let value = value.trim_start_matches('@');
    if let Ok(jid) = value.parse::<Jid>() {
        return Some(jid);
    }

    let digits = value.strip_prefix('+').unwrap_or(value);
    if digits.len() >= 7 && digits.chars().all(|digit| digit.is_ascii_digit()) {
        return format!("{digits}@s.whatsapp.net").parse().ok();
    }
    None
}
