//! `neha` runs a Space and talks to its Bots from a terminal.

mod client;
mod computer;

use std::collections::HashMap;
use std::io::Write;

use anyhow::{Context as _, Result, bail};
use clap::{ArgGroup, Parser, Subcommand};
use neha::id::Id;
use neha::seq::Seq;
use neha::surface::{
    BotCreate, CommandCall, MessageSend, ModelSpec, ProviderAdd, ProviderProtocol,
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
        /// The real Computer the Bot works on; a managed one when absent.
        #[arg(long)]
        computer: Option<Id>,
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
    /// Pair this machine to a Space and start its daemon.
    Pair {
        /// The Space URL; defaults to NEHA_SPACE_URL.
        #[arg(long)]
        space: Option<String>,
        /// The directory that holds one folder per Bot.
        #[arg(long)]
        workspace: Option<std::path::PathBuf>,
        /// The machine name the Space shows.
        #[arg(long)]
        name: Option<String>,
    },
    /// Start this machine's daemon with its saved pairing.
    Run,
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
                text_of(&result["computer"], "kind")?,
                result["computer"]["online"].as_bool().unwrap_or(false)
            );
            println!(
                "  Workspace: {}",
                text_of(&result["computer"], "workspace")?
            );
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
            workspace,
            name,
        } => computer::pair(space, workspace, name).await?,
        ComputerCommand::Run => computer::run().await?,
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
            let view =
                client::wait_for_run(&credential, run, |view| progress.observed(view)).await?;
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
}
