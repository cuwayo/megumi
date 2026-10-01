# Megumi WhatsApp

[![License: GPL v3](https://img.shields.io/badge/license-GPLv3-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-edition%202024-orange.svg)](https://www.rust-lang.org)
[![CI](https://github.com/cuwayo/megumi/actions/workflows/ci.yml/badge.svg)](https://github.com/cuwayo/megumi/actions)

A WhatsApp bot for groups and DMs, built on
[whatsapp-rust](https://github.com/oxidezap/whatsapp-rust). It answers `!`-prefixed
messages with commands for media conversion, song recognition, paper lookup, and
group administration, and it ships with a reusable command framework modelled on
[Poise](https://github.com/serenity-rs/poise) — so adding a command is writing one
`async fn`.

## Features

- **Stickers** — turn an image, video, GIF, or URL into a WhatsApp sticker, and
  convert a whole Telegram sticker pack into WhatsApp packs.
- **Song recognition** — identify a song from an audio or video clip.
- **Paper lookup** — resolve a DOI or title to its metadata and a Sci-Hub link.
- **Group administration** — rename the group, manage members, and change group
  settings, restricted to group admins.
- **Help that writes itself** — `!help` lists every command, grouped, from the
  same metadata the commands are declared with.
- **A real command layer** — typed arguments, subcommands, permission gates,
  cooldowns, and centralized error handling, instead of a hand-rolled match on
  message text.

## Requirements

- [Rust](https://rustup.rs) (edition 2024 — a recent stable toolchain)
- [ffmpeg](https://ffmpeg.org) on `PATH` — used by `!sticker` and `!shazam`
- A Telegram bot token, only if you want `!sticker` to convert Telegram sticker
  packs. Get one from [@BotFather](https://t.me/BotFather).

## Getting started

```bash
git clone https://github.com/cuwayo/megumi.git
cd megumi
cp .env.example .env        # only needed for the Telegram sticker-pack feature
cargo run
```

On the first run the bot prints a QR code. Open WhatsApp → **Linked devices** →
**Link a device** and scan it. The session is stored in `whatsapp.db` (gitignored),
so later runs reconnect without scanning again.

The bot answers from the account you linked. Send `!ping` to the chat to confirm
it is alive, then `!help` to see everything it can do.

## Commands

Every command starts with `!`. Most commands accept aliases, and `!help <command>`
shows a command's own usage and description.

### Everyday

| Command | Aliases | What it does |
| --- | --- | --- |
| `!help [command]` | `h`, `commands` | Lists all commands, or details one |
| `!ping` | `p` | Replies with pong — a liveness check |
| `!echo <text>` | | Repeats the text back |
| `!uptime` | | How long the bot has been running |
| `!scihub <doi or title>` | `sci`, `paper`, `doi` | Looks up a paper and replies with a Sci-Hub link |

### Media

| Command | Aliases | What it does |
| --- | --- | --- |
| `!sticker [url]` | `stiker` | Converts an image, video, GIF, or URL into a sticker |
| `!shazam [url]` | `sz` | Recognises a song from an audio or video clip |

For `!sticker` and `!shazam`, reply to a message that contains the media, send
the command as the media's caption, or pass a public URL. Extra words in a
caption are ignored, so `!sticker please` still works.

`!sticker <t.me/addstickers/...>` converts a Telegram sticker pack into one or
more WhatsApp packs of at most 60 stickers each. An optional second argument
names the pack. This path needs `TELEGRAM_BOT_TOKEN` in `.env`.

### Group administration

`!group` and all of its subcommands work only in groups, and only for group
admins. A bare `!group` tells you it needs a subcommand.

| Command | What it does |
| --- | --- |
| `!group info` | Shows the group's name, description, and settings |
| `!group subject <name>` | Renames the group |
| `!group description [text]` | Sets the description, or clears it when given nothing |
| `!group announce <on\|off>` | Sets whether only admins can send messages |
| `!group lock <on\|off>` | Sets whether only admins can edit the group's info |
| `!group ephemeral <off\|24h\|7d\|90d>` | Sets how long messages stay before disappearing |
| `!group addmode <all\|admins>` | Sets who can add new members |
| `!group linkmode <all\|admins>` | Sets who can share the invite link |
| `!group approval <on\|off>` | Sets whether an admin must approve new members |
| `!group link` | Shows the invite link |
| `!group resetlink` | Revokes the invite link and shows the new one |
| `!group add @member` | Adds members (tag them, reply to one, or type a number) |
| `!group kick @member` | Removes members |
| `!group promote @member` | Makes members group admins |
| `!group demote @member` | Removes members' admin role |
| `!group requests` | Lists who is waiting for approval to join |
| `!group approve @member` | Approves pending join requests |
| `!group reject @member` | Rejects pending join requests |

### Owner

`!console <shell command>` (alias `!sh`) runs a shell command on the machine
hosting the bot and replies with its output. It is restricted to messages the
bot sends itself — the account you linked — and it is hidden from `!help`.
Treat the host accordingly: anyone who can send messages from that account can
run commands on it.

## Configuration

Settings come from a `.env` file in the working directory. Every variable is
optional; without the file the bot runs with sticker-pack conversion disabled.

| Variable | Default | Purpose |
| --- | --- | --- |
| `TELEGRAM_BOT_TOKEN` | unset | Enables `!sticker` for Telegram sticker packs |
| `TELEGRAM_API_BASE` | `https://api.telegram.org` | Override for a proxy or a local Bot API server |
| `RUST_LOG` | unset | Tracing filter, e.g. `megumi=info` to see command outcomes |

## Development

```bash
cargo test --workspace        # build the macros, the framework, and the bot; run every test
cargo clippy --workspace --all-targets
cargo fmt --all
```

The workspace has three crates, all edition 2024:

- `crates/megumi-framework` — the command framework. Its library name is
  `megumi`, which is what command code imports.
- `crates/megumi-framework-macros` — the `#[command]` and `#[group]` proc macros.
- the repository root — the bot binary (`src/main.rs`) and its commands
  (`src/commands/<name>/mod.rs`).

Tests in `crates/megumi-framework/tests/` exercise the macro expansion directly
and are the fastest feedback for macro changes. Tests at the repository root
drive the bot's real command registry, so a new command is covered there once it
is registered.

Some sticker tests shell out to `ffmpeg` and take seconds rather than
milliseconds.

## Adding a command

A command is an async function returning `Result<(), megumi::Error>`. The first
paragraph of its doc comment becomes the one-line description `!help` shows.

```rust
/// Replies with pong.
#[megumi::command(name = "ping", aliases("p"))]
async fn ping(ctx: Context) -> Result<(), megumi::Error> {
    ctx.say("pong").await
}
```

To add one to the bot:

1. Create `src/commands/<name>/mod.rs` with the function.
2. Add the module and a `pub use` in `src/commands/mod.rs`.
3. List it in the `commands(...)` of the `#[group]` it belongs to in that same
   file. The group is the heading it appears under in `!help`.

Parameters after the context are parsed from the message in order. `#[rest]`
takes everything remaining and must be last, and `#[flag]` is a boolean that is
true when the user typed its name:

```rust
#[megumi::command(name = "add")]
async fn add(ctx: Context, a: i32, b: i32) -> Result<(), megumi::Error> {
    ctx.say(format!("{} + {} = {}", a, b, a + b)).await
}
```

`!add 2 3` replies `2 + 3 = 5`. A value that does not parse, or too many or too
few words, is reported back to the chat by the framework's error handler — a
command never has to format its own usage error.

### The framework

`megumi` is a WhatsApp-shaped port of Poise's command design. A WhatsApp group
is Poise's guild, and a message the bot sends itself (`is_from_me`) is the
owner. There is no slash-command layer, because WhatsApp has none.

The attributes `#[command]` accepts:

- `name`, `aliases(...)` — the trigger and its alternates
- `description`, `help_text` — override the doc comment
- `subcommands(...)`, `subcommand_required` — nested commands, reached as
  `!parent child`
- `permission = megumi::Permission::GroupAdmin` — `Everyone`, `GroupAdmin`, or
  `Owner`
- `guild_only`, `dm_only` — restrict to groups or to DMs; neither means both
- `check = predicate` — an extra `async fn(Context) -> Result<bool, Error>`
  gate; a parent's checks also run for its children
- `user_cooldown`, `guild_cooldown`, `channel_cooldown`, `member_cooldown`,
  `global_cooldown` — cooldown in seconds per bucket; a mistyped invocation
  never burns one
- `react = "⌛"` — react to the invoking message while the command runs, and
  remove the reaction when it finishes
- `on_error`, `manual_cooldowns`, `reuse_response`, `discard_spare_arguments`,
  `hide_in_help`

A subcommand inherits its parent's checks, permission, and chat-type
restriction, so those are declared once. Built-in argument types cover strings,
booleans, numbers, `Option<T>`, `Vec<T>`, and enums deriving
`ChoiceParameter`, where each variant is one accepted word.

Commands share state through a type parameter: call
`Framework::builder().setup(|| Data { ... })` and take `megumi::Context<Data>`.
This bot's `Data` records the process start time so `!uptime` measures the
whole run.

## Contributing

Contributions are welcome — bug reports, new commands, and documentation fixes
all help.

1. Fork the repository and create a branch from `main`.
2. Make your change. Match the style of the surrounding code, and keep each
   commit focused on one thing.
3. Run the checks and make sure they pass:

   ```bash
   cargo fmt --all --check
   cargo clippy --workspace --all-targets -- -D warnings
   cargo test --workspace
   ```

4. Open a pull request describing what changed and why.

If you want to add a command, the [Adding a command](#adding-a-command) section
above is the path, and `src/commands/ping/mod.rs` is the smallest complete
example to copy. If you change the `#[command]` macro or a type it expands to,
build with `cargo test --workspace`: the macro crate and the framework must be
rebuilt together, and checking one crate alone can report errors against a
stale expansion.

By contributing, you agree that your contributions are licensed under the
[GNU General Public License v3.0](LICENSE), the same license as the project.

## License

Megumi WhatsApp is free software, licensed under the
[GNU General Public License v3.0](LICENSE) — you can use, modify, and
redistribute it, provided derivative works stay under the same license.

It stands on [whatsapp-rust](https://github.com/oxidezap/whatsapp-rust) by
oxidezap and borrows its command-layer design from
[Poise](https://github.com/serenity-rs/poise) by the serenity-rs project. Both
are used under their own licenses.
