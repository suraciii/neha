//! `neha` runs a Space and talks to its Bots from a terminal.

mod client;
mod computer;

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal as _, Write};

use anyhow::{Context as _, Result, bail};
use clap::{ArgGroup, Parser, Subcommand};
use neha::id::Id;
use neha::record::RuleEffect;
use neha::seq::Seq;
use neha::surface::{
    BotComputer, BotCreate, CommandCall, MessageSend, ModelSpec, ProviderAdd, ProviderProtocol,
};
use neha_space::Home;
use serde_json::Value;

use crate::client::Credential;

/// Run a Space and talk to its Bots.
#[derive(Parser)]
#[command(name = "neha", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Top,
}

#[derive(Subcommand)]
enum Top {
    /// Start a Space on this machine.
    Serve {
        /// The loopback port; 0 picks a free one.
        #[arg(long, default_value_t = 7337)]
        port: u16,
        /// The browser client directory; defaults to NEHA_UI_DIR or ui/dist.
        #[arg(long)]
        ui: Option<std::path::PathBuf>,
    },
    /// Join a Space from its invitation link.
    Join {
        /// The invitation link printed by `neha serve`.
        link: String,
        /// Your display name in the Space.
        #[arg(long)]
        name: String,
    },
    /// Sign in to a Space you already joined.
    Login {
        /// Your display name in the Space.
        #[arg(long)]
        name: String,
        /// The Space URL; defaults to the saved one.
        #[arg(long)]
        server: Option<String>,
    },
    /// Show the account and Space this CLI uses.
    Whoami,
    /// Connect or list Providers.
    Provider {
        #[command(subcommand)]
        command: ProviderCommand,
    },
    /// List the Models the Space offers.
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
    /// Create or list Bots.
    Bot {
        #[command(subcommand)]
        command: BotCommand,
    },
    /// Pair this machine as a Computer or run its daemon.
    Computer {
        #[command(subcommand)]
        command: ComputerCommand,
    },
    /// List Chats and exchange Messages.
    Chat {
        #[command(subcommand)]
        command: ChatCommand,
    },
    /// Download Files.
    File {
        #[command(subcommand)]
        command: FileCommand,
    },
    /// Read a run.
    Run {
        #[command(subcommand)]
        command: RunCommand,
    },
    /// Answer approval requests and manage approval rules.
    Approval {
        #[command(subcommand)]
        command: ApprovalCommand,
    },
    /// Manage approval rules.
    Rule {
        #[command(subcommand)]
        command: RuleCommand,
    },
}

#[derive(Subcommand)]
enum ProviderCommand {
    /// Connect an OpenAI Chat Completions Provider.
    Add {
        /// The Provider's base URL, such as http://127.0.0.1:8000/v1.
        #[arg(long)]
        base_url: String,
        /// The protocol; only chat-completions is supported.
        #[arg(long, default_value = "chat-completions")]
        protocol: String,
        /// A Model name at the Provider; repeat for more.
        #[arg(long = "model", value_name = "NAME", required = true)]
        models: Vec<String>,
        /// The Model's context window in tokens.
        #[arg(long, default_value_t = 128_000)]
        context_limit: u32,
        /// The Model's largest reply in tokens.
        #[arg(long, default_value_t = 8_192)]
        max_output: u32,
        /// The Model does not read images.
        #[arg(long)]
        no_images: bool,
    },
    /// List connected Providers.
    List,
}

#[derive(Subcommand)]
enum ModelCommand {
    /// List the Models the Space offers.
    List,
}

#[derive(Subcommand)]
enum BotCommand {
    /// Create a Bot with its own Chat and Computer.
    Create {
        /// The Bot's name.
        name: String,
        /// Where the Bot works: local, hosted, or a Computer id.
        ///
        /// Defaults to the owner's Local Computer when one is paired, and a
        /// fresh Hosted Computer otherwise.
        #[arg(long, value_name = "LOCAL|HOSTED|ID")]
        computer: Option<String>,
    },
    /// Move your Bot to another real Computer you own.
    Move {
        /// The Bot identifier.
        bot: Id,
        /// The real Computer it should work on.
        #[arg(long)]
        computer: Id,
    },
    /// Show your Bot, its Chat, and its Computer.
    Show {
        /// The Bot identifier.
        bot: Id,
    },
    /// List your Bots.
    List,
}

