pub mod console;
pub mod download;
pub mod echo;
pub mod group;
pub mod help;
pub mod ping;
pub mod price;
pub mod scihub;
pub mod shazam;
pub mod sticker;
pub mod uptime;

pub use console::console;
pub use download::download;
pub use echo::echo;
pub use group::group;
pub use help::help;
pub use ping::ping;
pub use price::price;
pub use scihub::scihub;
pub use shazam::shazam;
pub use sticker::sticker;
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
        crate::commands::price,
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
