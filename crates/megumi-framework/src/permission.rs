//! Permissions, and the group-admin checks behind them.

use whatsapp_rust::types::message::MessageSource;
use whatsapp_rust::wacore_binary::JidExt;
use whatsapp_rust::{GroupParticipant, Jid};

/// Whether `jid` addresses the same user as `participant`.
///
/// WhatsApp addresses one person by either their phone number or their LID, and
/// a group's participant list can hold the two in separate fields, so every form
/// present on the participant is compared.
pub fn participant_matches(participant: &GroupParticipant, jid: &Jid) -> bool {
    participant.jid.is_same_user_as(jid)
        || participant
            .phone_number
            .as_ref()
            .is_some_and(|phone| phone.is_same_user_as(jid))
        || participant
            .lid
            .as_ref()
            .is_some_and(|lid| lid.is_same_user_as(jid))
}

/// Whether any of `author_jids` is an admin participant.
pub fn is_admin_participant(participants: &[GroupParticipant], author_jids: &[Jid]) -> bool {
    participants.iter().any(|participant| {
        participant.is_admin()
            && author_jids
                .iter()
                .any(|jid| participant_matches(participant, jid))
    })
}

/// JIDs that identify this message's author.
///
/// A message of our own is authored by this account, whichever form the stanza
/// addressed it by (phone number or LID), so the paired form held locally is a
/// candidate too. Someone else's message contributes only its own addressing
/// forms: this account's admin role must never authorise another member.
pub fn author_jids(source: &MessageSource, own_pn: Option<Jid>, own_lid: Option<Jid>) -> Vec<Jid> {
    let mut jids = vec![source.sender.clone()];
    jids.extend(source.sender_alt.clone());
    if source.is_from_me {
        jids.extend(own_pn);
        jids.extend(own_lid);
    }
    jids
}

/// Who may run a command, checked before its body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permission {
    /// Anyone in the chat.
    Everyone,
    /// Only a message from the bot's own account (`is_from_me`).
    Owner,
    /// Only a group admin, checked against the group's participant list.
    GroupAdmin,
}

/// Outcome of the group-admin check behind [`Permission::GroupAdmin`].
pub(crate) enum AdminStatus {
    Admin,
    NotAdmin,
    /// The group's participant list could not be read, so calling the author a
    /// non-admin would be a guess.
    Unreadable(String),
}