#[derive(Subcommand)]
enum ComputerCommand {
    /// Pair this machine with the Space on this machine.
    Pair {
        /// The Space URL; defaults to the ticket file's or the saved Space's.
        #[arg(long)]
        space: Option<String>,
        /// The ticket file; defaults to a Space ticket under the Neha
        /// configuration directory.
        #[arg(long)]
        ticket_file: Option<std::path::PathBuf>,
        /// The Workspace root that holds one folder per Bot.
        #[arg(long)]
        workspace: Option<std::path::PathBuf>,
        /// Pair without asking for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Start this machine's daemon with its saved pairing.
    Run {
        /// The Workspace root that holds one folder per Bot.
        #[arg(long)]
        workspace: Option<std::path::PathBuf>,
    },
    /// Add the machine that printed a pairing code to this Space.
    Add {
        /// The pairing code the machine printed.
        code: String,
    },
    /// List the Computers you own.
    List,
}

#[derive(Subcommand)]
enum ChatCommand {
    /// List your Chats.
    List,
    /// Send a Message, with an optional File, and wait for the Bot's reply.
    #[command(group(ArgGroup::new("payload").required(true).args(["text", "file"])))]
    Send {
        /// The Chat identifier.
        #[arg(long)]
        chat: Option<Id>,
        /// The Bot whose Chat to use instead of --chat.
        #[arg(long)]
        bot: Option<Id>,
        /// The Message text; leave it out when --file carries the Message.
        text: Option<String>,
        /// A file to upload and send with the Message.
        #[arg(long)]
        file: Option<std::path::PathBuf>,
        /// Print the run's steps and actions while it works.
        #[arg(long)]
        follow: bool,
    },
    /// List a Chat's Files.
    Files {
        /// The Chat identifier.
        #[arg(long)]
        chat: Option<Id>,
        /// The Bot whose Chat to use instead of --chat.
        #[arg(long)]
        bot: Option<Id>,
    },
    /// Read a Chat's Messages.
    Read {
        /// The Chat identifier.
        chat: Id,
        /// Start after this cursor.
        #[arg(long)]
        from: Option<Seq>,
    },
}

#[derive(Subcommand)]
enum FileCommand {
    /// Download a File.
    Get {
        /// The File identifier.
        file: Id,
        /// Where to write it; an existing directory receives the File under its name.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand)]
enum RunCommand {
    /// Show one run.
    Show {
        /// The run identifier.
        run: Id,
    },
}

#[derive(Subcommand)]
enum ApprovalCommand {
    /// List approvals waiting for you.
    List,
    /// Show one approval waiting for you.
    Show {
        /// The approval identifier.
        approval: Id,
    },
    /// Allow one approval; the waiting run continues.
    Allow {
        /// The approval identifier.
        approval: Id,
        /// Also allow this kind of action from now on.
        #[arg(long)]
        always: bool,
    },
    /// Deny one approval; the waiting run stops.
    Deny {
        /// The approval identifier.
        approval: Id,
    },
}

#[derive(Subcommand)]
enum RuleCommand {
    /// List rules.
    List {
        /// Only the rules of this Bot.
        #[arg(long)]
        bot: Option<Id>,
    },
    /// Add a rule that asks before, or allows, matching actions.
    Add {
        /// The Bot the rule applies to.
        #[arg(long)]
        bot: Id,
        /// What the rule does: ask before the action, or allow it.
        #[arg(long, value_enum)]
        effect: RuleEffectChoice,
        /// The tool the rule matches, such as shell or write_file.
        #[arg(long)]
        tool: String,
        /// Match shell commands that start with this text.
        #[arg(long)]
        command_prefix: Option<String>,
        /// Match file paths that start with this text.
        #[arg(long)]
        path_prefix: Option<String>,
    },
    /// Remove one rule.
    Remove {
        /// The rule identifier.
        rule: Id,
    },
}

/// The effect a `rule add` asks the Space for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum RuleEffectChoice {
    /// Ask before each matching action.
    Ask,
    /// Allow matching actions without asking.
    Allow,
}

impl RuleEffectChoice {
    /// Returns the pinned wire value.
    fn wire(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Allow => "allow",
        }
    }
}

impl From<RuleEffectChoice> for RuleEffect {
    fn from(choice: RuleEffectChoice) -> Self {
        match choice {
            RuleEffectChoice::Ask => Self::Ask,
            RuleEffectChoice::Allow => Self::Allow,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Top::Serve { port, ui } => serve(port, ui).await,
        Top::Join { link, name } => join(&link, &name).await,
        Top::Login { name, server } => login(&name, server).await,
        Top::Whoami => whoami(),
        Top::Provider { command } => provider(command).await,
        Top::Model { command } => model(command).await,
        Top::Bot { command } => bot(command).await,
        Top::Computer { command } => computer_command(command).await,
        Top::Chat { command } => chat(command).await,
        Top::File { command } => file(command).await,
        Top::Run { command } => run(command).await,
        Top::Approval { command } => approval(command).await,
        Top::Rule { command } => rule(command).await,
    }
}

async fn serve(port: u16, ui: Option<std::path::PathBuf>) -> Result<()> {
    let home = Home::from_env()
        .context("NEHA_HOME is not set; set it to the directory that holds this Space")?;
    let ui = ui
        .or_else(|| std::env::var_os("NEHA_UI_DIR").map(std::path::PathBuf::from))
        .or_else(|| Some(std::path::PathBuf::from("ui/dist")).filter(|path| path.is_dir()));
    if let Some(path) = &ui
        && !path.join("index.html").is_file()
    {
        bail!("the browser client at {} has no index.html", path.display());
    }
    let has_ui = ui.is_some();
    let running = neha_space::start_with_ui(home, port, ui)
        .await
        .context("the Space cannot start")?;

    println!("Neha is listening on {}", running.url());
    if let Some(invite) = &running.invite {
        println!("Invite: {invite}");
        if has_ui {
            if let Some(browser) = running.browser_invite() {
                println!("Open in a browser: {browser}");
            }
            println!("Or join from the CLI: neha join '{invite}' --name <your name>");
        } else {
            println!("Join with: neha join '{invite}' --name <your name>");
            println!("Build ui/ with `pnpm --dir ui build` to use the browser client.");
        }
    } else if has_ui {
        println!("Open {} in a browser.", running.browser_url());
        println!(
            "Sign in from the CLI with: neha login --name <name> --server {}",
            running.url()
        );
    } else {
        println!(
            "Sign in with: neha login --name <name> --server {}",
            running.url()
        );
        println!("Build ui/ with `pnpm --dir ui build` to use the browser client.");
    }
    println!("Files are stored in {}", running.storage());
    if let Some(hint) = pairing_hint(running.local_ticket.as_deref()) {
        println!("{hint}");
    }
    println!("Press Ctrl-C to stop.");

    tokio::signal::ctrl_c()
        .await
        .context("cannot listen for Ctrl-C")?;
    running.shutdown().await;

    Ok(())
}

async fn join(link: &str, name: &str) -> Result<()> {
    client::split_invite(link)?;
    let password = read_password()?;
    let credential = client::join(link, name, &password).await?;
    credential.save()?;
    println!(
        "Joined {} as {} (Admin: {}).",
        credential.url,
        credential.account.display_name,
        if credential.account.is_admin {
            "yes"
        } else {
            "no"
        }
    );

    Ok(())
}

async fn login(name: &str, server: Option<String>) -> Result<()> {
    let server = match server {
        Some(server) => server,
        None => {
            Credential::load()
                .context("pass --server when this machine has no saved Space")?
                .url
        }
    };
    let password = read_password()?;
    let credential = client::login(&server, name, &password).await?;
    credential.save()?;
    println!(
        "Signed in to {} as {}.",
        credential.url, credential.account.display_name
    );

    Ok(())
}

fn whoami() -> Result<()> {
    let credential = Credential::load()?;
    println!("Space: {}", credential.url);
    println!("Account: {}", credential.account.display_name);
    println!("Id: {}", credential.account.id);
    println!(
        "Admin: {}",
        if credential.account.is_admin {
            "yes"
        } else {
            "no"
        }
    );

    Ok(())
}

async fn provider(command: ProviderCommand) -> Result<()> {
    let credential = Credential::load()?;
    match command {
        ProviderCommand::Add {
            base_url,
            protocol,
            models,
            context_limit,
            max_output,
            no_images,
        } => {
            if protocol != "chat-completions" {
                bail!("this version only speaks chat-completions");
            }
            let api_key = provider_key()?;
            let default = models.first().cloned();
            let result = client::command(
                &credential,
                CommandCall::ProviderAdd(ProviderAdd {
                    protocol: ProviderProtocol::ChatCompletions,
                    base_url: base_url.clone(),
                    api_key,
                    models: models
                        .into_iter()
                        .map(|name| ModelSpec {
                            name,
                            context_limit,
                            max_output,
                            reads_images: !no_images,
                        })
                        .collect(),
                    default_model: default,
                }),
            )
            .await?;
            println!(
                "Connected Provider {} to {base_url}.",
                id_of(&result, "provider")?
            );
            for model in result["models"]
                .as_array()
                .context("no Models in the answer")?
            {
                println!(
                    "  Model {} ({})",
                    id_of(model, "id")?,
                    text_of(model, "name")?
                );
            }
        }
        ProviderCommand::List => {
            let result = client::command(&credential, CommandCall::ProviderList).await?;
            for provider in result.as_array().context("no Providers in the answer")? {
                println!(
                    "{}  {}  {}",
                    id_of(provider, "id")?,
                    text_of(provider, "protocol")?,
                    text_of(provider, "base_url")?
                );
            }
        }
    }

    Ok(())
}

async fn model(command: ModelCommand) -> Result<()> {
    let credential = Credential::load()?;
    match command {
        ModelCommand::List => {
            let result = client::command(&credential, CommandCall::ModelList).await?;
            for model in result.as_array().context("no Models in the answer")? {
                let default = if model["is_default"].as_bool().unwrap_or(false) {
                    "  default"
                } else {
                    ""
                };
                let images = if model["reads_images"].as_bool().unwrap_or(false) {
                    "images"
                } else {
                    "no images"
                };
                println!(
                    "{}  {}  context {}  max output {}  {images}{default}",
                    id_of(model, "id")?,
                    text_of(model, "name")?,
                    model["context_limit"].as_u64().unwrap_or_default(),
                    model["max_output"].as_u64().unwrap_or_default()
                );
            }
        }
    }

    Ok(())
}

async fn bot(command: BotCommand) -> Result<()> {
    let credential = Credential::load()?;
    match command {
        BotCommand::Create { name, computer } => {
            let computer = computer.as_deref().map(parse_bot_computer).transpose()?;
            let result = client::command(
                &credential,
                CommandCall::BotCreate(BotCreate {
                    name: name.clone(),
                    computer,
                }),
            )
            .await?;
            println!("Created Bot {} ({name}).", id_of(&result["bot"], "id")?);
            println!("  Chat: {}", id_of(&result["chat"], "id")?);
            println!("  Computer: {}", id_of(&result["computer"], "id")?);
            println!(
                "Say hello with: neha chat send --bot {} \"hello\"",
                id_of(&result["bot"], "id")?
            );
        }
        BotCommand::Move { bot, computer } => {
            let result =
                client::command(&credential, CommandCall::BotMove { bot, computer }).await?;
            println!(
                "Bot {} ({}) now works on Computer {} ({}, online: {}).",
                id_of(&result["bot"], "id")?,
                text_of(&result["bot"], "name")?,
                id_of(&result["to"], "id")?,
                text_of(&result["to"], "kind")?,
                result["to"]["online"].as_bool().unwrap_or(false),
            );
        }
        BotCommand::Show { bot } => {
            let result = client::command(&credential, CommandCall::BotShow { bot }).await?;
            println!(
                "Bot {} ({})",
                id_of(&result["bot"], "id")?,
                text_of(&result["bot"], "name")?
            );
            println!("  Chat: {}", id_of(&result["chat"], "id")?);
            println!(
                "  Computer: {} ({}, online: {})",
                id_of(&result["computer"], "id")?,
                computer_kind_label(text_of(&result["computer"], "kind")?),
                result["computer"]["online"].as_bool().unwrap_or(false)
            );
            // A Local Computer's Workspace lives on that machine, so the
            // Space records no path for it.
            let workspace = text_of(&result["computer"], "workspace")?;
            if !workspace.is_empty() {
                println!("  Workspace: {workspace}");
            }
        }
        BotCommand::List => {
            let result = client::command(&credential, CommandCall::BotList).await?;
            for bot in result.as_array().context("no Bots in the answer")? {
                println!(
                    "{}  {}  chat {}",
                    id_of(bot, "id")?,
                    text_of(bot, "name")?,
                    id_of(bot, "chat")?
                );
            }
        }
    }

    Ok(())
}

async fn computer_command(command: ComputerCommand) -> Result<()> {
    match command {
        ComputerCommand::Pair {
            space,
            ticket_file,
            workspace,
            yes,
        } => computer::pair(space, ticket_file, workspace, yes).await?,
        ComputerCommand::Run { workspace } => computer::run(workspace).await?,
        ComputerCommand::Add { code } => {
            let credential = Credential::load()?;
            let computer = client::command(&credential, CommandCall::ComputerAdd { code }).await?;
            let id = id_of(&computer, "id")?;
            println!("Added Computer {id} ({}).", text_of(&computer, "kind")?);
            println!("  Workspace: {}", text_of(&computer, "workspace")?);
            println!("Create a Bot on it with: neha bot create <name> --computer {id}");
        }
        ComputerCommand::List => {
            let credential = Credential::load()?;
            let result = client::command(&credential, CommandCall::ComputerList).await?;
            for computer in result.as_array().context("no Computers in the answer")? {
                let online = if computer["online"].as_bool().unwrap_or(false) {
                    "online"
                } else {
                    "offline"
                };
                println!(
                    "{}  {}  {online}  {}",
                    id_of(computer, "id")?,
                    text_of(computer, "kind")?,
                    text_of(computer, "workspace")?
                );
            }
        }
    }

    Ok(())
}

async fn chat(command: ChatCommand) -> Result<()> {
    let credential = Credential::load()?;
    match command {
        ChatCommand::List => {
            let result = client::command(&credential, CommandCall::ChatList).await?;
            for chat in result.as_array().context("no Chats in the answer")? {
                println!("{}  Bot {}", id_of(chat, "id")?, id_of(chat, "bot")?);
            }
        }
        ChatCommand::Send {
            chat,
            bot,
            text,
            file,
            follow,
        } => {
            let chat = resolve_chat(&credential, chat, bot).await?;
            let uploaded = match &file {
                Some(path) => Some(client::upload_file(&credential, chat, path).await?),
                None => None,
            };
            let command = CommandCall::MessageSend(MessageSend {
                chat,
                text: text.unwrap_or_default(),
                file: uploaded.as_ref().map(|file| file.id),
            });
            let result = client::command(&credential, command).await?;
            let run = id_of(&result, "run")?;
            let mut progress = Progress::new(follow);
            progress.accepted(&result);
            // A run that stops at an approval stays where it is: a terminal
            // asks and answers it here, a script gets the ids and the command
            // to answer it later.
            let view = match follow_run(&credential, chat, run, &mut progress).await? {
                Follow::Finished(view) => view,
                Follow::Waiting => return Ok(()),
            };
            let status = text_of(&view, "status")?;
            if status == "succeeded" {
                println!("{}", reply_text(&credential, chat, &view).await?);
            }
            println!("  run {run} ({status})");
            if let Some(file) = uploaded {
                println!("  file {} ({})", file.id, file.name);
            }
            if status != "succeeded" {
                bail!("{}", text_of(&view, "error").unwrap_or("the run failed"));
            }
        }
        ChatCommand::Files { chat, bot } => {
            let chat = resolve_chat(&credential, chat, bot).await?;
            let result = client::command(&credential, CommandCall::FileList { chat }).await?;
            for file in result.as_array().context("no Files in the answer")? {
                println!(
                    "{}  {}  {} bytes  {}",
                    id_of(file, "id")?,
                    text_of(file, "name")?,
                    file["size"].as_u64().unwrap_or_default(),
                    text_of(file, "media_type")?
                );
            }
        }
        ChatCommand::Read { chat, from } => {
            let result = client::command(&credential, CommandCall::ChatRead { chat, from }).await?;
            for message in result.as_array().context("no Messages in the answer")? {
                let author = match message["author"]["kind"].as_str().unwrap_or_default() {
                    "bot" => "Bot",
                    _ => "User",
                };
                let text = text_of(message, "text")?;
                let seq = message["seq"].as_str().unwrap_or_default();
                // A File alone is a Message, so the line can end at the colon.
                println!(
                    "{}",
                    if text.is_empty() {
                        format!("[{seq}] {author}:")
                    } else {
                        format!("[{seq}] {author}: {text}")
                    }
                );
                if let Some(file) = message["file"].as_str() {
                    println!("  file {file}");
                }
            }
        }
    }

    Ok(())
}

/// Returns the Chat a command acts on: the one named, the Bot's, or the only one.
async fn resolve_chat(credential: &Credential, chat: Option<Id>, bot: Option<Id>) -> Result<Id> {
    match (chat, bot) {
        (Some(chat), _) => Ok(chat),
        (None, Some(bot)) => {
            let shown = client::command(credential, CommandCall::BotShow { bot }).await?;
            id_of(&shown["chat"], "id")
        }
        (None, None) => {
            let chats = client::command(credential, CommandCall::ChatList).await?;
            let chats = chats.as_array().context("no Chats in the answer")?;
            match chats.as_slice() {
                [chat] => id_of(chat, "id"),
                [] => bail!("this Space has no Chat; create a Bot first"),
                _ => bail!("pass --chat or --bot; this Space has several Chats"),
            }
        }
    }
}

async fn file(command: FileCommand) -> Result<()> {
    let credential = Credential::load()?;
    match command {
        FileCommand::Get { file, out } => {
            let (name, bytes) = client::download_file(&credential, file).await?;
            // An explicit file path is the destination; only an implicit one
            // needs the name the Space gave the File.
            let path = match &out {
                Some(path) if !path.is_dir() => path.clone(),
                Some(directory) => directory.join(space_file_name(name.as_deref())?),
                None => std::path::PathBuf::from(space_file_name(name.as_deref())?),
            };
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("cannot create {}", parent.display()))?;
            }
            // An implicit destination never replaces a file the User already has.
            if out.is_none() && path.exists() {
                bail!(
                    "{} already exists; pass --out with the path you want",
                    path.display()
                );
            }
            std::fs::write(&path, &bytes)
                .with_context(|| format!("cannot write {}", path.display()))?;
            let shown = std::fs::canonicalize(&path).unwrap_or(path);
            println!("Wrote {} ({} bytes).", shown.display(), bytes.len());
        }
    }

