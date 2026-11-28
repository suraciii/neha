//! `neha` runs a Space and talks to its Bots from a terminal.

mod client;
mod computer;

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal as _, Write};

use anyhow::{Context as _, Result, anyhow, bail};
use clap::{ArgGroup, Parser, Subcommand};
use neha::id::Id;
use neha::record::{ComputerSharing, RuleEffect};
use neha::seq::Seq;
use neha::surface::{
    BotComputer, BotCreate, BotUpdate, CommandCall, ComputerCreate, ComputerUpdate, MessageSend,
    ModelSpec, ProviderAdd, ProviderConnect, ProviderProtocol, ThinkingLevel,
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
        /// The public domain users reach through a TLS proxy, such as
        /// neha.example.com; defaults to NEHA_DOMAIN. Use NEHA_PUBLIC_URL for
        /// a full https origin.
        #[arg(long)]
        domain: Option<String>,
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
    /// Browse and refresh the models.dev catalog.
    Catalog {
        #[command(subcommand)]
        command: CatalogCommand,
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
    /// Connect a Provider.
    Add {
        /// The Provider's base URL, such as http://127.0.0.1:8000/v1.
        #[arg(long)]
        base_url: String,
        /// The protocol: chat-completions, openai-responses, or anthropic-messages.
        #[arg(long, default_value = "chat-completions", value_parser = parse_protocol)]
        protocol: ProviderProtocol,
        /// A Model name at the Provider; repeat for more, or use --discover.
        #[arg(
            long = "model",
            value_name = "NAME",
            required_unless_present = "discover"
        )]
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
        /// The Model cannot call tools.
        #[arg(long)]
        no_tools: bool,
        /// The Model can think before it answers.
        #[arg(long)]
        thinks: bool,
        /// The Space's default thinking level: minimal, low, medium, high, max.
        #[arg(long, value_parser = parse_thinking)]
        thinking: Option<ThinkingLevel>,
        /// Ask the endpoint for its Models instead of naming them.
        #[arg(long, conflicts_with = "models")]
        discover: bool,
    },
    /// Connect a vendor the catalog knows.
    Connect {
        /// The catalog vendor id, such as openai or deepseek.
        vendor: String,
        /// Override the preset base URL.
        #[arg(long)]
        base_url: Option<String>,
        /// Override the preset protocol.
        #[arg(long, value_parser = parse_protocol)]
        protocol: Option<ProviderProtocol>,
        /// The Space's default thinking level.
        #[arg(long, value_parser = parse_thinking)]
        thinking: Option<ThinkingLevel>,
    },
    /// Refresh a Provider's Models from its endpoint or the catalog.
    Refresh {
        /// The Provider identifier.
        provider: Id,
    },
    /// List connected Providers.
    List,
}

#[derive(Subcommand)]
enum CatalogCommand {
    /// List the Providers the catalog knows.
    List,
    /// Show the catalog cache state.
    Status,
    /// Fetch the catalog from models.dev.
    Refresh,
}

#[derive(Subcommand)]
enum ModelCommand {
    /// List the Models the Space offers.
    List,
    /// Make a Model the Space's default Model.
    Default {
        /// The Model the Space uses by default from now on.
        model: Id,
    },
}

#[derive(Subcommand)]
enum BotCommand {
    /// Create a Bot with its own Chat and Computer.
    Create {
        /// The Bot's name.
        name: String,
        /// Where the Bot works: local, hosted, or a Computer id.
        ///
        /// The owner's Space Computer when omitted; `hosted` creates a new
        /// exclusive Hosted Computer, and an id names an existing Computer.
        #[arg(long, value_name = "LOCAL|HOSTED|ID")]
        computer: Option<String>,
        /// The Bot's thinking level: minimal, low, medium, high, max.
        #[arg(long, value_parser = parse_thinking)]
        thinking: Option<ThinkingLevel>,
        /// The Model the Bot uses; defaults to the Space's default Model.
        #[arg(long)]
        model: Option<Id>,
    },
    /// Change your Bot's Model or thinking level.
    Set {
        /// The Bot identifier.
        bot: Id,
        /// The Model the Bot uses.
        #[arg(long)]
        model: Option<Id>,
        /// The Bot's thinking level: minimal, low, medium, high, max.
        #[arg(long, value_parser = parse_thinking)]
        thinking: Option<ThinkingLevel>,
        /// Use the Space's default Model again.
        #[arg(long)]
        reset_model: bool,
        /// Use the Space's default thinking level again.
        #[arg(long)]
        reset_thinking: bool,
    },
    /// Move your Bot to another Computer you own.
    Move {
        /// The Bot identifier.
        bot: Id,
        /// The Computer it should work on.
        #[arg(long)]
        computer: Id,
    },
    /// Rename your Bot.
    Rename {
        /// The Bot identifier.
        bot: Id,
        /// Its new name.
        name: String,
    },
    /// Show your Bot's file access.
    Access {
        /// The Bot identifier.
        bot: Id,
    },
    /// Grant your Bot write access to another folder.
    Grant {
        /// The Bot identifier.
        bot: Id,
        /// The folder the Bot may also write; it must already exist.
        path: std::path::PathBuf,
    },
    /// Take one granted folder away from your Bot.
    Revoke {
        /// The Bot identifier.
        bot: Id,
        /// The granted folder to take away.
        path: std::path::PathBuf,
    },
    /// Turn your Bot's full access on or off.
    FullAccess {
        /// The Bot identifier.
        bot: Id,
        /// On removes the sandbox; off restores the granted folders fence.
        #[arg(action = clap::ArgAction::Set, value_parser = parse_full_access)]
        full_access: bool,
        /// Turn full access on without asking for confirmation.
        #[arg(long)]
        yes: bool,
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
    /// Create a Hosted Computer you own.
    Create {
        /// The Computer's name for you.
        #[arg(long)]
        name: String,
        /// Let more than one Bot use it.
        #[arg(long)]
        shared: bool,
        /// Keep it to one Bot; this is the default.
        #[arg(long)]
        exclusive: bool,
    },
    /// Change who may use a Hosted Computer.
    Update {
        /// The Hosted Computer identifier.
        computer: Id,
        /// Let more than one Bot use it.
        #[arg(long)]
        shared: bool,
        /// Keep it to one Bot.
        #[arg(long)]
        exclusive: bool,
    },
    /// Clear a Hosted Computer's Workspace.
    Reset {
        /// The Hosted Computer identifier.
        computer: Id,
    },
    /// List the Computers you own.
    List,
}

#[derive(Subcommand)]
enum ChatCommand {
    /// List your Chats.
    List,
    /// Send a Message, with an optional File, and wait for the Bot's reply.
    #[command(group(
        ArgGroup::new("payload")
            .required(true)
            .multiple(true)
            .args(["text", "file"])
    ))]
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
        /// The directory this run works in; defaults to the Bot's Workspace.
        #[arg(long)]
        cwd: Option<std::path::PathBuf>,
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
    // The dependency graph carries one rustls provider (aws-lc-rs, pulled in
    // by reqwest's `rustls` feature). The command socket chooses it explicitly
    // before it connects.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    match Cli::parse().command {
        Top::Serve { port, ui, domain } => serve(port, ui, domain).await,
        Top::Join { link, name } => join(&link, &name).await,
        Top::Login { name, server } => login(&name, server).await,
        Top::Whoami => whoami(),
        Top::Provider { command } => provider(command).await,
        Top::Catalog { command } => catalog(command).await,
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

