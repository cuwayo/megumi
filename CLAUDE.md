# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A WhatsApp bot built on [`whatsapp-rust`](https://github.com/oxidezap/whatsapp-rust), plus a command layer
(`megumi-framework`) whose design is ported from [Poise](https://github.com/serenity-rs/poise). The port is
deliberately WhatsApp-shaped: a **WhatsApp group is Poise's guild**, a message from the bot's own account
(`is_from_me`) is the **owner**, and there is no slash-command/autocomplete/modal layer.

## Golden rules

These override taste. When two rules pull in different directions, the earlier one wins.

1. **Simplest thing that is correct.** Prefer the straightforward implementation over a clever one.
   No new abstraction, trait, generic, or crate unless it removes real duplication or complexity that
   already exists. Three similar lines beat a helper that exists to be elegant.
2. **No function that does one thing once.** Inline a function whose body is a single expression or a
   couple of lines and has exactly one caller. Extract only when the function is called from more than
   one place, or when naming it makes a genuinely hard stretch of code readable.
3. **Comment why, never what.** Every module gets a `//!` header saying what it is for, and every
   public item gets a `///` doc comment. Comment a private item or a block only when the reason is not
   obvious from the code: a limit imposed by WhatsApp, an ordering constraint, a trade-off. Do not
   narrate what the next line does.
4. **Match the code around the edit.** Copy the naming, error style, and comment density of the file
   you are in. This repo writes full prose doc comments, not terse ones.
5. **Change only what the task needs.** No drive-by refactors, renames, or reformatting of untouched
   code. Leave a file strictly better than you found it, but only where you were already working.
6. **Verify, don't assume.** A change to a command is done when `cargo test --workspace`,
   `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo +nightly fmt --all --check`
   pass — clippy and fmt are the two gates CI enforces. Say so when they do, and show the failure
   when they don't.

## Commands

```bash
cargo test --workspace          # build macros + framework + bot, run every test
cargo test -p megumi-framework --lib              # framework unit tests only
cargo test -p megumi-framework --test command_macro   # one integration test binary
cargo test -p megumi-framework --test command_macro typed_parameters_are_advertised   # one test
cargo clippy --workspace --all-targets
cargo +nightly fmt --all        # CI pins rustfmt to nightly; stable fmt will disagree
cargo run                       # starts the bot; prints a QR code to scan on first run
```

CI (`.github/workflows/ci.yml`) runs `cargo fmt --all --check` with nightly rustfmt and
`cargo clippy --workspace --all-targets -- -D warnings`.

Running the bot needs `ffmpeg` on `PATH` (`!sticker` and `!shazam` shell out to it),
`yt-dlp` on `PATH` (`!download` shells out to it; it merges with `ffmpeg`), and a `.env`
(see `.env.example`). Every variable is optional: `TELEGRAM_BOT_TOKEN` enables the Telegram
sticker-pack path of `!sticker`, `TELEGRAM_API_BASE` overrides the Telegram API host, and
`RUST_LOG` sets the tracing filter (the default is `megumi=info,whatsapp_rust=info,warn`).
`whatsapp.db*` is the SQLite session store, gitignored.

Test binaries in `crates/megumi-framework/tests/` exercise the `#[command]` macro expansion directly and are
the fastest way to check macro changes. Tests in the repo-root `tests/` drive the **real** bot registry via
`megumi_whatsapp::framework()`, so adding a command there is what makes `routing.rs` pass.

Some sticker tests shell out to `ffmpeg`; they are the slow ones (seconds, not milliseconds).

## Workspace layout

Three crates, all `edition = "2024"`, `resolver = "3"`:

- `crates/megumi-framework-macros` — the `#[command]` / `#[group]` / `#[derive(ChoiceParameter)]` proc macros.
  `command.rs` parses attributes and emits a `RegisteredCommand`; `signature.rs` turns the function signature
  into argument bindings; `choice_parameter.rs` turns an enum's variants into the words it accepts.
- `crates/megumi-framework` — the library. **Its lib name is `megumi`**, so the bot depends on it as
  `megumi = { package = "megumi-framework", ... }`. Public API is re-exported from `lib.rs`.
- repo root — the bot binary (`src/main.rs`), its commands (`src/commands/<name>/mod.rs`), and integration tests.

The proc macros expand against `::megumi::__private::{RegisteredCommand, GroupDescriptor, ChoicesOf}` and
`::megumi::{PopArgument, Args, Context, ...}`. Changing a type those paths name means the **macro crate and
the framework must be rebuilt together** — `cargo test --workspace` does this; a lone `cargo check -p megumi`
can report stale errors against an old expansion.

## Command layer architecture

`Command` (framework) and `RegisteredCommand` (macro output) carry the same metadata fields; `IntoCommand`
converts one to the other. When you add a field to `Command`, add it to `RegisteredCommand`, to
`RegisteredCommand::into_command`, and emit it from `expand_command` in the macro crate — the three must stay
in sync or every command stops compiling.

`Context`, `Command`, `Framework`, and `FrameworkBuilder` are all generic over user data `U`
(Poise's `U`), defaulting to `NoData`. A command written against `ctx: Context` stays
`RegisteredCommand<NoData>`; a bot that shares state aliases `type Context = megumi::Context<Data>`
and calls `Framework::builder().setup(|client| async move { Ok(Data { ... }) })` **before**
registering commands. The macro projects `U` via `_GetGenerics` from the first parameter's type.
`setup` panics if commands were already added, because a `Command<NoData>` cannot become a
`Command<Data>`.

`setup` is **async and connect-driven**: it receives the connected `Arc<Client>` and runs once,
lazily, on the **first event** the framework sees — not at `build()`. Lazy-on-first-event (rather
than only on `Connected`) is deliberate: whatsapp-rust can deliver a message before it announces the
connection, and its default `EventDelivery::Concurrent` spawns each event's callback on its own task,
so no ordering between event kinds is guaranteed. A failed setup logs and drops every later event.

The framework is **injected into the client**, the way Poise is handed to Serenity's client builder:
`FrameworkExt::framework` is a trait extension on `whatsapp_rust::BotBuilder` (which has no
`framework` method of its own) that registers an `on_event` callback running
`Framework::dispatch_event`. The client drives the framework; the framework's optional `event_handler`
(a `fn(FrameworkContext<U>, Arc<Event>) -> BoxFuture<Result<(), Error>>`) runs after the framework has
handled the event's messages. Setup and event-handler failures are **logged**, not routed through
`on_error`, which expects a command context they lack.

Dispatch (`Framework::dispatch_event` → `dispatch_message` → `Command::invoke`) runs in this order:

1. strip a configured prefix, look the name up in the alias map (only **top-level** commands are registered)
2. descend into `subcommands` for as long as the next word names a child; `subcommand_required` errors here
3. gates: `guild_only`/`dm_only`, permission (owner/group-admin), framework `command_check`, the command's own
   `checks`, then cooldown **consult**
4. `react` on the invoking message, `pre_command` hook
5. the command body, which parses its own typed arguments and calls `start_cooldown` **after** parsing
   (a mistyped invocation must not burn a cooldown)
6. on `Ok`, `post_command`; the reaction is removed either way

Errors never reply inline: `Command::invoke` returns a `FrameworkError`, and the dispatcher hands it to the
command's `on_error` or the framework's. The default handler answers the chat quoting the offending message.

### Argument parsing

A command parameter that is not `Context` or `Args` is popped off the front of the message via `PopArgument`
(sync — unlike Poise, nothing needs a network round-trip). `#[rest]` consumes the remainder and must be last;
a bare `&str` immediately after an `Args` parameter is treated as `#[rest]` too, for backward compatibility.
`#[flag]` is a boolean that is true when the user typed the parameter's name. `#[lazy]` defers parsing of one
parameter; it cannot combine with `#[rest]` or `#[flag]`.

An enum deriving `ChoiceParameter` is a parameter whose accepted words are its variants. The macro advertises
those choices through `__private::ChoicesOf`, which resolves to the enum's `list()` only for types that
implement the trait and to nothing otherwise — that resolution trick is why a new parameter type needs no
registration.

`#[command(subcommands(...))]` takes **idents**, not string literals: `subcommands(get, set)`. A subcommand
inherits its parent's checks, permission, and chat-type restriction, so those are declared once on the parent.

## WhatsApp-specific constraints the framework encodes

- A WhatsApp message carries **one** media payload. `CreateReply` attaching a second replaces the first;
  text plus a sticker/audio attachment is refused rather than silently dropped. A link preview is dropped
  when media is attached, but a `link_card` survives an attachment and replaces a link preview.
- Editing is text-to-text only. `reuse_response` stores the first reply's id and edits it, but an attachment
  falls back to sending a new message (see `CreateReply::edit`).
- `Context::attachment()` reads the message itself or the message it quotes; `Context::download` fetches bytes.
- `!sticker` converts media locally with `ffmpeg`; its transcode path steps quality/frame-rate down to fit
  WhatsApp's 100 KB still / 500 KB animated sticker limits (`src/commands/sticker/transcode.rs`).

## The bot's commands

`src/lib.rs::framework()` registers four `#[group]`s defined in `src/commands/mod.rs`, which is what `!help`
groups commands under. Each group's `context = crate::Context` is how the macro learns the bot's `Data` type.

- `utility` — `help`, `ping`, `echo`, `uptime`, `scihub`. `!scihub` resolves a DOI (bare, `doi.org`, or
  Sci-Hub URL) or a title against the Crossref API and replies with a Sci-Hub link.
- `media` — `sticker`, `shazam`, `download`. The first two take their input from a quoted message, the
  command's own caption, or a URL, and ignore spare words in a caption. `!download <url>` fetches the video
  at an http(s) address with `yt-dlp` (`src/commands/download/`) and sends it back. `yt-dlp` is told to
  refuse anything over 100 MB (`MAX_DOWNLOAD`); what comes back is sent as a playable video up to 64 MB and
  as a document past that. `!shazam` transcodes to 16 kHz mono PCM with `ffmpeg`,
  fingerprints it locally (`src/commands/shazam/fingerprint.rs`), and recognises it against Shazam.
  `!sticker <t.me/addstickers/...>` converts a Telegram sticker pack into WhatsApp packs of at most 60
  stickers; that path is the only one needing `TELEGRAM_BOT_TOKEN`.
- `admin` — `group`, whose subcommands rename the group, change its settings, and manage members. The parent
  declares `guild_only`, `permission = GroupAdmin`, and `subcommand_required`, so its body never runs and
  every child inherits the gates. Settings that take a fixed set of words (`announce`, `ephemeral`,
  `addmode`, …) parse them with `ChoiceParameter` enums. `!group news` is the one setting the bot keeps
  itself: it subscribes the group to a morning RSS digest (`src/news/`), stored in `news.json` and posted by a
  task spawned when the client connects.
- `owner` — `console` (alias `sh`), `permission = Owner` and `hide_in_help`. It runs the rest of the message
  under `sh -c` with a 10 s timeout and replies with the tail of the output.

`Data` (in `src/data.rs`) holds the process start time, pinned in `framework()` so `!uptime` measures the
whole run, the news subscription store, and the handle to the running digest task. The start time is pinned
at build time (outside the async `setup` closure) so it still precedes the first command; the rest is built
inside `setup`. The digest loop is started by `src/news::event_handler`, the framework's `event_handler`,
on the `Connected` event — `main` no longer wires `on_connected` itself.

## Adding a command

Create `src/commands/<name>/mod.rs` with an `async fn` returning `Result<(), megumi::Error>`, add the module
and a `pub use` to `src/commands/mod.rs`, then add it to the `commands(...)` of the `#[group]` it belongs to
in that same file. The group's `description` is the heading it appears under in `!help`.
`README.md` documents every supported `#[command]` attribute and is the reference for what the macro accepts.