    Ok(())
}

/// Reads the name the Space gave a File, as one path component.
///
/// Only an implicit destination, or a `--out` directory, needs this name.
fn space_file_name(name: Option<&str>) -> Result<String> {
    let Some(name) = name else {
        bail!("the Space did not name the File; pass --out with the path you want");
    };
    if !is_one_component(name) {
        bail!(
            "the Space named the File {name:?}, which is not a file name; pass --out with the path you want"
        );
    }

    Ok(name.to_owned())
}

/// Whether a name can be one path component, wherever this CLI runs.
fn is_one_component(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\', '\0'])
}

async fn run(command: RunCommand) -> Result<()> {
    let credential = Credential::load()?;
    match command {
        RunCommand::Show { run } => {
            let result = client::command(&credential, CommandCall::RunShow { run }).await?;
            let stdout = std::io::stdout();
            write_run(&mut stdout.lock(), &result)?;
        }
    }

    Ok(())
}

/// Prints one run the way `run show` reports it.
fn write_run(out: &mut impl Write, result: &Value) -> Result<()> {
    writeln!(
        out,
        "Run {}: {}",
        id_of(result, "id")?,
        text_of(result, "status")?
    )?;
    if let Some(position) = result["position"].as_u64() {
        writeln!(out, "  position {position}")?;
    }
    if let Some(approval) = result["approval"].as_str() {
        writeln!(out, "  approval {approval}")?;
    }
    if let Some(reply) = result["reply"].as_str() {
        writeln!(out, "  reply {reply}")?;
    }
    if let Some(error) = result["error"].as_str() {
        writeln!(out, "  error {error}")?;
    }
    if !result["usage"].is_null() {
        writeln!(out, "  usage {}", result["usage"])?;
    }
    if let Some(steps) = result["steps"].as_array() {
        for step in steps {
            writeln!(
                out,
                "  step {}: {} tool call(s)",
                step["index"], step["tool_calls"]
            )?;
        }
    }
    if let Some(actions) = result["actions"].as_array() {
        for (index, action) in actions.iter().enumerate() {
            writeln!(
                out,
                "  action {} (step {}): {} ({})",
                index + 1,
                action["step"].as_u64().unwrap_or_default(),
                action["tool"].as_str().unwrap_or("?"),
                action["status"].as_str().unwrap_or("?")
            )?;
            if let Some(output) = action["output"].as_str() {
                for line in output.lines().take(8) {
                    writeln!(out, "    {line}")?;
                }
            }
        }
    }

    Ok(())
}

