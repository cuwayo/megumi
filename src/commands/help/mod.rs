use chrono::Local;
use megumi::{CreateReply, Error, LinkPreview, command};

use crate::Context;

const SOURCE_URL: &str = "https://github.com/cuwayo/megumi";
const MENU_THUMBNAIL: &[u8] = include_bytes!("menu-thumbnail.jpg");

/// Shows the available commands, or the details of one.
#[command(name = "help", aliases("h", "commands"))]
async fn help(ctx: Context, #[rest] query: &str) -> Result<(), Error> {
    let query = query.trim();
    let help = if query.is_empty() {
        ctx.help_text()
            .unwrap_or_else(|| "No commands are registered.".to_string())
    } else {
        ctx.command_help(query)
            .unwrap_or_else(|| format!("Command `{query}` was not found."))
    };

    let name = ctx.message.info.push_name.as_str();
    let now = Local::now().format("%A, %d %B %Y %H:%M");
    let content = format!("👋 Hi, {name}!\n📅 {now}\n💻 Source Code: {SOURCE_URL}\n\n{help}");

    ctx.send(
        CreateReply::new().content(content).link_preview(
            LinkPreview::new(SOURCE_URL)
                .title("Megumi Project")
                .description(SOURCE_URL)
                .high_quality_thumbnail(MENU_THUMBNAIL.to_vec()),
        ),
    )
    .await
}
