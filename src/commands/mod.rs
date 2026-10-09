pub mod ask;
pub mod console;
pub mod download;
pub mod echo;
pub mod forget;
pub mod group;
pub mod help;
pub mod memory;
pub mod ping;
pub mod remind;
pub mod scihub;
pub mod shazam;
pub mod sticker;
pub mod summary;
pub mod uptime;

pub use ask::ask;
pub use console::console;
pub use download::download;
pub use echo::echo;
pub use forget::forget;
pub use group::group;
pub use help::help;
pub use memory::memory;
pub use ping::ping;
pub use remind::remind;
pub use scihub::scihub;
pub use shazam::shazam;
pub use sticker::sticker;
pub use summary::summary;
pub use uptime::uptime;

use megumi::group;

/// The everyday commands: help, liveness, echoing, and the literature lookup.
///
/// The commands live in their own modules; this module is the group that lists
/// them, so the help listing shows them together.
#[group(
    description = "Everyday commands",
    context = crate::Context,
    commands(
        crate::commands::help,
        crate::commands::ping,
        crate::commands::echo,
        crate::commands::uptime,
        crate::commands::scihub,
    )
)]
pub mod utility {}

/// The commands that turn one kind of media into another.
#[group(
    description = "Media conversion",
    context = crate::Context,
    commands(
        crate::commands::sticker,
        crate::commands::shazam,
        crate::commands::download,
    )
)]
pub mod media {}

/// The group-management commands, reachable only by group admins.
#[group(
    description = "Group management",
    context = crate::Context,
    commands(crate::commands::group)
)]
pub mod admin {}

/// The owner-only operator console.
#[group(
    description = "Owner tools",
    context = crate::Context,
    commands(crate::commands::console)
)]
pub mod owner {}

/// The deterministic entry points to the AI agent.
#[group(
    description = "Assistant",
    context = crate::Context,
    commands(
        crate::commands::ask,
        crate::commands::summary,
        crate::commands::memory,
        crate::commands::forget,
        crate::commands::remind,
    )
)]
pub mod assistant {}