/// Answers one approval and prints what the Space decided.
async fn decide(credential: &Credential, approval: Id, allow: bool, always: bool) -> Result<()> {
    let result = client::command(
        credential,
        CommandCall::ApprovalDecide {
            approval,
            allow,
            lasting: always,
        },
    )
    .await?;
    let run = result.get("run").and_then(Value::as_str);
    println!("{}", decision_line(allow, approval, run));

    Ok(())
}

/// The line that reports one answered approval.
fn decision_line(allow: bool, approval: Id, run: Option<&str>) -> String {
    let verb = if allow { "Allowed" } else { "Denied" };
    match run {
        Some(run) => format!("{verb} {approval} for run {run}."),
        None => format!("{verb} {approval}."),
    }
}

/// Lists the approvals waiting for the caller.
async fn approval(command: ApprovalCommand) -> Result<()> {
    let credential = Credential::load()?;
    match command {
        ApprovalCommand::List => {
            let list =
                client::command(&credential, CommandCall::ApprovalList { chat: None }).await?;
            for row in approval_rows(&list)? {
                println!("{row}");
            }
        }
        ApprovalCommand::Show { approval } => {
            let list =
                client::command(&credential, CommandCall::ApprovalList { chat: None }).await?;
            let found = find_entity(&list, "approval", approval)?
                .with_context(|| format!("there is no pending approval {approval}"))?;
            println!("{}", approval_row(&found)?);
            write_approval_details(&mut std::io::stdout().lock(), &found)?;
        }
        ApprovalCommand::Allow { approval, always } => {
            decide(&credential, approval, true, always).await?;
        }
        ApprovalCommand::Deny { approval } => {
            decide(&credential, approval, false, false).await?;
        }
    }

    Ok(())
}

/// One row per pending approval: approval, run, bot, tool, reason, summary.
fn approval_rows(list: &Value) -> Result<Vec<String>> {
    list.as_array()
        .context("no approvals in the answer")?
        .iter()
        .map(approval_row)
        .collect()
}

/// Formats one pending approval as a single line.
fn approval_row(approval: &Value) -> Result<String> {
    Ok(format!(
        "{}  {}  {}  {}  {}  {}",
        entity_id(approval, &["approval"])?,
        entity_id(approval, &["run"])?,
        entity_id(approval, &["bot"])?,
        text_of(approval, "tool")?,
        text_of(approval, "reason")?,
        approval["summary"].as_str().unwrap_or_default()
    ))
}

/// Prints where one approval sits in its run.
fn write_approval_details(out: &mut impl Write, approval: &Value) -> Result<()> {
    if let Some(step) = approval["step"].as_u64() {
        writeln!(out, "  step {step}")?;
    }
    if let Some(action) = approval["action"].as_str() {
        writeln!(out, "  action {action}")?;
    }
    if let Some(chat) = approval["chat"].as_str() {
        writeln!(out, "  chat {chat}")?;
    }
    if let Some(rule) = approval["rule"].as_str() {
        writeln!(out, "  rule {rule}")?;
    }

    Ok(())
}

/// Adds or removes approval rules.
async fn rule(command: RuleCommand) -> Result<()> {
    let credential = Credential::load()?;
    match command {
        RuleCommand::List { bot } => {
            let list = client::command(&credential, CommandCall::RuleList { bot }).await?;
            for row in rule_rows(&list)? {
                println!("{row}");
            }
        }
        RuleCommand::Add {
            bot,
            effect,
            tool,
            command_prefix,
            path_prefix,
        } => {
            validate_rule_arguments(&tool, command_prefix.as_deref(), path_prefix.as_deref())?;
            let result = client::command(
                &credential,
                CommandCall::RuleAdd {
                    bot,
                    effect: effect.into(),
                    tool: tool.clone(),
                    command_prefix: command_prefix.clone(),
                    path_prefix: path_prefix.clone(),
                },
            )
            .await?;
            let rule = entity_id(&result, &["rule", "id"])?;
            println!(
                "{}",
                rule_add_line(
                    rule,
                    bot,
                    effect.wire(),
                    &tool,
                    command_prefix.as_deref(),
                    path_prefix.as_deref()
                )
            );
        }
        RuleCommand::Remove { rule } => {
            let result = client::command(&credential, CommandCall::RuleRemove { rule }).await?;
            let shown = result
                .get("rule")
                .and_then(Value::as_str)
                .map_or_else(|| rule.to_string(), str::to_owned);
            println!("Removed rule {shown}.");
        }
    }

    Ok(())
}