async fn serve(port: u16, ui: Option<std::path::PathBuf>, domain: Option<String>) -> Result<()> {
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
    let domain = match domain.or_else(|| std::env::var("NEHA_DOMAIN").ok()) {
        Some(value) => {
            let value = value.trim().to_owned();
            if value.is_empty() {
                bail!("--domain needs a host");
            }
            Some(value)
        }
        None => None,
    };
    if let Some(domain) = &domain
        && (domain.contains("://")
            || domain.contains('/')
            || domain.contains('?')
            || domain.contains('#'))
    {
        bail!("--domain takes a host[:port], not a URL or path");
    }
    let public_url = domain.map(|domain| format!("https://{domain}"));
    let has_ui = ui.is_some();
    let running = match public_url {
        Some(public_url) => {
            neha_space::start_with_ui_public(home, port, ui, Some(public_url)).await
        }
        None => neha_space::start_with_ui(home, port, ui).await,
    }
    .context("the Space cannot start")?;
    let public = running.public_url();
    let cli_server = public.clone().unwrap_or_else(|| running.url());

    println!("Neha is listening on {}", running.url());
    if let Some(public) = &public {
        println!("Public: {public}");
    }
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
        println!("Sign in from the CLI with: neha login --name <name> --server {cli_server}");
    } else {
        println!("Sign in with: neha login --name <name> --server {cli_server}");
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
            no_tools,
            thinks,
            thinking,
            discover,
        } => {
            let api_key = provider_key()?;
            let default = models.first().cloned();
            let result = client::command(
                &credential,
                CommandCall::ProviderAdd(ProviderAdd {
                    protocol,
                    base_url: base_url.clone(),
                    api_key,
                    models: models
                        .into_iter()
                        .map(|name| ModelSpec {
                            name,
                            context_limit,
                            max_output,
                            reads_images: !no_images,
                            thinks,
                            tools: !no_tools,
                            thinking_levels: Vec::new(),
                        })
                        .collect(),
                    default_model: default,
                    thinking,
                    vendor: None,
                    discover,
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
        ProviderCommand::Connect {
            vendor,
            base_url,
            protocol,
            thinking,
        } => {
            let api_key = provider_key()?;
            let result = client::command(
                &credential,
                CommandCall::ProviderConnect(ProviderConnect {
                    vendor: vendor.clone(),
                    api_key,
                    base_url,
                    protocol,
                    thinking,
                }),
            )
            .await?;
            println!(
                "Connected Provider {} from {vendor}.",
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
        ProviderCommand::Refresh { provider } => {
            let result =
                client::command(&credential, CommandCall::ProviderRefresh { provider }).await?;
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
                    "{}  {}  {}  {}",
                    id_of(provider, "id")?,
                    provider["vendor"].as_str().unwrap_or("-"),
                    text_of(provider, "protocol")?,
                    text_of(provider, "base_url")?
                );
            }
        }
    }

    Ok(())
}

async fn catalog(command: CatalogCommand) -> Result<()> {
    let credential = Credential::load()?;
    match command {
        CatalogCommand::List => {
            let result = client::command(&credential, CommandCall::CatalogList).await?;
            for provider in result.as_array().context("no Providers in the answer")? {
                let protocol = provider["protocol"].as_str().unwrap_or("unsupported");
                let selectable = provider["selectable"].as_bool().unwrap_or(false);
                println!(
                    "{}  {}  {}  {} models{}",
                    text_of(provider, "id")?,
                    text_of(provider, "name")?,
                    protocol,
                    provider["model_count"].as_u64().unwrap_or_default(),
                    if selectable {
                        ""
                    } else {
                        "  (not connectable)"
                    }
                );
            }
        }
        CatalogCommand::Status => {
            let status = client::command(&credential, CommandCall::CatalogStatus).await?;
            println!(
                "Catalog: {} ({} Providers, {} Models)",
                text_of(&status, "source")?,
                status["provider_count"].as_u64().unwrap_or_default(),
                status["model_count"].as_u64().unwrap_or_default()
            );
            if status["builtin"].as_bool().unwrap_or(false) {
                println!("  Built-in snapshot; run `neha catalog refresh` to cache a fresh one.");
            } else {
                println!(
                    "  Cache: {}",
                    status["cache_path"].as_str().unwrap_or_default()
                );
                if let Some(age) = status["age_seconds"].as_u64() {
                    println!(
                        "  Age: {age}s; stale: {}",
                        status["stale"].as_bool().unwrap_or(false)
                    );
                }
                if let Some(etag) = status["etag"].as_str() {
                    println!("  ETag: {etag}");
                }
            }
        }
        CatalogCommand::Refresh => {
            let result = client::command(&credential, CommandCall::CatalogRefresh).await?;
            let status = &result["status"];
            println!(
                "Catalog refreshed: {} Providers, {} Models (changed: {}).",
                status["provider_count"].as_u64().unwrap_or_default(),
                status["model_count"].as_u64().unwrap_or_default(),
                result["changed"].as_bool().unwrap_or(false)
            );
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
                let thinks = if model["thinks"].as_bool().unwrap_or(false) {
                    if let Some(levels) = model["thinking_levels"].as_array()
                        && !levels.is_empty()
                    {
                        format!(
                            "thinks ({})",
                            levels
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join(",")
                        )
                    } else {
                        "thinks".to_owned()
                    }
                } else {
                    "no thinking".to_owned()
                };
                let tools = if model["tools"].as_bool().unwrap_or(true) {
                    "tools"
                } else {
                    "no tools"
                };
                println!(
                    "{}  {}  context {}  max output {}  {images}  {thinks}  {tools}{default}",
                    id_of(model, "id")?,
                    text_of(model, "name")?,
                    model["context_limit"].as_u64().unwrap_or_default(),
                    model["max_output"].as_u64().unwrap_or_default()
                );
            }
        }
        ModelCommand::Default { model } => {
            let result = client::command(&credential, CommandCall::ModelDefault { model }).await?;
            let name = result["models"]
                .as_array()
                .and_then(|models| {
                    models
                        .iter()
                        .find(|view| view["id"] == serde_json::json!(model))
                })
                .and_then(|view| view["name"].as_str())
                .unwrap_or_default();
            println!("The default Model is now {model} ({name}).");
        }
    }

    Ok(())
}

/// Parses a protocol name.
fn parse_protocol(value: &str) -> Result<ProviderProtocol> {
    match value {
        "chat-completions" | "chat_completions" => Ok(ProviderProtocol::ChatCompletions),
        "openai-responses" | "openai_responses" => Ok(ProviderProtocol::OpenAiResponses),
        "anthropic-messages" | "anthropic_messages" => Ok(ProviderProtocol::AnthropicMessages),
        other => bail!(
            "unknown protocol {other:?}; use chat-completions, openai-responses, or anthropic-messages"
        ),
    }
}

/// Parses a thinking level.
fn parse_thinking(value: &str) -> Result<ThinkingLevel> {
    match value {
        "minimal" => Ok(ThinkingLevel::Minimal),
        "low" => Ok(ThinkingLevel::Low),
        "medium" => Ok(ThinkingLevel::Medium),
        "high" => Ok(ThinkingLevel::High),
        "max" => Ok(ThinkingLevel::Max),
        other => bail!("unknown thinking level {other:?}; use minimal, low, medium, high, or max"),
    }
}

/// Parses `on|off` for a Bot's full access.
fn parse_full_access(value: &str) -> Result<bool> {
    match value {
        "on" | "true" | "yes" => Ok(true),
        "off" | "false" | "no" => Ok(false),
        other => bail!("expects on or off, not {other:?}"),
    }
}

/// Checks one grant path and returns its normalized spelling.
///
/// A grant names one absolute folder: an empty, relative, or `..` path is
/// refused here, where the message can name the path. The Space reads the
/// path with the same rule and folds repeated separators and `.` components
/// away, so two spellings of one folder are one grant.
fn normalized_grant(path: &std::path::Path) -> Result<String> {
    let spelled = path.to_string_lossy();
    let trimmed = spelled.trim();
    let refusal = |message: &str| anyhow!("{message}: {trimmed}");
    neha::folder::normalize_folder(trimmed).map_err(|error| match error {
        neha::folder::FolderError::Empty => anyhow!("a grant cannot be empty"),
        neha::folder::FolderError::Relative => refusal("a grant must be an absolute path"),
        neha::folder::FolderError::ParentDir => refusal("a grant must not use `..`"),
        neha::folder::FolderError::Nul => refusal("a grant must not contain a NUL"),
    })
}

/// Checks one grant path and returns the canonical path to store.
///
/// The folder must exist: the Computer opens it as a write root, and a grant
/// for a folder nobody can see would silently write nowhere.
fn checked_grant(path: &std::path::Path) -> Result<String> {
    let granted = normalized_grant(path)?;
    let folder = std::fs::canonicalize(&granted)
        .with_context(|| format!("the grant folder does not exist: {granted}"))?;
    if !folder.is_dir() {
        bail!("a grant must be a folder: {}", folder.display());
    }

    Ok(folder.to_string_lossy().into_owned())
}

/// Returns the folder a revoke names, preferring the path the record shows.
///
/// A grant stores a folder's canonical path. The argument is compared as the
/// User spelled it first, without trimming anything: a directory name that
/// ends in a space keeps it, so `logs ` cannot take the grant for `logs`. Only
/// after that does the raw path get normalized at the edge and resolved as a
/// live alias — the same argument then removes the grant a symlink path added
/// — and a path that is gone is matched by the value the record holds.
fn revocable_grant(path: &std::path::Path, shown: &[String]) -> Result<String> {
    let spelled = path.to_string_lossy();
    // The exact spelling, keeping every component. A path that is not a
    // folder form at all falls through to the edge normalization below.
    if let Ok(exact) = neha::folder::normalize_recorded(&spelled)
        && let Some(recorded) = recorded_grant(shown, &exact)
    {
        return Ok(recorded);
    }
    let normalized = normalized_grant(path)?;
    if let Some(recorded) = recorded_grant(shown, &normalized) {
        return Ok(recorded);
    }
    let Ok(alias) = std::fs::canonicalize(&normalized) else {
        return Ok(normalized);
    };
    let alias = alias.to_string_lossy().into_owned();
    if let Some(recorded) = recorded_grant(shown, &alias) {
        return Ok(recorded);
    }

    Ok(alias)
}

/// Returns the recorded grant that names one folder, if the Bot has it.
fn recorded_grant(shown: &[String], folder: &str) -> Option<String> {
    let folder = recorded_form(folder);
    shown
        .iter()
        .find(|grant| recorded_form(grant) == folder)
        .cloned()
}

/// Returns one grant path in the form a comparison reads it.
///
/// A stored path keeps every component exactly; a path a User typed is trimmed
/// at the CLI boundary before it gets here.
fn recorded_form(grant: &str) -> String {
    neha::folder::normalize_recorded(grant).unwrap_or_else(|_| grant.trim().to_owned())
}

/// A Bot update that changes nothing yet, for a caller to name its one change.
fn access_update(bot: Id) -> BotUpdate {
    BotUpdate {
        bot,
        name: None,
        reset_model: false,
        reset_thinking: false,
        full_access: None,
        full_access_consent: None,
        grants: None,
        grant_add: None,
        grant_remove: None,
        model: None,
        thinking: None,
    }
}

/// Sends one access change and returns the updated Bot.
async fn update_access(credential: &Credential, update: BotUpdate) -> Result<Value> {
    client::command(credential, CommandCall::BotUpdate(update)).await
}

/// Prints one Bot's file access from a `bot show` answer or an access change.
fn print_access(bot: &Value) -> Result<()> {
    println!("Bot {} ({})", id_of(bot, "id")?, text_of(bot, "name")?);
    write_access(&mut std::io::stdout().lock(), bot)
}

/// Writes one Bot's file access: its full-access state, then its grants.
fn write_access(out: &mut impl Write, bot: &Value) -> Result<()> {
    let full_access = if bot["full_access"].as_bool().unwrap_or(false) {
        "on"
    } else {
        "off"
    };
    writeln!(out, "  full access: {full_access}")?;
    match bot["grants"].as_array() {
        Some(grants) if !grants.is_empty() => {
            for grant in grants {
                writeln!(out, "  grant {}", grant.as_str().unwrap_or_default())?;
            }
        }
        _ => writeln!(out, "  grants: none")?,
    }

    Ok(())
}

/// Prints the risk full access takes and asks the User to confirm it.
///
/// `--yes` answers the prompt; a non-interactive run without it is an error,
/// because silently removing the sandbox from a script would skip the one
/// decision that asks for.
fn confirm_full_access(
    bot: Id,
    yes: bool,
    interactive: bool,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<()> {
    writeln!(
        output,
        "Full access starts no sandbox for this Bot: it reads and writes"
    )?;
    writeln!(
        output,
        "everything this machine's user can, inside and outside its"
    )?;
    writeln!(output, "Workspace and granted folders.")?;
    writeln!(
        output,
        "It can also read Neha's configuration directory, ~/.config/neha by"
    )?;
    writeln!(
        output,
        "default, which holds the saved pairing, the machine's token, and CLI"
    )?;
    writeln!(output, "sessions.")?;
    if yes {
        return Ok(());
    }
    if !interactive {
        bail!(
            "full access is not confirmed: stdin is not a terminal; pass --yes to turn it on without a prompt"
        );
    }
    write!(
        output,
        "Turn full access on for Bot {bot}? Type yes to confirm: "
    )?;
    output.flush()?;
    let mut answer = String::new();
    if input.read_line(&mut answer)? == 0 || answer.trim() != "yes" {
        bail!("full access is not confirmed: answer yes to turn it on");
    }

    Ok(())
}

async fn bot(command: BotCommand) -> Result<()> {
    let credential = Credential::load()?;
    match command {
        BotCommand::Create {
            name,
            computer,
            thinking,
            model,
        } => {
            let computer = computer.as_deref().map(parse_bot_computer).transpose()?;
            let result = client::command(
                &credential,
                CommandCall::BotCreate(BotCreate {
                    name: name.clone(),
                    computer,
                    thinking,
                    model,
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
        BotCommand::Set {
            bot,
            model,
            thinking,
            reset_model,
            reset_thinking,
        } => {
            if model.is_none() && thinking.is_none() && !reset_model && !reset_thinking {
                bail!("a Bot update names --model, --thinking, --reset-model, or --reset-thinking");
            }
            let result = client::command(
                &credential,
                CommandCall::BotUpdate(BotUpdate {
                    model,
                    thinking,
                    reset_model,
                    reset_thinking,
                    ..access_update(bot)
                }),
            )
            .await?;
            println!("Updated Bot {}.", id_of(&result, "id")?);
            match result["model"].as_str() {
                Some(model) => println!("  model: {model}"),
                None if reset_model => println!("  model: Space default"),
                None => {}
            }
            match result["thinking"].as_str() {
                Some(thinking) => println!("  thinking: {thinking}"),
                None if reset_thinking => println!("  thinking: Space default"),
                None => {}
            }
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
        BotCommand::Rename { bot, name } => {
            let result = client::command(
                &credential,
                CommandCall::BotUpdate(BotUpdate {
                    name: Some(name),
                    ..access_update(bot)
                }),
            )
            .await?;
            println!(
                "Renamed Bot {} to {}.",
                id_of(&result, "id")?,
                text_of(&result, "name")?
            );
        }
        BotCommand::Access { bot } => {
            let shown = client::command(&credential, CommandCall::BotShow { bot }).await?;
            print_access(&shown["bot"])?;
        }
        BotCommand::Grant { bot, path } => {
            let granted = checked_grant(&path)?;
            // The Space adds the folder to its current list, so a grant that
            // arrives with another change cannot overwrite it.
            let result = update_access(
                &credential,
                BotUpdate {
                    grant_add: Some(granted),
                    ..access_update(bot)
                },
            )
            .await?;
            print_access(&result)?;
        }
        BotCommand::Revoke { bot, path } => {
            // Which recorded grant this path names is read first; the change
            // itself stays one atomic remove command.
            let shown = client::command(&credential, CommandCall::BotShow { bot }).await?;
            let shown: Vec<String> = shown["bot"]["grants"]
                .as_array()
                .context("the answer has no grants")?
                .iter()
                .map(|grant| grant.as_str().unwrap_or_default().to_owned())
                .collect();
            let revoked = revocable_grant(&path, &shown)?;
            let result = update_access(
                &credential,
                BotUpdate {
                    grant_remove: Some(revoked),
                    ..access_update(bot)
                },
            )
            .await?;
            print_access(&result)?;
        }
        BotCommand::FullAccess {
            bot,
            full_access,
            yes,
        } => {
            if full_access {
                let stdin = std::io::stdin();
                confirm_full_access(
                    bot,
                    yes,
                    stdin.is_terminal(),
                    &mut stdin.lock(),
                    &mut std::io::stdout().lock(),
                )?;
            }
            let result = update_access(
                &credential,
                BotUpdate {
                    full_access: Some(full_access),
                    // The confirmation asked for this range under this rule;
                    // the Space records the answer only when it carries the
                    // version it states.
                    full_access_consent: full_access
                        .then_some(neha::record::FULL_ACCESS_CONSENT_VERSION),
                    ..access_update(bot)
                },
            )
            .await?;
            print_access(&result)?;
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
            // `bot access` prints the same lines.
            write_access(&mut std::io::stdout().lock(), &result["bot"])?;
            if let Some(model) = result["bot"]["model"].as_str() {
                println!("  model: {model}");
            }
            if let Some(thinking) = result["bot"]["thinking"].as_str() {
                println!("  thinking: {thinking}");
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
        ComputerCommand::Create {
            name,
            shared,
            exclusive,
        } => {
            let credential = Credential::load()?;
            if shared && exclusive {
                bail!("a Computer create names at most one of --shared or --exclusive");
            }
            let sharing = if shared {
                ComputerSharing::Shared
            } else {
                ComputerSharing::Exclusive
            };
            let result = client::command(
                &credential,
                CommandCall::ComputerCreate(ComputerCreate { name, sharing }),
            )
            .await?;
            println!(
                "Created Computer {} ({}, {}).",
                id_of(&result, "id")?,
                text_of(&result, "name")?,
                text_of(&result, "sharing")?
            );
            println!("  Workspace: {}", text_of(&result, "workspace")?);
        }
        ComputerCommand::Update {
            computer,
            shared,
            exclusive,
        } => {
            if shared == exclusive {
                bail!("a Computer update names exactly one of --shared or --exclusive");
            }
            let credential = Credential::load()?;
            let sharing = if shared {
                ComputerSharing::Shared
            } else {
                ComputerSharing::Exclusive
            };
            let result = client::command(
                &credential,
                CommandCall::ComputerUpdate(ComputerUpdate { computer, sharing }),
            )
            .await?;
            println!(
                "Computer {} ({}) is now {}.",
                id_of(&result, "id")?,
                text_of(&result, "name")?,
                text_of(&result, "sharing")?
            );
        }
        ComputerCommand::Reset { computer } => {
            let credential = Credential::load()?;
            let result =
                client::command(&credential, CommandCall::ComputerReset { computer }).await?;
            let id = id_of(&result, "computer")?;
            println!("Reset Computer {id} ({}).", text_of(&result, "kind")?);
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
                    "{}  {}  {}  {}  {online}  {}",
                    id_of(computer, "id")?,
                    text_of(computer, "name")?,
                    text_of(computer, "kind")?,
                    text_of(computer, "sharing")?,
                    text_of(computer, "workspace")?
                );
            }
        }
    }

    Ok(())
}

async fn chat(command: ChatCommand) -> Result<()> {
    if let Some(bot) = client::BotSession::from_env()? {
        let ChatCommand::Send {
            chat,
            bot: named,
            text,
            file,
            cwd,
            follow,
        } = command
        else {
            bail!("a Bot's shell can send a Message or a File with `neha chat send`");
        };
        if named.is_some() {
            bail!("a Bot's shell names no Bot");
        }
        if cwd.is_some() {
            bail!("a Bot's shell cannot choose a working directory");
        }
        if follow {
            bail!("a Bot's shell has no run to follow");
        }
        let Some(file) = &file else {
            bail!("a Bot's shell sends a File with `neha chat send --file`");
        };
        let chat = bot.chat(chat)?;
        let uploaded = Some(bot.upload(file).await?);
        let post = bot
            .send(chat, text.unwrap_or_default(), uploaded.as_ref())
            .await?;
        println!("Sent {} in Chat {chat}.", post.message);
        if let Some(file) = post.file {
            println!("  file {} ({})", file.id, file.name);
        }

        return Ok(());
    }
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
            cwd,
            follow,
        } => {
            let chat = resolve_chat(&credential, chat, bot).await?;
            let uploaded = match &file {
                Some(path) => Some(client::upload_file(&credential, chat, path).await?),
                None => None,
            };
            let cwd = cwd.map(|path| path.to_string_lossy().into_owned());
            let command = CommandCall::MessageSend(MessageSend {
                chat,
                cwd_writable: cwd.is_some().then_some(true),
                cwd,
                text: text.unwrap_or_default(),
                file: uploaded.as_ref().map(|file| file.id),
            });
            let result = client::command(&credential, command).await?;
            let run = id_of(&result, "run")?;
            // The Space records the normalized directory, so show the User
            // the directory this run actually works in.
            if let Some(cwd) = result["cwd"].as_str() {
                println!("  cwd {cwd}");
            }
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

/// Writes one downloaded File to the destination a `file get` named.
fn write_downloaded(
    name: Option<String>,
    bytes: Vec<u8>,
    out: Option<std::path::PathBuf>,
    implicit_dir: Option<std::path::PathBuf>,
) -> Result<()> {
    let path = match &out {
        Some(path) if !path.is_dir() => path.clone(),
        Some(directory) => directory.join(space_file_name(name.as_deref())?),
        None => {
            let name = space_file_name(name.as_deref())?;
            match implicit_dir {
                Some(directory) => directory.join(name),
                None => std::path::PathBuf::from(name),
            }
        }
    };
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    if out.is_none() && path.exists() {
        bail!(
            "{} already exists; pass --out with the path you want",
            path.display()
        );
    }
    std::fs::write(&path, &bytes).with_context(|| format!("cannot write {}", path.display()))?;
    let shown = std::fs::canonicalize(&path).unwrap_or(path);
    println!("Wrote {} ({} bytes).", shown.display(), bytes.len());

    Ok(())
}

async fn file(command: FileCommand) -> Result<()> {
    if let Some(bot) = client::BotSession::from_env()? {
        let FileCommand::Get { file, out } = command;
        let (name, bytes) = bot.download(file).await?;
        let workspace = std::env::var_os("HOME")
            .map(std::path::PathBuf::from)
            .filter(|path| path.is_dir())
            .context("a Bot's shell has no HOME folder")?;
        write_downloaded(name, bytes, out, Some(workspace))?;

        return Ok(());
    }
    let credential = Credential::load()?;
    match command {
        FileCommand::Get { file, out } => {
            let (name, bytes) = client::download_file(&credential, file).await?;
            write_downloaded(name, bytes, out, None)?;
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
    if let Some(cwd) = result["cwd"].as_str() {
        writeln!(out, "  cwd {cwd}")?;
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
        escape_text(text_of(approval, "tool")?),
        text_of(approval, "reason")?,
        escape_text(approval["summary"].as_str().unwrap_or_default())
    ))
}

/// Prints where one approval sits in its run.
fn write_approval_details(out: &mut impl Write, approval: &Value) -> Result<()> {
    if let Some(step) = approval["step"].as_u64() {
        writeln!(out, "  step {step}")?;
    }
    if let Some(index) = approval["index"].as_u64() {
        writeln!(out, "  index {index}")?;
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
    // The directory the run works in, when it named one: the action lands
    // there, so the User reads it before answering.
    if let Some(cwd) = approval["cwd"].as_str() {
        writeln!(out, "  cwd {}", escape_text(cwd))?;
    }
    // An ambiguous approval cannot name one rule; saying so lets the User
    // know the answer may create a different lasting rule than expected.
    let ambiguous = approval["ambiguous"].as_bool().unwrap_or(false);
    writeln!(out, "  ambiguous {ambiguous}")?;
    if let Some(arguments) = approval.get("arguments") {
        // The whole action, escaped and never clipped, so the User can check
        // exactly what the approval will run.
        let arguments =
            serde_json::to_string(arguments).context("the approval arguments are not JSON")?;
        writeln!(out, "  arguments {}", escape_text(&arguments))?;
    }

    Ok(())
}

/// Escapes control characters so a card cannot hide or rewrite itself.
fn escape_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch if ch.is_control() => escaped.push_str(&format!("\\u{{{:04x}}}", ch as u32)),
            ch => escaped.push(ch),
        }
    }

    escaped
}

/// Names the files an Always allow would cover from a path.
fn file_scope(path: &str) -> String {
    let parent = std::path::Path::new(path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    match parent {
        None => "files in the Workspace root".to_owned(),
        Some(parent) if parent == std::path::Path::new(".") => {
            "files in the Workspace root".to_owned()
        }
        Some(parent) => format!(
            "files under \"{}\"",
            escape_text(&parent.display().to_string())
        ),
    }
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
            println!(
                "{}",
                rule_add_report(
                    &result,
                    bot,
                    effect.wire(),
                    &tool,
                    command_prefix.as_deref(),
                    path_prefix.as_deref()
                )?
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

/// The line that reports one added rule with the values the Space stored.
///
/// The Space normalizes a prefix before it records the rule; printing what the
/// Space returned shows the rule that actually exists, not the text typed.
fn rule_add_report(
    result: &Value,
    bot: Id,
    effect: &str,
    tool: &str,
    command_prefix: Option<&str>,
    path_prefix: Option<&str>,
) -> Result<String> {
    let rule = entity_id(result, &["rule", "id"])?;
    let command_prefix = result.get("command_prefix").map_or_else(
        || command_prefix.map(str::to_owned),
        |value| value.as_str().map(str::to_owned),
    );
    let path_prefix = result.get("path_prefix").map_or_else(
        || path_prefix.map(str::to_owned),
        |value| value.as_str().map(str::to_owned),
    );

    Ok(rule_add_line(
        rule,
        bot,
        effect,
        tool,
        command_prefix.as_deref(),
        path_prefix.as_deref(),
    ))
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

/// What an approval view said about an Always allow's reach.
#[derive(Clone, Debug, PartialEq, Eq)]
enum LastingScope {
    /// The view had no field; the CLI may derive a scope from the action.
    Missing,
    /// The Space said this action has no lasting scope.
    Unavailable,
    /// The Space named the scope.
    Named(String),
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
    /// Whether no rule can judge the action, so the reason cannot name one.
    ambiguous: bool,
    /// Whether the Space says the action can be undone, when it says.
    undoable: Option<String>,
    /// What an Always allow would cover, as the Space said or left out.
    lasting_scope: LastingScope,
    /// The directory the run works in, when it named one.
    cwd: Option<String>,
    /// The arguments the action runs with.
    arguments: Value,
}

impl ApprovalCard {
    /// Reads one card from the Space's approval view.
    fn from_view(approval: &Value) -> Result<Self> {
        Ok(Self {
            approval: entity_id(approval, &["approval"])?,
            tool: text_of(approval, "tool")?.to_owned(),
            summary: approval["summary"].as_str().unwrap_or_default().to_owned(),
            reason: text_of(approval, "reason")?.to_owned(),
            ambiguous: approval["ambiguous"].as_bool().unwrap_or(false),
            undoable: approval["undoable"].as_str().map(str::to_owned),
            lasting_scope: match approval.get("lasting_scope") {
                None => LastingScope::Missing,
                Some(Value::Null) => LastingScope::Unavailable,
                Some(Value::String(scope)) if !scope.is_empty() => {
                    LastingScope::Named(scope.clone())
                }
                Some(_) => LastingScope::Unavailable,
            },
            arguments: approval.get("arguments").cloned().unwrap_or(Value::Null),
            cwd: approval["cwd"].as_str().map(str::to_owned),
        })
    }

    /// Returns whether the Space would accept a lasting answer.
    fn offers_always(&self) -> bool {
        self.always_scope().is_some()
    }

    /// Returns the card's line about undoing the action.
    ///
    /// The Space names the undoability; when that field is missing this falls
    /// back to the tool: a shell command or a write can change the machine and
    /// is not undoable, while a read may leak data and is not called safe.
    fn undo_text(&self) -> &'static str {
        match self.undoable.as_deref() {
            Some("no") => "This action cannot be undone.",
            Some("unknown") => "It is not known whether this action can be undone.",
            _ => match self.tool.as_str() {
                "shell" | "write_file" => "This action cannot be undone.",
                _ => "It is not known whether this action can be undone.",
            },
        }
    }

    /// Returns what an Always allow would cover, when the prompt offers it.
    ///
    /// The Space decides whether a lasting rule is possible: an unavailable
    /// scope means no Always allow at all. A missing field is derived locally
    /// from the tool and its arguments, but only when that derivation is
    /// possible; the User never grants a lasting rule without seeing its
    /// reach.
    fn always_scope(&self) -> Option<String> {
        if self.reason != "rule" && self.reason != "grant" {
            return None;
        }

        match &self.lasting_scope {
            LastingScope::Named(scope) => Some(escape_text(scope)),
            LastingScope::Unavailable => None,
            LastingScope::Missing => self.derived_scope(),
        }
    }

    /// Derives a lasting scope from the tool and its arguments.
    fn derived_scope(&self) -> Option<String> {
        match self.tool.as_str() {
            "shell" => self
                .arguments
                .get("command")
                .and_then(Value::as_str)
                .and_then(|command| command.split_whitespace().next())
                .map(|word| format!("shell commands starting with \"{}\"", escape_text(word))),
            "read_file" | "write_file" | "list_files" => self
                .arguments
                .get("path")
                .and_then(Value::as_str)
                .filter(|path| !path.is_empty())
                .map(file_scope),
            _ => None,
        }
    }

    /// Returns the reason as the card reads it.
    fn reason_text(&self) -> String {
        // The first use of a Local Computer is the reason even if something
        // else is true; then ambiguity wins because no rule can name what
        // asked.
        if self.reason == "first_use_local" {
            "first use of your Local Computer".to_owned()
        } else if self.ambiguous {
            "a form no rule can judge".to_owned()
        } else if self.reason == "rule" {
            "a rule you set".to_owned()
        } else if self.reason == "grant" {
            "a folder you have not granted".to_owned()
        } else {
            self.reason.clone()
        }
    }

    /// Returns the keys the prompt offers.
    fn options(&self) -> &'static str {
        if self.offers_always() {
            "[a]llow / [A]lways allow / [d]eny / [q]uit"
        } else {
            "[a]llow / [d]eny / [q]uit"
        }
    }

    /// Returns the answers the prompt accepts.
    fn answers(&self) -> &'static str {
        if self.offers_always() {
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
            format!("\n  {}", escape_text(&self.summary))
        };
        // The action lands in the directory the run works in, when it named
        // one; the line comes with the action it belongs to.
        let cwd = self
            .cwd
            .as_deref()
            .filter(|cwd| !cwd.is_empty())
            .map(|cwd| format!("\n  in {}", escape_text(cwd)))
            .unwrap_or_default();

        let scope = self
            .always_scope()
            .map(|scope| format!("\n  Always allow covers: {scope}"))
            .unwrap_or_default();

        format!(
            "Approval {} for {}{}{}\n  reason: {}{}\n  {}",
            self.approval,
            escape_text(&self.tool),
            summary,
            cwd,
            self.reason_text(),
            scope,
            self.undo_text()
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
        if let Some(decision) = decision_from_input(&line, card.offers_always()) {
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

    use super::{
        Cli, ComputerCommand, Top, computer_kind_label, pairing_hint, parse_bot_computer,
        parse_protocol, parse_thinking,
    };

    use super::{
        ApprovalCard, ApprovalCommand, Decision, LastingScope, RuleCommand, RuleEffectChoice,
        approval_row, decision_from_input, decision_line, escape_text, read_decision,
        rule_add_line, rule_add_report, rule_rows, validate_rule_arguments, waiting_advice,
        write_approval_details,
    };

    use super::{
        BotCommand, checked_grant, confirm_full_access, normalized_grant, revocable_grant,
        write_access,
    };

    /// Parses a CLI command line.
    fn parse(args: &[&str]) -> Top {
        Cli::try_parse_from(std::iter::once("neha").chain(args.iter().copied()))
            .unwrap()
            .command
    }

    /// A card for one tool; absent Space fields exercise the local fallbacks.
    fn approval_card(tool: &str, reason: &str) -> ApprovalCard {
        ApprovalCard {
            approval: Id::new(),
            tool: tool.to_owned(),
            summary: "printf approved > proof.txt".to_owned(),
            reason: reason.to_owned(),
            ambiguous: false,
            undoable: None,
            lasting_scope: LastingScope::Missing,
            cwd: None,
            arguments: json!({"command": "printf approved > proof.txt"}),
        }
    }

    #[test]
    fn provider_connect_catalog_and_bot_model_parse() {
        let connect = Cli::try_parse_from([
            "neha",
            "provider",
            "connect",
            "openai",
            "--protocol",
            "openai-responses",
            "--thinking",
            "high",
        ])
        .unwrap();
        let Top::Provider {
            command:
                super::ProviderCommand::Connect {
                    vendor,
                    protocol,
                    thinking,
                    ..
                },
        } = connect.command
        else {
            panic!("expected provider connect");
        };
        assert_eq!(vendor, "openai");
        assert_eq!(
            protocol,
            Some(neha::surface::ProviderProtocol::OpenAiResponses)
        );
        assert_eq!(thinking, Some(neha::surface::ThinkingLevel::High));

        let status = Cli::try_parse_from(["neha", "catalog", "status"]).unwrap();
        assert!(matches!(
            status.command,
            Top::Catalog {
                command: super::CatalogCommand::Status
            }
        ));

        let discover = Cli::try_parse_from([
            "neha",
            "provider",
            "add",
            "--base-url",
            "http://127.0.0.1:9000/v1",
            "--discover",
        ])
        .unwrap();
        let Top::Provider {
            command:
                super::ProviderCommand::Add {
                    discover, models, ..
                },
        } = discover.command
        else {
            panic!("expected provider add");
        };
        assert!(discover);
        assert!(models.is_empty());

        let model = Id::new();
        let update = Cli::try_parse_from([
            "neha",
            "bot",
            "set",
            &model.to_string(),
            "--model",
            &model.to_string(),
            "--thinking",
            "low",
        ])
        .unwrap();
        let Top::Bot {
            command:
                super::BotCommand::Set {
                    bot,
                    model: selected,
                    thinking,
                    reset_model,
                    reset_thinking,
                },
        } = update.command
        else {
            panic!("expected bot set");
        };
        assert_eq!(bot, model);
        assert_eq!(selected, Some(model));
        assert_eq!(thinking, Some(neha::surface::ThinkingLevel::Low));
        assert!(!reset_model, "choosing a Model is not a reset");
        assert!(!reset_thinking, "choosing a level is not a reset");

        let reset = Cli::try_parse_from([
            "neha",
            "bot",
            "set",
            &model.to_string(),
            "--reset-model",
            "--reset-thinking",
        ])
        .unwrap();
        let Top::Bot {
            command:
                super::BotCommand::Set {
                    model: selected,
                    thinking,
                    reset_model,
                    reset_thinking,
                    ..
                },
        } = reset.command
        else {
            panic!("expected bot set");
        };
        assert!(selected.is_none());
        assert!(thinking.is_none());
        assert!(reset_model);
        assert!(reset_thinking);
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
    fn a_provider_protocol_and_thinking_level_parse() {
        assert_eq!(
            parse_protocol("chat-completions").unwrap(),
            neha::surface::ProviderProtocol::ChatCompletions
        );
        assert_eq!(
            parse_protocol("openai-responses").unwrap(),
            neha::surface::ProviderProtocol::OpenAiResponses
        );
        assert_eq!(
            parse_protocol("anthropic-messages").unwrap(),
            neha::surface::ProviderProtocol::AnthropicMessages
        );
        assert!(parse_protocol("grpc").is_err());
        assert_eq!(
            parse_thinking("minimal").unwrap(),
            neha::surface::ThinkingLevel::Minimal
        );
        assert_eq!(
            parse_thinking("max").unwrap(),
            neha::surface::ThinkingLevel::Max
        );
        assert!(parse_thinking("turbo").is_err());

        let provider = parse(&[
            "provider",
            "add",
            "--base-url",
            "http://127.0.0.1:9000/v1",
            "--protocol",
            "openai-responses",
            "--model",
            "m",
            "--thinks",
            "--thinking",
            "high",
        ]);
        let Top::Provider {
            command:
                super::ProviderCommand::Add {
                    protocol,
                    thinks,
                    thinking,
                    ..
                },
        } = provider
        else {
            panic!("expected provider add");
        };
        assert_eq!(protocol, neha::surface::ProviderProtocol::OpenAiResponses);
        assert!(thinks);
        assert_eq!(thinking, Some(neha::surface::ThinkingLevel::High));

        let bot = parse(&["bot", "create", "Scout", "--thinking", "low"]);
        let Top::Bot {
            command: super::BotCommand::Create { thinking, .. },
        } = bot
        else {
            panic!("expected bot create");
        };
        assert_eq!(thinking, Some(neha::surface::ThinkingLevel::Low));
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
    fn computer_reset_takes_one_computer() {
        let id = neha::id::Id::new().to_string();
        let cli = Cli::try_parse_from(["neha", "computer", "reset", &id]).unwrap();
        match cli.command {
            Top::Computer {
                command: ComputerCommand::Reset { computer },
            } => assert_eq!(computer.to_string(), id),
            _ => panic!("the reset command did not parse"),
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
            ("shell", None, Some("/tmp"), "--path-prefix"),
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
        let rule = approval_card("shell", "rule");
        let rendered = rule.render();
        assert!(rendered.contains("Approval "), "{rendered}");
        assert!(rendered.contains("for shell"), "{rendered}");
        assert!(
            rendered.contains("printf approved > proof.txt"),
            "{rendered}"
        );
        assert!(rendered.contains("reason: a rule you set"), "{rendered}");
        assert!(
            rendered.contains("This action cannot be undone."),
            "{rendered}"
        );
        assert_eq!(rule.options(), "[a]llow / [A]lways allow / [d]eny / [q]uit");

        let first_use = approval_card("shell", "first_use_local");
        let rendered = first_use.render();
        assert!(
            rendered.contains("reason: first use of your Local Computer"),
            "{rendered}"
        );
        // The first use of a Local Computer cannot be covered by a lasting
        // rule, so its card neither offers nor describes Always allow.
        assert_eq!(first_use.options(), "[a]llow / [d]eny / [q]uit");
        assert!(!rendered.contains("Always allow"), "{rendered}");
    }

    #[test]
    fn a_card_reads_the_space_view_fields() {
        let view = json!({
            "approval": Id::new(),
            "tool": "shell",
            "summary": "printf hi",
            "reason": "rule",
            "ambiguous": true,
            "undoable": "no",
            "lasting_scope": "shell commands starting with \"printf\"",
            "arguments": {"command": "printf hi"},
        });
        let card = ApprovalCard::from_view(&view).unwrap();
        assert!(card.ambiguous);
        assert_eq!(card.undoable.as_deref(), Some("no"));
        assert_eq!(
            card.lasting_scope,
            LastingScope::Named("shell commands starting with \"printf\"".to_owned())
        );
        assert_eq!(card.arguments["command"], "printf hi");
        let rendered = card.render();
        assert!(
            rendered.contains("This action cannot be undone."),
            "{rendered}"
        );
        assert!(
            rendered.contains("Always allow covers: shell commands starting with \"printf\""),
            "{rendered}"
        );
        assert!(
            rendered.contains("reason: a form no rule can judge"),
            "{rendered}"
        );
    }

    #[test]
    fn a_card_names_the_directory_the_run_works_in() {
        let view = json!({
            "approval": Id::new(),
            "tool": "shell",
            "summary": "printf hi",
            "reason": "rule",
            "cwd": "/work/logs",
            "arguments": {"command": "printf hi"},
        });
        let card = ApprovalCard::from_view(&view).unwrap();
        assert_eq!(card.cwd.as_deref(), Some("/work/logs"));
        let rendered = card.render();
        assert!(rendered.contains("\n  in /work/logs"), "{rendered}");
        // The line sits with the action it belongs to, before the reason.
        let action = rendered
            .find("printf hi")
            .expect("the card names the action");
        let directory = rendered
            .find("in /work/logs")
            .expect("the card names the directory");
        let reason = rendered.find("reason:").expect("the card names a reason");
        assert!(action < directory && directory < reason, "{rendered}");

        // A run that named no directory shows no line; an empty one is none.
        let mut bare = json!({
            "approval": Id::new(),
            "tool": "shell",
            "summary": "printf hi",
            "reason": "rule",
            "arguments": {"command": "printf hi"},
        });
        let rendered = ApprovalCard::from_view(&bare).unwrap().render();
        assert!(!rendered.contains("\n  in "), "{rendered}");
        bare.as_object_mut()
            .unwrap()
            .insert("cwd".to_owned(), json!(""));
        let rendered = ApprovalCard::from_view(&bare).unwrap().render();
        assert!(!rendered.contains("\n  in "), "{rendered}");
    }

    #[test]
    fn a_card_says_whether_the_action_can_be_undone() {
        // The Space's undoable field wins over what the tool looks like.
        let mut card = approval_card("read_file", "rule");
        card.undoable = Some("no".to_owned());
        let rendered = card.render();
        assert!(
            rendered.contains("This action cannot be undone."),
            "{rendered}"
        );
        assert!(
            !rendered.contains("It is not known whether this action can be undone."),
            "{rendered}"
        );

        let mut card = approval_card("shell", "rule");
        card.undoable = Some("unknown".to_owned());
        let rendered = card.render();
        assert!(
            rendered.contains("It is not known whether this action can be undone."),
            "{rendered}"
        );
        assert!(
            !rendered.contains("This action cannot be undone."),
            "{rendered}"
        );

        // A missing field falls back to the tool.
        for tool in ["shell", "write_file"] {
            let rendered = approval_card(tool, "rule").render();
            assert!(
                rendered.contains("This action cannot be undone."),
                "{tool}: {rendered}"
            );
            assert!(
                !rendered.contains("It is not known whether this action can be undone."),
                "{tool}: {rendered}"
            );
            // The line comes after the tool, summary, and reason.
            let reason = rendered.find("reason:").expect("the card names a reason");
            let undo = rendered
                .find("This action cannot be undone.")
                .expect("the card names the undo state");
            assert!(reason < undo, "{tool}: {rendered}");
        }
        for tool in ["read_file", "list_files"] {
            let rendered = approval_card(tool, "rule").render();
            assert!(
                rendered.contains("It is not known whether this action can be undone."),
                "{tool}: {rendered}"
            );
            assert!(
                !rendered.contains("This action cannot be undone."),
                "{tool}: {rendered}"
            );
        }
        // A tool this CLI does not know is not called safe either.
        assert!(
            approval_card("future_tool", "rule")
                .render()
                .contains("It is not known whether this action can be undone.")
        );
    }

    #[test]
    fn a_card_does_not_blame_a_rule_when_no_rule_can_judge() {
        // An ambiguous approval has no matching rule: the card must not say a
        // rule asked, while an ordinary rule approval still names the rule.
        let mut card = approval_card("shell", "rule");
        card.ambiguous = true;
        let rendered = card.render();
        assert!(
            rendered.contains("reason: a form no rule can judge"),
            "{rendered}"
        );
        assert!(!rendered.contains("a rule you set"), "{rendered}");

        let card = approval_card("shell", "rule");
        let rendered = card.render();
        assert!(rendered.contains("reason: a rule you set"), "{rendered}");
        assert!(!rendered.contains("a form no rule can judge"), "{rendered}");

        // Ambiguity also wins for a reason this build does not know.
        let mut card = approval_card("shell", "future_reason");
        card.ambiguous = true;
        assert!(
            card.render().contains("reason: a form no rule can judge"),
            "{}",
            card.render()
        );

        // The first-use reason is unchanged even if the field says ambiguous.
        for ambiguous in [false, true] {
            let mut card = approval_card("shell", "first_use_local");
            card.ambiguous = ambiguous;
            let rendered = card.render();
            assert!(
                rendered.contains("reason: first use of your Local Computer"),
                "{rendered}"
            );
            assert!(!rendered.contains("a rule you set"), "{rendered}");
            assert!(!rendered.contains("a form no rule can judge"), "{rendered}");
        }
    }

    #[test]
    fn a_card_shows_what_always_allow_covers() {
        // The Space's scope wins.
        let mut card = approval_card("shell", "rule");
        card.lasting_scope = LastingScope::Named("shell commands starting with \"git\"".to_owned());
        let rendered = card.render();
        assert!(
            rendered.contains("Always allow covers: shell commands starting with \"git\""),
            "{rendered}"
        );

        // Without the field the scope is derived from the command's first word.
        let card = approval_card("shell", "rule");
        let rendered = card.render();
        assert!(
            rendered.contains("Always allow covers: shell commands starting with \"printf\""),
            "{rendered}"
        );

        // File scopes name the parent directory, or the Workspace root.
        let mut card = approval_card("read_file", "rule");
        card.arguments = json!({"path": "logs/run.txt"});
        assert!(
            card.render()
                .contains("Always allow covers: files under \"logs\""),
            "{}",
            card.render()
        );
        card.arguments = json!({"path": "note.txt"});
        assert!(
            card.render()
                .contains("Always allow covers: files in the Workspace root"),
            "{}",
            card.render()
        );
        card.arguments = json!({"path": "/tmp/report.txt"});
        assert!(
            card.render()
                .contains("Always allow covers: files under \"/tmp\""),
            "{}",
            card.render()
        );
        // An unknown tool without a scope cannot derive one, so no Always
        // allow is offered.
        let mut card = approval_card("future_tool", "rule");
        card.arguments = serde_json::Value::Null;
        assert!(
            !card.render().contains("Always allow covers"),
            "{}",
            card.render()
        );
        assert!(
            !card.options().contains("Always allow"),
            "{}",
            card.options()
        );

        // First use has no Always allow, so it names no lasting scope.
        let rendered = approval_card("shell", "first_use_local").render();
        assert!(!rendered.contains("Always allow covers"), "{rendered}");
    }

    #[test]
    fn a_card_offers_always_only_with_a_lasting_scope() {
        // The Space named a scope: Always allow is offered with that reach.
        let mut card = approval_card("shell", "rule");
        card.lasting_scope = LastingScope::Named("shell commands starting with \"git\"".to_owned());
        assert!(card.options().contains("[A]lways allow"), "{card:?}");
        assert!(
            card.render()
                .contains("Always allow covers: shell commands starting with \"git\""),
            "{}",
            card.render()
        );

        // The Space said this action has no lasting scope (for example a path
        // the Space refused): no Always allow, and no scope line.
        let mut card = approval_card("shell", "rule");
        card.lasting_scope = LastingScope::Unavailable;
        assert!(
            !card.options().contains("Always allow"),
            "{}",
            card.options()
        );
        assert!(
            !card.render().contains("Always allow covers"),
            "{}",
            card.render()
        );

        // A missing field is derived when the action allows it.
        let card = approval_card("shell", "rule");
        assert!(card.options().contains("[A]lways allow"), "{card:?}");
        assert!(
            card.render()
                .contains("Always allow covers: shell commands starting with \"printf\""),
            "{}",
            card.render()
        );

        // A missing field that cannot be derived offers no Always allow.
        let mut card = approval_card("shell", "rule");
        card.arguments = serde_json::Value::Null;
        assert!(
            !card.options().contains("Always allow"),
            "{}",
            card.options()
        );
        assert!(
            !card.render().contains("Always allow covers"),
            "{}",
            card.render()
        );

        // A view with an explicit null is unavailable, not missing.
        let view = json!({
            "approval": Id::new(),
            "tool": "shell",
            "summary": "printf hi",
            "reason": "rule",
            "ambiguous": false,
            "undoable": "no",
            "lasting_scope": null,
            "arguments": {"command": "printf hi"},
        });
        let card = ApprovalCard::from_view(&view).unwrap();
        assert_eq!(card.lasting_scope, LastingScope::Unavailable);
        assert!(
            !card.options().contains("Always allow"),
            "{}",
            card.options()
        );
    }

    #[test]
    fn a_card_escapes_control_characters_and_keeps_long_commands() {
        assert_eq!(
            escape_text("a\nb\rc\td\u{001b}[31m"),
            "a\\nb\\rc\\td\\u{001b}[31m"
        );

        let mut card = approval_card("shell", "rule");
        card.summary = "printf 'hi'\n\r\t\u{001b}[2J spoof".to_owned();
        let rendered = card.render();
        assert!(
            !rendered.contains('\u{001b}'),
            "no escape byte: {rendered:?}"
        );
        assert!(rendered.contains("\\n"), "{rendered:?}");
        assert!(rendered.contains("\\r"), "{rendered:?}");
        assert!(rendered.contains("\\t"), "{rendered:?}");
        assert!(rendered.contains("\\u{001b}[2J"), "{rendered:?}");
        assert!(rendered.contains("spoof"), "{rendered:?}");

        // A long command is shown whole; the terminal wraps it.
        let long = format!("printf '{}' > proof.txt", "x".repeat(400));
        card.summary = long.clone();
        let rendered = card.render();
        assert!(
            rendered.contains(&escape_text(&long)),
            "the long command is not clipped: {rendered:?}"
        );
    }

    #[test]
    fn approval_show_names_the_directory_the_run_works_in() {
        let mut approval = json!({
            "approval": Id::new(),
            "run": Id::new(),
            "bot": Id::new(),
            "action": Id::new(),
            "chat": Id::new(),
            "step": 1,
            "index": 0,
            "tool": "shell",
            "reason": "rule",
            "summary": "printf hi",
            "arguments": {"command": "printf hi"},
        });
        // A view without the directory says nothing about one.
        let mut out = Vec::new();
        write_approval_details(&mut out, &approval).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(!out.contains("cwd"), "{out}");

        // The Space's word is shown, escaped so a card cannot hide itself.
        approval
            .as_object_mut()
            .unwrap()
            .insert("cwd".to_owned(), json!("/work/logs\u{001b}[2J"));
        let mut out = Vec::new();
        write_approval_details(&mut out, &approval).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("  cwd /work/logs\\u{001b}[2J"), "{out}");
    }

    #[test]
    fn approval_show_prints_the_full_arguments() {
        let command = format!("printf '{}' > proof.txt", "a".repeat(400));
        let rule = Id::new();
        let approval = json!({
            "approval": Id::new(),
            "run": Id::new(),
            "bot": Id::new(),
            "action": Id::new(),
            "chat": Id::new(),
            "step": 2,
            "index": 3,
            "tool": "shell",
            "reason": "rule",
            "rule": rule,
            "ambiguous": false,
            "summary": "printf long",
            "arguments": {"command": command, "cwd": "logs"},
        });
        let mut out = Vec::new();
        write_approval_details(&mut out, &approval).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("  index 3"), "{out}");
        assert!(out.contains(&format!("  rule {rule}")), "{out}");
        assert!(out.contains("  ambiguous false"), "{out}");
        let expected = escape_text(&serde_json::to_string(&approval["arguments"]).unwrap());
        assert!(out.contains(&format!("  arguments {expected}")), "{out}");
        assert!(out.contains(&"a".repeat(400)), "the whole command is shown");
    }

    #[test]
    fn approval_show_marks_an_ambiguous_approval() {
        // A rule that cannot judge the action names no single rule; the User
        // must see that before answering.
        let mut approval = json!({
            "approval": Id::new(),
            "run": Id::new(),
            "bot": Id::new(),
            "action": Id::new(),
            "chat": Id::new(),
            "step": 1,
            "index": 0,
            "tool": "shell",
            "reason": "rule",
            "ambiguous": true,
            "summary": "printf?",
            "arguments": {"command": "printf? $(date)"},
        });
        let mut out = Vec::new();
        write_approval_details(&mut out, &approval).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("  ambiguous true"), "{out}");
        assert!(!out.contains("  rule "), "{out}");

        // A view without the field is not called ambiguous.
        approval.as_object_mut().unwrap().remove("ambiguous");
        let mut out = Vec::new();
        write_approval_details(&mut out, &approval).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("  ambiguous false"), "{out}");
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
        let card = approval_card("shell", "rule");
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
        let first_use = approval_card("shell", "first_use_local");
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
    fn a_rule_add_reports_the_normalized_prefixes() {
        let rule = Id::new();
        let bot = Id::new();
        // The Space stored "logs" after normalizing the typed "./logs/".
        let result = json!({
            "rule": rule,
            "command_prefix": null,
            "path_prefix": "logs",
        });
        assert_eq!(
            rule_add_report(&result, bot, "ask", "write_file", None, Some("./logs/")).unwrap(),
            format!("Added rule {rule} for Bot {bot}: ask write_file (path-prefix \"logs\")")
        );

        // The same for shell commands: print what the Space kept.
        let result = json!({
            "rule": rule,
            "command_prefix": "git",
            "path_prefix": null,
        });
        assert_eq!(
            rule_add_report(&result, bot, "allow", "shell", Some("  git  "), None).unwrap(),
            format!("Added rule {rule} for Bot {bot}: allow shell (command-prefix \"git\")")
        );

        // A Space answer without the fields falls back to what was typed.
        let result = json!({"rule": rule});
        assert_eq!(
            rule_add_report(&result, bot, "ask", "shell", Some("git"), None).unwrap(),
            format!("Added rule {rule} for Bot {bot}: ask shell (command-prefix \"git\")")
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

    #[test]
    fn the_bot_access_commands_parse() {
        let bot = Id::new();
        let bot_text = bot.to_string();
        assert!(matches!(
            parse(&["bot", "access", bot_text.as_str()]),
            Top::Bot {
                command: BotCommand::Access { bot: shown }
            } if shown == bot
        ));
        match parse(&["bot", "grant", bot_text.as_str(), "/data/shared"]) {
            Top::Bot {
                command: BotCommand::Grant { bot: shown, path },
            } => {
                assert_eq!(shown, bot);
                assert_eq!(path, std::path::Path::new("/data/shared"));
            }
            _ => panic!("bot grant did not parse"),
        }
        match parse(&["bot", "revoke", bot_text.as_str(), "/data/shared"]) {
            Top::Bot {
                command: BotCommand::Revoke { bot: shown, path },
            } => {
                assert_eq!(shown, bot);
                assert_eq!(path, std::path::Path::new("/data/shared"));
            }
            _ => panic!("bot revoke did not parse"),
        }
        match parse(&["bot", "full-access", bot_text.as_str(), "off"]) {
            Top::Bot {
                command:
                    BotCommand::FullAccess {
                        bot: shown,
                        full_access,
                        yes,
                    },
            } => {
                assert_eq!(shown, bot);
                assert!(!full_access);
                assert!(!yes);
            }
            _ => panic!("bot full-access did not parse"),
        }

        // The one command that changed everything at once is gone.
        assert!(
            Cli::try_parse_from(["neha", "bot", "update", bot_text.as_str()]).is_err(),
            "`bot update` is removed"
        );
        for line in [
            vec!["bot", "grant", bot_text.as_str()],
            vec!["bot", "revoke", bot_text.as_str()],
            vec!["bot", "full-access", bot_text.as_str()],
            vec!["bot", "full-access", bot_text.as_str(), "maybe"],
        ] {
            let refused = Cli::try_parse_from(std::iter::once("neha").chain(line.iter().copied()));
            assert!(refused.is_err(), "{line:?} must be refused");
        }
    }

    #[test]
    fn a_grant_path_is_folded_before_it_is_stored() {
        assert_eq!(
            normalized_grant(std::path::Path::new("/data//shared/./notes/")).unwrap(),
            "/data/shared/notes"
        );
        assert_eq!(
            normalized_grant(std::path::Path::new("  /data/shared  ")).unwrap(),
            "/data/shared"
        );
        assert_eq!(normalized_grant(std::path::Path::new("/")).unwrap(), "/");

        for bad in [
            "",
            "   ",
            "relative/path",
            "./relative",
            "/data/../etc",
            "/data\u{0}notes",
        ] {
            let error = normalized_grant(std::path::Path::new(bad))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("empty")
                    || error.contains("absolute")
                    || error.contains("`..`")
                    || error.contains("NUL"),
                "{bad:?}: {error}"
            );
        }
    }

    #[test]
    fn a_grant_folder_must_exist_and_be_a_folder() {
        let directory = tempfile::tempdir().unwrap();
        let folder = directory.path().join("shared");
        std::fs::create_dir(&folder).unwrap();
        assert_eq!(
            checked_grant(&folder).unwrap(),
            std::fs::canonicalize(&folder)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        );

        let error = checked_grant(&directory.path().join("missing"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not exist"), "{error}");
        let file = directory.path().join("notes.txt");
        std::fs::write(&file, "notes").unwrap();
        let error = checked_grant(&file).unwrap_err().to_string();
        assert!(error.contains("must be a folder"), "{error}");
    }

    #[test]
    fn a_revoke_reads_a_live_folder_like_a_grant() {
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("shared");
        std::fs::create_dir(&real).unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // A grant stores the folder a symlink names; the same argument must
        // find that folder again when it is revoked.
        let granted = checked_grant(&link).unwrap();
        assert_eq!(
            granted,
            std::fs::canonicalize(&real)
                .unwrap()
                .to_string_lossy()
                .into_owned()
        );
        assert_eq!(
            revocable_grant(&link, std::slice::from_ref(&granted)).unwrap(),
            granted
        );

        // A folder that is gone has no target to read, so the recorded value
        // is what a revoke names.
        std::fs::remove_file(&link).unwrap();
        std::fs::remove_dir(&real).unwrap();
        assert_eq!(
            revocable_grant(
                &std::path::PathBuf::from(&granted),
                std::slice::from_ref(&granted)
            )
            .unwrap(),
            granted
        );
    }

    #[test]
    fn a_revoke_keeps_a_name_that_ends_in_a_space() {
        let directory = tempfile::tempdir().unwrap();
        let plain = directory.path().join("logs");
        let spaced = directory.path().join("logs ");
        std::fs::create_dir(&plain).unwrap();
        std::fs::create_dir(&spaced).unwrap();
        let plain_grant = checked_grant(&plain).unwrap();
        let spaced_grant = checked_grant(&spaced).unwrap();
        let shown = vec![plain_grant.clone(), spaced_grant.clone()];

        // The argument's trailing space is the name: the exact spelling wins
        // over the trimmed form, so `logs ` cannot take the grant for `logs`.
        assert_eq!(
            revocable_grant(&spaced, &shown).unwrap(),
            spaced_grant,
            "the exact spelling names its own folder"
        );
        assert_eq!(revocable_grant(&plain, &shown).unwrap(), plain_grant);
        // A raw spelling normalized at the edge still names the folder.
        let slash = format!("{}/", plain.display());
        assert_eq!(
            revocable_grant(std::path::Path::new(&slash), &shown).unwrap(),
            plain_grant
        );
    }

    #[test]
    fn a_revoke_removes_the_recorded_grant_a_later_link_points_elsewhere() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let first_grant = checked_grant(&first).unwrap();
        let second_grant = checked_grant(&second).unwrap();
        let shown = vec![first_grant.clone(), second_grant.clone()];

        // The granted folder may become a symlink to another granted folder
        // after it was recorded. The record's own path is what the revoke
        // names, so the other grant is not removed instead.
        std::fs::rename(&first, directory.path().join("moved")).unwrap();
        std::os::unix::fs::symlink(&second, &first).unwrap();
        assert_eq!(
            revocable_grant(&first, &shown).unwrap(),
            first_grant,
            "the recorded path wins over the link's target"
        );
        assert_eq!(
            revocable_grant(&second, &shown).unwrap(),
            second_grant,
            "a path the record shows is removed exactly as recorded"
        );

        // An ancestor that becomes a symlink does not redirect the revoke
        // either: the record's path is matched before anything is resolved.
        let nested_parent = directory.path().join("nested");
        let nested = nested_parent.join("shared");
        std::fs::create_dir_all(&nested).unwrap();
        let nested_grant = checked_grant(&nested).unwrap();
        std::fs::rename(&nested_parent, directory.path().join("nested-moved")).unwrap();
        let other_parent = directory.path().join("other-parent");
        std::fs::create_dir_all(other_parent.join("shared")).unwrap();
        std::os::unix::fs::symlink(&other_parent, &nested_parent).unwrap();
        assert_eq!(
            revocable_grant(&nested, std::slice::from_ref(&nested_grant)).unwrap(),
            nested_grant,
            "a link above the granted folder does not change what was recorded"
        );

        // A path no grant shows is read as an alias while it exists, so the
        // grant a symlink path added is still reachable.
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(&second, &alias).unwrap();
        assert_eq!(
            revocable_grant(&alias, &shown).unwrap(),
            second_grant,
            "an alias resolves to the recorded folder"
        );
        // A path no grant shows and no folder names is sent as the User
        // spelled it, so the Space refuses it with the name the User used.
        let missing = directory.path().join("missing");
        assert_eq!(
            revocable_grant(&missing, &shown).unwrap(),
            missing.to_string_lossy().into_owned()
        );
    }

    #[test]
    fn bot_access_prints_the_full_access_state_and_every_grant() {
        let bot = json!({
            "id": Id::new(),
            "name": "ada",
            "full_access": true,
            "grants": ["/data/shared", "/var/tmp/notes"],
        });
        let mut shown = Vec::new();
        write_access(&mut shown, &bot).unwrap();
        assert_eq!(
            String::from_utf8(shown).unwrap(),
            "  full access: on\n  grant /data/shared\n  grant /var/tmp/notes\n"
        );

        // A Bot without grants says so instead of printing nothing.
        let bot = json!({"id": Id::new(), "name": "ada", "full_access": false, "grants": []});
        let mut shown = Vec::new();
        write_access(&mut shown, &bot).unwrap();
        assert_eq!(
            String::from_utf8(shown).unwrap(),
            "  full access: off\n  grants: none\n"
        );
    }

    #[test]
    fn full_access_asks_for_yes_and_refuses_everything_else() {
        let bot = Id::new();
        let mut asked = Vec::new();
        confirm_full_access(
            bot,
            false,
            true,
            &mut std::io::Cursor::new(b"yes\n".as_slice()),
            &mut asked,
        )
        .unwrap();
        let asked = String::from_utf8(asked).unwrap();
        // The risk names the configuration directory the Bot can then read.
        assert!(asked.contains("~/.config/neha"), "{asked}");
        assert!(asked.contains("no sandbox"), "{asked}");
        assert!(asked.contains("Type yes to confirm"), "{asked}");

        for answer in ["no\n", "y\n", "\n", "YES\n", "yes please\n"] {
            let mut shown = Vec::new();
            let error = confirm_full_access(
                bot,
                false,
                true,
                &mut std::io::Cursor::new(answer.as_bytes()),
                &mut shown,
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("answer yes"), "{answer:?}: {error}");
        }

        // A terminal that closed cannot answer.
        let error = confirm_full_access(
            bot,
            false,
            true,
            &mut std::io::Cursor::new(b"".as_slice()),
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("answer yes"), "{error}");

        // `--yes` answers the prompt without asking.
        let mut shown = Vec::new();
        confirm_full_access(
            bot,
            true,
            true,
            &mut std::io::Cursor::new(b"".as_slice()),
            &mut shown,
        )
        .unwrap();
        let shown = String::from_utf8(shown).unwrap();
        assert!(!shown.contains("Type yes"), "{shown}");

        // A non-interactive run must say `--yes`.
        let error = confirm_full_access(
            bot,
            false,
            false,
            &mut std::io::Cursor::new(b"yes\n".as_slice()),
            &mut Vec::new(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("--yes"), "{error}");
    }
}