/// One row per rule: rule, bot, effect, tool, and the matching prefix.
fn rule_rows(list: &Value) -> Result<Vec<String>> {
    list.as_array()
        .context("no rules in the answer")?
        .iter()
        .map(rule_row)
        .collect()
}

/// Formats one rule as a single line.
fn rule_row(rule: &Value) -> Result<String> {
    let mut row = format!(
        "{}  {}  {}  {}",
        entity_id(rule, &["rule", "id"])?,
        entity_id(rule, &["bot"])?,
        text_of(rule, "effect")?,
        text_of(rule, "tool")?
    );
    if let Some(prefix) = rule["command_prefix"].as_str() {
        row.push_str(&format!("  command-prefix {prefix}"));
    }
    if let Some(prefix) = rule["path_prefix"].as_str() {
        row.push_str(&format!("  path-prefix {prefix}"));
    }

    Ok(row)
}

/// The line that reports one added rule.
fn rule_add_line(
    rule: Id,
    bot: Id,
    effect: &str,
    tool: &str,
    command_prefix: Option<&str>,
    path_prefix: Option<&str>,
) -> String {
    let mut line = format!("Added rule {rule} for Bot {bot}: {effect} {tool}");
    if let Some(prefix) = command_prefix {
        line.push_str(&format!(" (command-prefix {prefix:?})"));
    }
    if let Some(prefix) = path_prefix {
        line.push_str(&format!(" (path-prefix {prefix:?})"));
    }

    line
}

/// Refuses a prefix that cannot match the tool it names.
///
/// The Space validates the rule too; this check turns the refusal into a
/// local error before a request is sent, because a prefix on the wrong tool
/// can only ever match nothing.
fn validate_rule_arguments(
    tool: &str,
    command_prefix: Option<&str>,
    path_prefix: Option<&str>,
) -> Result<()> {
    if command_prefix.is_some() && path_prefix.is_some() {
        bail!("pass at most one of --command-prefix and --path-prefix");
    }
    let tool = tool.trim();
    if tool.is_empty() {
        bail!("--tool must not be empty");
    }
    if let Some(prefix) = command_prefix {
        if tool != "shell" {
            bail!("--command-prefix matches shell actions; {tool} is not shell");
        }
        if prefix.trim().is_empty() {
            bail!("--command-prefix must not be empty");
        }
    }
    if let Some(prefix) = path_prefix {
        if tool == "shell" {
            bail!("--path-prefix matches file actions; shell uses --command-prefix");
        }
        if !matches!(tool, "read_file" | "write_file" | "list_files") {
            bail!(
                "--path-prefix matches file actions (read_file, write_file, list_files); \
                 {tool} is not one"
            );
        }
        if prefix.trim().is_empty() {
            bail!("--path-prefix must not be empty");
        }
    }

    Ok(())
}

/// Returns the identifier in one of the candidate fields.
fn entity_id(value: &Value, fields: &[&str]) -> Result<Id> {
    for field in fields {
        if value.get(*field).is_some() {
            return id_of(value, field);
        }
    }

    bail!("the answer has none of {}", fields.join(", "))
}

/// Returns the entry with this identifier, when the answer has one.
fn find_entity(list: &Value, field: &str, id: Id) -> Result<Option<Value>> {
    let wanted = id.to_string();

    Ok(list
        .as_array()
        .context("no entries in the answer")?
        .iter()
        .find(|entry| entry[field].as_str() == Some(wanted.as_str()))
        .cloned())
}

/// One approval as the interactive prompt shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ApprovalCard {
    /// The approval identifier.
    approval: Id,
    /// The tool that asked.
    tool: String,
    /// The short text the Space wrote.
    summary: String,
    /// The reason the Space gave.
    reason: String,
}

impl ApprovalCard {
    /// Reads one card from the Space's approval view.
    fn from_view(approval: &Value) -> Result<Self> {
        Ok(Self {
            approval: entity_id(approval, &["approval"])?,
            tool: text_of(approval, "tool")?.to_owned(),
            summary: approval["summary"].as_str().unwrap_or_default().to_owned(),
            reason: text_of(approval, "reason")?.to_owned(),
        })
    }

    /// Returns whether the Space would accept a lasting answer.
    fn allows_always(&self) -> bool {
        self.reason == "rule"
    }

    /// Returns the reason as the card reads it.
    fn reason_text(&self) -> String {
        match self.reason.as_str() {
            "first_use_local" => "first use of your Local Computer".to_owned(),
            "rule" => "a rule you set".to_owned(),
            other => other.to_owned(),
        }
    }

    /// Returns the keys the prompt offers.
    fn options(&self) -> &'static str {
        if self.allows_always() {
            "[a]llow / [A]lways allow / [d]eny / [q]uit"
        } else {
            "[a]llow / [d]eny / [q]uit"
        }
    }

    /// Returns the answers the prompt accepts.
    fn answers(&self) -> &'static str {
        if self.allows_always() {
            "a, A, d"
        } else {
            "a, d"
        }
    }

    /// Renders the approval card; the prompt is printed by the reader.
    fn render(&self) -> String {
        let summary = if self.summary.is_empty() {
            String::new()
        } else {
            format!("\n  {}", self.summary)
        };

        format!(
            "Approval {} for {}{}\n  reason: {}",
            self.approval,
            self.tool,
            summary,
            self.reason_text()
        )
    }
}

/// The answer a User gave to a card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decision {
    /// Allow this one action.
    Allow,
    /// Allow this kind of action from now on.
    AlwaysAllow,
    /// Deny the action; the run stops.
    Deny,
    /// Leave the run waiting.
    Quit,
}

/// Maps one line of input to an answer.
fn decision_from_input(line: &str, always_available: bool) -> Option<Decision> {
    match line.trim() {
        "a" => Some(Decision::Allow),
        "A" if always_available => Some(Decision::AlwaysAllow),
        "d" => Some(Decision::Deny),
        "q" => Some(Decision::Quit),
        _ => None,
    }
}

/// Asks until the User gives an answer the card offers.
fn read_decision(
    card: &ApprovalCard,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<Decision> {
    loop {
        write!(output, "{} ", card.options())?;
        output.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            // A terminal that closed cannot answer; leave the run waiting.
            return Ok(Decision::Quit);
        }
        if let Some(decision) = decision_from_input(&line, card.allows_always()) {
            return Ok(decision);
        }
        writeln!(output, "answer {}; q quits.", card.answers())?;
    }
}

/// The line that tells a script how to answer a waiting run.
fn waiting_advice(run: Id, approval: Id) -> String {
    format!(
        "run {run} is waiting for approval {approval}; answer with `neha approval allow {approval}`"
    )
}

/// What following a run stopped at.
enum Follow {
    /// The run ended; this is its last view.
    Finished(Value),
    /// The run still waits for an approval.
    Waiting,
}

/// Follows one run, answering approvals on a terminal and reporting them to a script.
async fn follow_run(
    credential: &Credential,
    chat: Id,
    run: Id,
    progress: &mut Progress,
) -> Result<Follow> {
    loop {
        let view = client::wait_for_run(credential, run, |view| progress.observed(view)).await?;
        if view["status"].as_str() != Some("waiting") {
            return Ok(Follow::Finished(view));
        }
        let approval = id_of(&view, "approval")?;
        if !std::io::stdin().is_terminal() {
            println!("{}", waiting_advice(run, approval));
            return Ok(Follow::Waiting);
        }
        let card = approval_card(credential, chat, approval).await?;
        println!("{}", card.render());
        let decision = {
            let stdin = std::io::stdin();
            let mut input = stdin.lock();
            let stdout = std::io::stdout();
            let mut output = stdout.lock();
            read_decision(&card, &mut input, &mut output)?
        };
        let (allow, lasting) = match decision {
            Decision::Quit => {
                println!("{}", waiting_advice(run, approval));
                return Ok(Follow::Waiting);
            }
            Decision::Allow => (true, false),
            Decision::AlwaysAllow => (true, true),
            Decision::Deny => (false, false),
        };
        client::command(
            credential,
            CommandCall::ApprovalDecide {
                approval,
                allow,
                lasting,
            },
        )
        .await?;
    }
}

/// Reads one waiting approval's card from the Space.
async fn approval_card(credential: &Credential, chat: Id, approval: Id) -> Result<ApprovalCard> {
    let list = client::command(credential, CommandCall::ApprovalList { chat: Some(chat) }).await?;
    let found = find_entity(&list, "approval", approval)?
        .with_context(|| format!("approval {approval} is no longer waiting"))?;

    ApprovalCard::from_view(&found)
}

/// Reads the text of the Message a finished run replied with.
async fn reply_text(credential: &Credential, chat: Id, run: &Value) -> Result<String> {
    let reply = run["reply"]
        .as_str()
        .context("the run has no reply Message")?;
    let messages = client::command(credential, CommandCall::ChatRead { chat, from: None }).await?;
    let message = messages
        .as_array()
        .context("no Messages in the answer")?
        .iter()
        .find(|message| message["id"].as_str() == Some(reply))
        .with_context(|| format!("the reply Message {reply} is not in the Chat"))?;

    Ok(text_of(message, "text")?.to_owned())
}

/// Prints a run's progress while the CLI waits for it to end.
struct Progress {
    /// Whether to print the run's steps and actions as they appear.
    follow: bool,
    /// The queue place printed last, if the run was queued.
    queued: Option<u64>,
    /// Whether the start of the run was printed.
    running: bool,
    /// How many steps were printed.
    steps: usize,
    /// The status printed last for each action.
    actions: HashMap<String, String>,
}

impl Progress {
    fn new(follow: bool) -> Self {
        Self {
            follow,
            queued: None,
            running: false,
            steps: 0,
            actions: HashMap::new(),
        }
    }

    /// Prints what the command's own result says about the run.
    fn accepted(&mut self, result: &Value) {
        match result["status"].as_str() {
            Some("queued") => {
                let position = result["position"].as_u64().unwrap_or(1);
                println!("queued (position {position})");
                self.queued = Some(position);
            }
            Some("running") => {
                println!("running");
                self.running = true;
            }
            _ => {}
        }
    }

    /// Prints what a poll of the run shows that the CLI has not shown yet.
    fn observed(&mut self, view: &Value) {
        match view["status"].as_str() {
            Some("queued") => {
                let position = view["position"]
                    .as_u64()
                    .unwrap_or_else(|| self.queued.unwrap_or(1));
                if self.queued != Some(position) {
                    println!("waiting (position {position})");
                    self.queued = Some(position);
                }
            }
            Some("running") if !self.running => {
                println!("running");
                self.running = true;
            }
            _ => {}
        }
        if self.follow {
            self.show_progress(view);
        }
    }

    /// Prints the steps and actions the run recorded since the last poll.
    fn show_progress(&mut self, view: &Value) {
        if let Some(steps) = view["steps"].as_array() {
            for step in steps.iter().skip(self.steps) {
                eprintln!(
                    "  step {}: {} tool call(s)",
                    step["index"], step["tool_calls"]
                );
            }
            self.steps = steps.len();
        }
        if let Some(actions) = view["actions"].as_array() {
            for action in actions {
                let id = action["id"].as_str().unwrap_or_default().to_owned();
                let status = action["status"].as_str().unwrap_or("unknown");
                if self.actions.get(&id).is_some_and(|shown| shown == status) {
                    continue;
                }
                // An action is still running until its final status is known.
                let shown = if status == "unknown" {
                    "running"
                } else {
                    status
                };
                eprintln!(
                    "  action {} (step {}): {shown}",
                    action["tool"].as_str().unwrap_or("?"),
                    action["step"]
                );
                self.actions.insert(id, status.to_owned());
            }
        }
    }
}

/// The line that tells the User how to pair this PC, when a ticket is waiting.
///
/// A Space with no Admin yet has no ticket, so there is nothing to pair with
/// until someone joins; the hint is printed only once a ticket exists.
fn pairing_hint(local_ticket: Option<&std::path::Path>) -> Option<&'static str> {
    local_ticket.map(|_| "Pair this PC: neha computer pair")
}

/// Parses the value of `--computer`: `local`, `hosted`, or a Computer id.
fn parse_bot_computer(text: &str) -> Result<BotComputer> {
    let text = text.trim();
    match text {
        "" => bail!("--computer needs local, hosted, or a Computer id"),
        "local" => Ok(BotComputer::Local),
        "hosted" => Ok(BotComputer::Hosted),
        _ => match text.parse::<Id>() {
            Ok(computer) => Ok(BotComputer::Id(computer)),
            Err(_) => bail!("--computer {text:?} is not local, hosted, or a Computer id"),
        },
    }
}

/// The User-facing name of a Computer kind.
fn computer_kind_label(kind: &str) -> &str {
    match kind {
        "local" => "Local",
        "hosted" => "Hosted",
        other => other,
    }
}

fn provider_key() -> Result<String> {
    if let Ok(key) = std::env::var("NEHA_PROVIDER_KEY")
        && !key.trim().is_empty()
    {
        return Ok(key);
    }

    rpassword::prompt_password("Provider API key: ").context("cannot read the API key")
}

fn read_password() -> Result<String> {
    if let Ok(password) = std::env::var("NEHA_PASSWORD")
        && !password.is_empty()
    {
        return Ok(password);
    }

    rpassword::prompt_password("Password: ").context("cannot read the password")
}

fn id_of(value: &Value, field: &str) -> Result<Id> {
    value
        .get(field)
        .and_then(Value::as_str)
        .with_context(|| format!("the answer has no {field}"))?
        .parse()
        .with_context(|| format!("the answer's {field} is not an Id"))
}

fn text_of<'a>(value: &'a Value, field: &str) -> Result<&'a str> {
    value
        .get(field)
        .and_then(Value::as_str)
        .with_context(|| format!("the answer has no {field}"))
}

#[cfg(test)]
mod tests {
    use neha::id::Id;
    use serde_json::json;

    use super::{space_file_name, write_run};

    use clap::Parser as _;

    use super::{Cli, ComputerCommand, Top, computer_kind_label, pairing_hint, parse_bot_computer};

    use super::{
        ApprovalCard, ApprovalCommand, Decision, RuleCommand, RuleEffectChoice, approval_row,
        decision_from_input, decision_line, read_decision, rule_add_line, rule_rows,
        validate_rule_arguments, waiting_advice, write_approval_details,
    };

    /// Parses a CLI command line.
    fn parse(args: &[&str]) -> Top {
        Cli::try_parse_from(std::iter::once("neha").chain(args.iter().copied()))
            .unwrap()
            .command
    }

    #[test]
    fn the_bot_computer_choice_parses_local_hosted_and_an_id() {
        assert_eq!(
            parse_bot_computer("local").unwrap(),
            neha::surface::BotComputer::Local
        );
        assert_eq!(
            parse_bot_computer("hosted").unwrap(),
            neha::surface::BotComputer::Hosted
        );
        let computer = Id::new();
        assert_eq!(
            parse_bot_computer(&computer.to_string()).unwrap(),
            neha::surface::BotComputer::Id(computer)
        );
        for value in ["", "managed", "real", "not-a-computer"] {
            let error = parse_bot_computer(value).unwrap_err().to_string();
            assert!(
                error.contains("local, hosted, or a Computer id"),
                "{value:?}: {error}"
            );
        }
    }

    #[test]
    fn the_pairing_hint_waits_for_a_ticket() {
        assert_eq!(
            pairing_hint(Some(std::path::Path::new(
                "/home/ada/.config/neha/local-computer.ticket"
            ))),
            Some("Pair this PC: neha computer pair")
        );
        assert_eq!(pairing_hint(None), None);
    }

    #[test]
    fn a_computer_kind_reads_local_and_hosted() {
        assert_eq!(computer_kind_label("local"), "Local");
        assert_eq!(computer_kind_label("hosted"), "Hosted");
        assert_eq!(computer_kind_label("future"), "future");
    }

    #[test]
    fn the_pair_command_takes_the_new_flags() {
        let cli = Cli::try_parse_from([
            "neha",
            "computer",
            "pair",
            "--space",
            "http://127.0.0.1:7337",
            "--ticket-file",
            "/home/ada/.config/neha/local-computer.ticket",
            "--workspace",
            "/home/ada/.neha/workspaces",
            "--yes",
        ])
        .unwrap();
        match cli.command {
            Top::Computer {
                command:
                    ComputerCommand::Pair {
                        space,
                        ticket_file,
                        workspace,
                        yes,
                    },
            } => {
                assert_eq!(space.as_deref(), Some("http://127.0.0.1:7337"));
                assert_eq!(
                    ticket_file.as_deref(),
                    Some(std::path::Path::new(
                        "/home/ada/.config/neha/local-computer.ticket"
                    ))
                );
                assert_eq!(
                    workspace.as_deref(),
                    Some(std::path::Path::new("/home/ada/.neha/workspaces"))
                );
                assert!(yes);
            }
            _ => panic!("the pair command did not parse"),
        }
    }

    #[test]
    fn the_machine_name_flag_is_gone_and_run_takes_a_workspace() {
        assert!(
            Cli::try_parse_from(["neha", "computer", "pair", "--name", "laptop"]).is_err(),
            "--name is no longer part of pairing"
        );
        let cli = Cli::try_parse_from([
            "neha",
            "computer",
            "run",
            "--workspace",
            "/home/ada/.neha/workspaces",
        ])
        .unwrap();
        match cli.command {
            Top::Computer {
                command: ComputerCommand::Run { workspace },
            } => assert_eq!(
                workspace.as_deref(),
                Some(std::path::Path::new("/home/ada/.neha/workspaces"))
            ),
            _ => panic!("the run command did not parse"),
        }
    }

    /// One run as `run show` reports it.
    fn run_view(status: &str, position: Option<u32>) -> serde_json::Value {
        let mut view = json!({
            "id": Id::new(),
            "chat": Id::new(),
            "bot": Id::new(),
            "acting": Id::new(),
            "command": Id::new(),
            "message": Id::new(),
            "status": status,
            "reply": null,
            "error": null,
            "usage": null,
            "steps": [],
            "actions": [],
        });
        if let Some(position) = position {
            view["position"] = position.into();
        }
        view
    }

    #[test]
    fn a_queued_run_shows_its_place_and_a_running_one_does_not() {
        let queued = run_view("queued", Some(2));
        let mut shown = Vec::new();
        write_run(&mut shown, &queued).unwrap();
        let shown = String::from_utf8(shown).unwrap();
        assert!(shown.contains("queued"), "{shown}");
        assert!(shown.contains("  position 2"), "{shown}");

        let running = run_view("running", None);
        let mut shown = Vec::new();
        write_run(&mut shown, &running).unwrap();
        let shown = String::from_utf8(shown).unwrap();
        assert!(!shown.contains("position"), "{shown}");
    }

    #[test]
    fn a_space_name_is_a_file_name_only_when_it_is_one_path_component() {
        assert_eq!(space_file_name(Some("报告 v2.txt")).unwrap(), "报告 v2.txt");
        for name in [
            None,
            Some(""),
            Some("."),
            Some(".."),
            Some("a/b"),
            Some("a\\b"),
            Some("a\0b"),
        ] {
            assert!(space_file_name(name).is_err(), "{name:?}");
        }
    }

    #[test]
    fn the_approval_commands_parse() {
        assert!(matches!(
            parse(&["approval", "list"]),
            Top::Approval {
                command: ApprovalCommand::List
            }
        ));
        let approval = Id::new();
        let text = approval.to_string();
        match parse(&["approval", "show", text.as_str()]) {
            Top::Approval {
                command: ApprovalCommand::Show { approval: shown },
            } => assert_eq!(shown, approval),
            _ => panic!("approval show did not parse"),
        }
        match parse(&["approval", "allow", text.as_str(), "--always"]) {
            Top::Approval {
                command:
                    ApprovalCommand::Allow {
                        approval: shown,
                        always,
                    },
            } => {
                assert_eq!(shown, approval);
                assert!(always);
            }
            _ => panic!("approval allow did not parse"),
        }
        match parse(&["approval", "deny", text.as_str()]) {
            Top::Approval {
                command: ApprovalCommand::Deny { approval: shown },
            } => assert_eq!(shown, approval),
            _ => panic!("approval deny did not parse"),
        }
        assert!(Cli::try_parse_from(["neha", "approval", "allow"]).is_err());
    }

    #[test]
    fn the_rule_commands_parse() {
        let bot = Id::new();
        let bot_text = bot.to_string();
        match parse(&["rule", "list", "--bot", bot_text.as_str()]) {
            Top::Rule {
                command: RuleCommand::List { bot: shown },
            } => assert_eq!(shown, Some(bot)),
            _ => panic!("rule list did not parse"),
        }
        match parse(&[
            "rule",
            "add",
            "--bot",
            bot_text.as_str(),
            "--effect",
            "ask",
            "--tool",
            "shell",
            "--command-prefix",
            "git",
        ]) {
            Top::Rule {
                command:
                    RuleCommand::Add {
                        bot: shown,
                        effect,
                        tool,
                        command_prefix,
                        path_prefix,
                    },
            } => {
                assert_eq!(shown, bot);
                assert_eq!(effect, RuleEffectChoice::Ask);
                assert_eq!(tool, "shell");
                assert_eq!(command_prefix.as_deref(), Some("git"));
                assert_eq!(path_prefix, None);
            }
            _ => panic!("rule add did not parse"),
        }
        let rule = Id::new();
        let rule_text = rule.to_string();
        match parse(&["rule", "remove", rule_text.as_str()]) {
            Top::Rule {
                command: RuleCommand::Remove { rule: shown },
            } => assert_eq!(shown, rule),
            _ => panic!("rule remove did not parse"),
        }
        assert!(
            Cli::try_parse_from([
                "neha",
                "rule",
                "add",
                "--bot",
                bot_text.as_str(),
                "--effect",
                "maybe",
                "--tool",
                "shell",
            ])
            .is_err(),
            "an unknown effect is refused"
        );
    }

    #[test]
    fn a_rule_prefix_is_checked_against_its_tool() {
        validate_rule_arguments("shell", None, None).unwrap();
        validate_rule_arguments("shell", Some("git"), None).unwrap();
        validate_rule_arguments("read_file", None, Some("/home/ada")).unwrap();
        validate_rule_arguments("write_file", None, Some(".")).unwrap();

        for (tool, command_prefix, path_prefix, expected) in [
            ("shell", Some("git"), Some("/tmp"), "at most one"),
            ("read_file", Some("git"), None, "--command-prefix"),
            ("shell", None, Some("/tmp"), "--path-prefix"),
            ("take_file", None, Some("/tmp"), "--path-prefix"),
            ("shell", Some("  "), None, "must not be empty"),
            ("read_file", None, Some(" "), "must not be empty"),
            ("   ", None, None, "--tool"),
        ] {
            let error = validate_rule_arguments(tool, command_prefix, path_prefix)
                .unwrap_err()
                .to_string();
            assert!(error.contains(expected), "{tool:?}: {error}");
        }
    }

    #[test]
    fn a_card_hides_always_allow_until_a_rule_asked() {
        let rule = ApprovalCard {
            approval: Id::new(),
            tool: "shell".to_owned(),
            summary: "printf approved > proof.txt".to_owned(),
            reason: "rule".to_owned(),
        };
        let rendered = rule.render();
        assert!(rendered.contains("Approval "), "{rendered}");
        assert!(rendered.contains("for shell"), "{rendered}");
        assert!(
            rendered.contains("printf approved > proof.txt"),
            "{rendered}"
        );
        assert!(rendered.contains("reason: a rule you set"), "{rendered}");
        assert_eq!(rule.options(), "[a]llow / [A]lways allow / [d]eny / [q]uit");

        let first_use = ApprovalCard {
            reason: "first_use_local".to_owned(),
            ..rule
        };
        let rendered = first_use.render();
        assert!(
            rendered.contains("reason: first use of your Local Computer"),
            "{rendered}"
        );
        // The first use of a Local Computer cannot be covered by a lasting
        // rule, so its prompt does not offer Always allow.
        assert_eq!(first_use.options(), "[a]llow / [d]eny / [q]uit");
        assert!(!first_use.options().contains("Always allow"));
    }

    #[test]
    fn a_decision_letter_maps_to_an_approval_answer() {
        for always in [false, true] {
            assert_eq!(decision_from_input("a\n", always), Some(Decision::Allow));
            assert_eq!(decision_from_input(" a ", always), Some(Decision::Allow));
            assert_eq!(decision_from_input("d\n", always), Some(Decision::Deny));
            assert_eq!(decision_from_input("q\n", always), Some(Decision::Quit));
        }
        assert_eq!(
            decision_from_input("A\n", true),
            Some(Decision::AlwaysAllow)
        );
        // The first use of a Local Computer cannot be covered by a rule, so
        // the card does not offer Always allow there.
        assert_eq!(decision_from_input("A\n", false), None);
        for line in ["", "yes", "D", "Q", "aa"] {
            assert_eq!(decision_from_input(line, true), None, "{line:?}");
        }
    }

    #[test]
    fn a_card_reprompts_until_the_answer_is_offered() {
        let card = ApprovalCard {
            approval: Id::new(),
            tool: "shell".to_owned(),
            summary: "ls".to_owned(),
            reason: "rule".to_owned(),
        };
        let mut input = std::io::Cursor::new(b"x\nA\n");
        let mut output = Vec::new();
        let decision = read_decision(&card, &mut input, &mut output).unwrap();
        assert_eq!(decision, Decision::AlwaysAllow);
        let output = String::from_utf8(output).unwrap();
        assert_eq!(output.matches("[a]llow / [A]lways allow").count(), 2);
        assert!(output.contains("answer a, A, d; q quits."), "{output}");

        // A terminal that closed cannot answer.
        let mut input = std::io::Cursor::new(b"");
        let mut output = Vec::new();
        assert_eq!(
            read_decision(&card, &mut input, &mut output).unwrap(),
            Decision::Quit
        );

        // Always allow is not a valid answer for the first use.
        let first_use = ApprovalCard {
            reason: "first_use_local".to_owned(),
            ..card
        };
        let mut input = std::io::Cursor::new(b"A\nq\n");
        let mut output = Vec::new();
        assert_eq!(
            read_decision(&first_use, &mut input, &mut output).unwrap(),
            Decision::Quit
        );
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("answer a, d; q quits."), "{output}");
    }

    #[test]
    fn a_waiting_run_names_the_command_to_answer_it() {
        let run = Id::new();
        let approval = Id::new();
        assert_eq!(
            waiting_advice(run, approval),
            format!(
                "run {run} is waiting for approval {approval}; answer with `neha approval allow {approval}`"
            )
        );
    }

    #[test]
    fn approval_and_rule_rows_show_the_pinned_columns() {
        let approval = Id::new();
        let run = Id::new();
        let bot = Id::new();
        let action = Id::new();
        let chat = Id::new();
        let view = json!({
            "approval": approval,
            "run": run,
            "bot": bot,
            "action": action,
            "chat": chat,
            "step": 2,
            "tool": "shell",
            "reason": "first_use_local",
            "summary": "printf approved > proof.txt",
        });
        assert_eq!(
            approval_row(&view).unwrap(),
            format!(
                "{approval}  {run}  {bot}  shell  first_use_local  printf approved > proof.txt"
            )
        );
        let mut details = Vec::new();
        write_approval_details(&mut details, &view).unwrap();
        let details = String::from_utf8(details).unwrap();
        assert!(details.contains("  step 2"), "{details}");
        assert!(details.contains(&format!("  action {action}")), "{details}");
        assert!(details.contains(&format!("  chat {chat}")), "{details}");

        let rule = Id::new();
        let list = json!([{
            "rule": rule,
            "bot": bot,
            "effect": "ask",
            "tool": "shell",
            "command_prefix": "git",
            "path_prefix": null,
        }]);
        assert_eq!(
            rule_rows(&list).unwrap(),
            vec![format!("{rule}  {bot}  ask  shell  command-prefix git")]
        );
        assert_eq!(rule_rows(&json!([])).unwrap(), Vec::<String>::new());
        assert_eq!(
            rule_add_line(rule, bot, "ask", "shell", Some("git"), None),
            format!("Added rule {rule} for Bot {bot}: ask shell (command-prefix \"git\")")
        );
        assert_eq!(
            rule_add_line(rule, bot, "allow", "read_file", None, Some("/tmp")),
            format!("Added rule {rule} for Bot {bot}: allow read_file (path-prefix \"/tmp\")")
        );
        assert_eq!(
            decision_line(true, approval, Some(&run.to_string())),
            format!("Allowed {approval} for run {run}.")
        );
        assert_eq!(
            decision_line(false, approval, None),
            format!("Denied {approval}.")
        );
    }

    #[test]
    fn a_waiting_run_shows_its_pending_approval() {
        let mut view = run_view("waiting", None);
        let approval = Id::new();
        view["approval"] = json!(approval);
        let mut shown = Vec::new();
        write_run(&mut shown, &view).unwrap();
        let shown = String::from_utf8(shown).unwrap();
        assert!(shown.contains(&format!("  approval {approval}")), "{shown}");
        assert!(shown.contains("Run "), "{shown}");
    }
}
