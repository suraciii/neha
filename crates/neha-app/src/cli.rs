//! The `neha` command line: its parser and the commands it runs.

use std::collections::HashMap;
use std::io::{BufRead, IsTerminal as _, Write};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use clap::{ArgGroup, Parser, Subcommand};
use neha::id::Id;
use neha::memory::MemoryKind;
use neha::record::{ComputerSharing, McpScope, RuleEffect, SkillLibrary};
use neha::seq::Seq;
use neha::surface::{
    BotComputer, BotCreate, BotUpdate, CommandCall, ComputerCreate, ComputerUpdate, McpAdd,
    McpSignInStart, MessageSend, ModelSpec, ProviderAdd, ProviderConnect, ProviderProtocol,
    SkillAdd, SkillAddScript, ThinkingLevel,
};
use neha_space::Home;
use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::client::Credential;
use crate::{client, computer};

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
    /// Manage the skills a Bot may load.
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
    },
    /// Connect an MCP server or list the servers you may see.
    Mcp {
        #[command(subcommand)]
        command: McpCommand,
    },
    /// Install and manage Agent Plugins packages.
    Plugin {
        #[command(subcommand)]
        command: PluginCommand,
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
    /// Show your Bot's memory, or remove one of its entries.
    Memory {
        /// The Bot identifier; `remove` names the Bot instead.
        bot: Option<Id>,
        #[command(subcommand)]
        command: Option<MemoryCommand>,
    },
    /// List your Bots.
    List,
    /// Give your Bot a skill, or take one away.
    Skill {
        #[command(subcommand)]
        command: BotSkillCommand,
    },
    /// Give your Bot an MCP server, or take one away.
    Mcp {
        #[command(subcommand)]
        command: BotMcpCommand,
    },
}

#[derive(Subcommand)]
enum MemoryCommand {
    /// Remove one memory entry.
    Remove {
        /// The Bot whose memory holds the entry.
        bot: Id,
        /// The memory entry identifier.
        entry: Id,
    },
}

#[derive(Subcommand)]
enum ComputerCommand {
    /// Pair this machine with a Space, on this machine or another.
    Pair {
        /// The Space URL; a remote https origin pairs with a device code.
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
    /// Run this container as a Hosted Computer worker.
    Worker,
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
    /// Revoke a Local Computer's credential and disconnect its daemon.
    Revoke {
        /// The Local Computer identifier.
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
    ///
    /// A Computer action is a built-in tool such as shell or write_file, which
    /// only a Bot on a Local Computer can have a rule for. A Space-served call
    /// is a skill's script (--tool run_skill_script with --skill and --script)
    /// or an MCP server's tool (--tool with --server, or --tool alone for every
    /// server that offers that tool name), and a Bot on either Computer can
    /// have a rule for it. A rule names one of these kinds, and its prefixes
    /// fit the tool it names.
    Add {
        /// The Bot the rule applies to.
        #[arg(long)]
        bot: Id,
        /// What the rule does: ask before the action, or allow it.
        #[arg(long, value_enum)]
        effect: RuleEffectChoice,
        /// The tool the rule matches: a built-in tool such as shell or
        /// write_file, run_skill_script with --skill and --script, or the name
        /// of a tool an MCP server lists.
        #[arg(long)]
        tool: String,
        /// Match shell commands that start with this text.
        #[arg(long)]
        command_prefix: Option<String>,
        /// Match file paths that start with this text.
        #[arg(long)]
        path_prefix: Option<String>,
        /// Match a skill's script by the skill's name; with --script.
        #[arg(long, requires = "script")]
        skill: Option<String>,
        /// Match one script of that skill by its file name; with --skill.
        #[arg(long, requires = "skill")]
        script: Option<String>,
        /// Match a tool an MCP server gives, by the server's name; without it
        /// an MCP tool name matches every enabled server that offers it.
        #[arg(long)]
        server: Option<String>,
    },
    /// Remove one rule.
    Remove {
        /// The rule identifier.
        rule: Id,
    },
}

#[derive(Subcommand)]
enum SkillCommand {
    /// Add a skill from a directory that holds a SKILL.md and its scripts.
    Add {
        /// The skill's directory: SKILL.md, and scripts directly in scripts/.
        path: std::path::PathBuf,
        /// The Bot whose library holds it; without it the Space's library
        /// holds it, which only the Admin may add to.
        #[arg(long)]
        bot: Option<Id>,
    },
    /// List skills: the Space's and your Bots', or one Bot's enabled skills.
    List {
        /// Only the skills this Bot may use.
        #[arg(long)]
        bot: Option<Id>,
    },
    /// Remove a skill from its library.
    Remove {
        /// The skill identifier.
        skill: Id,
    },
}

#[derive(Subcommand)]
enum PluginCommand {
    /// Install a package from a source the Admin allows.
    ///
    /// A repository source is <url>@<40-or-64-hex-commit>: the commit is the
    /// 40- or 64-character lowercase hexadecimal object id of the commit to
    /// read, and a branch or tag is not a commit. A directory source is a
    /// plain path to a directory that holds plugin.json, read where it is; it
    /// is supported only on Linux, and on any other platform, install from a
    /// repository.
    ///
    /// The Admin lists the sources the Space allows under [plugins] sources
    /// in the Space's config.toml; a source the list does not allow is
    /// refused.
    ///
    /// A package is at most 200 files, 1024 entries in all (files and
    /// directories), 100 KiB per file, and 4 MiB per package; a package above
    /// one of those is refused with a report.
    Install {
        /// The source the package comes from.
        source: String,
    },
    /// List the installed packages and what they brought.
    List,
    /// Remove a package and everything it brought.
    Remove {
        /// The package identifier.
        plugin: Id,
    },
    /// Let the Space offer a package's servers and skills again.
    Enable {
        /// The package identifier.
        plugin: Id,
    },
    /// Stop the Space offering a package's servers and skills.
    Disable {
        /// The package identifier.
        plugin: Id,
    },
}

#[derive(Subcommand)]
enum McpCommand {
    /// Connect an MCP server.
    ///
    /// The credential comes from NEHA_MCP_TOKEN, never from the command line.
    /// With the token set, the Space connects the server at once; naming an
    /// existing Space server again attaches the credential to it.
    ///
    /// Without an address, the command attaches the Space's credential to an
    /// existing package's stdio server of that name; only the Admin may do
    /// that, and the token is asked for when the environment carries none.
    ///
    /// Without the token, the command opens the browser to sign in. The
    /// sign-in is bound to a loopback receiver this command runs, so the
    /// browser brings the answer back to the terminal. Naming an existing
    /// server of the same name and address attaches your own credential to it;
    /// a new server enters the Space when the sign-in finishes.
    Add {
        /// The server's name, unique in the Space: letters, digits, or hyphens.
        name: String,
        /// The address it speaks Streamable HTTP at; omit it to attach the
        /// Space credential to a package's existing stdio server of this name.
        url: Option<String>,
        /// Connect it for the signed-in User alone; without it the Space is
        /// the scope, which only the Admin may add to.
        #[arg(long, value_parser = parse_mcp_scope)]
        scope: Option<McpScope>,
    },
    /// List the MCP servers you may see.
    List,
    /// Disconnect an MCP server.
    Remove {
        /// The server identifier.
        server: Id,
    },
}

#[derive(Subcommand)]
enum BotSkillCommand {
    /// Give a Bot a skill.
    Enable {
        /// The Bot identifier.
        bot: Id,
        /// The skill identifier.
        skill: Id,
    },
    /// Take a skill away from a Bot.
    Disable {
        /// The Bot identifier.
        bot: Id,
        /// The skill identifier.
        skill: Id,
    },
}

#[derive(Subcommand)]
enum BotMcpCommand {
    /// Give a Bot the tools of an MCP server.
    Enable {
        /// The Bot identifier.
        bot: Id,
        /// The MCP server identifier.
        server: Id,
    },
    /// Take an MCP server's tools away from a Bot.
    Disable {
        /// The Bot identifier.
        bot: Id,
        /// The MCP server identifier.
        server: Id,
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

/// Runs the `neha` command line from this process's arguments.
#[tokio::main]
pub async fn run() -> Result<()> {
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
        Top::Run { command } => run_command(command).await,
        Top::Approval { command } => approval(command).await,
        Top::Rule { command } => rule(command).await,
        Top::Skill { command } => skill_command(command).await,
        Top::Mcp { command } => mcp_command(command).await,
        Top::Plugin { command } => plugin_command(command).await,
    }
}

async fn serve(port: u16, ui: Option<std::path::PathBuf>, domain: Option<String>) -> Result<()> {
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
    let public_url = match domain {
        Some(domain) => Some(format!("https://{domain}")),
        None => match std::env::var("NEHA_PUBLIC_URL") {
            Ok(value) => Some(value),
            Err(std::env::VarError::NotPresent) => None,
            Err(error) => bail!("NEHA_PUBLIC_URL is not usable: {error}"),
        },
    };
    // Validate the origin, proxy list, and listener before opening the home:
    // a start that may not serve must not rename a database or clear temporary
    // files. The bound listener is the one this start uses.
    let startup = neha_space::prepare_start(port, public_url)
        .await
        .context("the Space cannot start")?;
    let home = Home::from_env()
        .context("NEHA_HOME is not set; set it to the directory that holds this Space")?;
    let has_ui = ui.is_some();
    let running = neha_space::start_with_ui_prepared(home, ui, startup)
        .await
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

/// Parses an MCP server's scope.
///
/// A server added without `--scope user` belongs to the Space, so `user` is
/// the only value the option spells.
fn parse_mcp_scope(value: &str) -> Result<McpScope> {
    match value {
        "user" => Ok(McpScope::User),
        other => bail!("unknown scope {other:?}; --scope takes user, and the Space is the default"),
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

/// Checks one grant path and returns it in the form the Space records.
///
/// The folder may be on another machine, so this client checks only the
/// syntax: it cannot see whether the path exists or what it resolves to. The
/// Computer resolves the folder when it opens the write root, and a grant for
/// a folder that is not there fails the action that uses it.
fn checked_grant(path: &std::path::Path) -> Result<String> {
    normalized_grant(path)
}

/// Returns the folder a revoke names, preferring the path the record shows.
///
/// A grant may name a folder on another machine, so this client compares the
/// argument with the record's paths and never resolves it against this
/// machine's filesystem. The argument is compared as the User spelled it
/// first, without trimming anything: a directory name that ends in a space
/// keeps it, so `logs ` cannot take the grant for `logs`. Only after that does
/// the raw path get normalized at the edge.
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

    Ok(normalized)
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

/// The line that says how a Bot gets its first memory entry.
const EMPTY_MEMORY: &str = "No memory entries. Ask the Bot in a Chat to remember something.";

/// Formats a memory list: one line per entry, then the room each kind has.
fn memory_rows(list: &Value) -> Result<Vec<String>> {
    let entries = list["entries"]
        .as_array()
        .context("no memory entries in the answer")?;
    let mut rows: Vec<String> = entries.iter().map(memory_row).collect::<Result<_>>()?;
    if entries.is_empty() {
        rows.push(EMPTY_MEMORY.to_owned());
    }
    rows.extend(memory_usage_rows(list)?);

    Ok(rows)
}

/// Formats one memory entry as a single line.
///
/// An entry the caller does not read prints its metadata and `(metadata only)`
/// in place of the text the Space withheld.
fn memory_row(entry: &Value) -> Result<String> {
    let content = if entry["readable"].as_bool().unwrap_or(false) {
        visible_text(text_of(entry, "text")?)
    } else {
        "(metadata only)".to_owned()
    };

    Ok(format!(
        "{}  {}  {}  author {}  source {} {}  {}  {}",
        id_of(entry, "memory")?,
        text_of(entry, "kind")?,
        text_of(entry, "scope")?,
        text_of(entry, "author")?,
        text_of(entry, "source_kind")?,
        text_of(entry, "source")?,
        byte_size(size_of(entry, "size")?),
        content,
    ))
}

/// Makes a memory text printable on one line.
///
/// A memory text is what the Bot or a User wrote, so it may hold a line break
/// or a terminal control sequence. Escaping those keeps one entry on one line
/// and keeps the terminal's state out of the Bot's memory. Only a control
/// character changes; a literal backslash stays as it was written.
fn visible_text(text: &str) -> String {
    let mut shown = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\n' => shown.push_str("\\n"),
            '\r' => shown.push_str("\\r"),
            '\t' => shown.push_str("\\t"),
            ch if ch.is_control() => {
                shown.push_str(&format!("\\u{{{:x}}}", ch as u32));
            }
            ch => shown.push(ch),
        }
    }

    shown
}

/// The room each memory kind has: its used size and its limit.
fn memory_usage_rows(list: &Value) -> Result<Vec<String>> {
    let used = list["used"]
        .as_object()
        .context("the answer has no memory usage")?;
    let limits = list["limits"]
        .as_object()
        .context("the answer has no memory limits")?;
    let mut rows = Vec::new();
    for kind in MemoryKind::ALL {
        let name = kind.as_str();
        let used = used
            .get(name)
            .and_then(Value::as_u64)
            .with_context(|| format!("the answer has no {name} memory usage"))?;
        let limit = limits
            .get(name)
            .and_then(Value::as_u64)
            .with_context(|| format!("the answer has no {name} memory limit"))?;
        rows.push(format!(
            "{name}: {} used / {} limit",
            byte_size(used),
            byte_size(limit)
        ));
    }

    Ok(rows)
}

/// Returns the number in one of a list's fields.
fn size_of(value: &Value, field: &str) -> Result<u64> {
    value[field]
        .as_u64()
        .with_context(|| format!("the answer has no numeric {field}"))
}

/// A byte count for a reader: `62 B`, `1.5 KiB`, `4.0 KiB`, `16 KiB`.
fn byte_size(bytes: u64) -> String {
    const UNITS: [&str; 3] = ["KiB", "MiB", "GiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
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
        BotCommand::Memory { bot, command } => match command {
            None => {
                let Some(bot) = bot else {
                    bail!(
                        "`neha bot memory` needs a Bot: run `neha bot memory <bot>` to list \
                         its memory, or `neha bot memory remove <bot> <entry>` to remove one"
                    );
                };
                let list = client::command(&credential, CommandCall::MemoryList { bot }).await?;
                for row in memory_rows(&list)? {
                    println!("{row}");
                }
            }
            Some(MemoryCommand::Remove { bot, entry }) => {
                // The Space refuses an entry that belongs to another Bot.
                let result = client::command(
                    &credential,
                    CommandCall::MemoryRemove { bot, memory: entry },
                )
                .await?;
                let shown = result
                    .get("memory")
                    .and_then(Value::as_str)
                    .map_or_else(|| entry.to_string(), str::to_owned);
                println!("Removed memory {shown}.");
            }
        },
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
        BotCommand::Skill { command } => bot_skill(command).await?,
        BotCommand::Mcp { command } => bot_mcp(command).await?,
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
        ComputerCommand::Worker => computer::worker().await?,
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
        ComputerCommand::Revoke { computer } => {
            let credential = Credential::load()?;
            let result =
                client::command(&credential, CommandCall::ComputerRevoke { computer }).await?;
            let id = id_of(&result, "id")?;
            println!(
                "Revoked Computer {id}. Its daemon stops and the record stays offline; pair it again to use it."
            );
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
            for row in chat_read_rows(&result)? {
                println!("{row}");
            }
        }
    }

    Ok(())
}

/// The line that says a later request reads the Chat's earlier span through a
/// summary.
const COMPACTED_MARKER: &str = "  (earlier Messages were compacted)";

/// Formats `chat read`: one line per Message, its File, and its compaction
/// marker.
///
/// The record keeps every Message; a compaction's summary is not one of them,
/// so the marker only says where the summarized span ends. The summary text is
/// never printed.
fn chat_read_rows(result: &Value) -> Result<Vec<String>> {
    let messages = result.as_array().context("no Messages in the answer")?;
    let mut rows = Vec::with_capacity(messages.len());
    for message in messages {
        let author = match message["author"]["kind"].as_str().unwrap_or_default() {
            "bot" => "Bot",
            _ => "User",
        };
        let text = text_of(message, "text")?;
        let seq = message["seq"].as_str().unwrap_or_default();
        // A File alone is a Message, so the line can end at the colon.
        rows.push(if text.is_empty() {
            format!("[{seq}] {author}:")
        } else {
            format!("[{seq}] {author}: {text}")
        });
        if let Some(file) = message["file"].as_str() {
            rows.push(format!("  file {file}"));
        }
        if message["compacted"].as_bool().unwrap_or(false) {
            rows.push(COMPACTED_MARKER.to_owned());
        }
    }

    Ok(rows)
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

async fn run_command(command: RunCommand) -> Result<()> {
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

/// Escapes control characters and the escape character so a card cannot hide
/// or rewrite itself. A literal `\n` renders as `\\n`, so no raw control
/// character and no ambiguous escape reaches the reader.
fn escape_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
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
            skill,
            script,
            server,
        } => {
            validate_rule_arguments(
                &tool,
                command_prefix.as_deref(),
                path_prefix.as_deref(),
                skill.as_deref(),
                script.as_deref(),
                server.as_deref(),
            )?;
            let result = client::command(
                &credential,
                CommandCall::RuleAdd {
                    bot,
                    effect: effect.into(),
                    tool: tool.clone(),
                    command_prefix: command_prefix.clone(),
                    path_prefix: path_prefix.clone(),
                    skill: skill.clone(),
                    script: script.clone(),
                    server: server.clone(),
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
                    path_prefix.as_deref(),
                    skill.as_deref(),
                    script.as_deref(),
                    server.as_deref()
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

/// Adds, lists, and removes skills.
async fn skill_command(command: SkillCommand) -> Result<()> {
    let credential = Credential::load()?;
    match command {
        SkillCommand::Add { path, bot } => {
            let (document, scripts) = read_skill_directory(&path)?;
            let library = if bot.is_some() {
                SkillLibrary::Bot
            } else {
                SkillLibrary::Space
            };
            let result = client::command(
                &credential,
                CommandCall::SkillAdd(SkillAdd {
                    library,
                    bot,
                    document,
                    scripts,
                }),
            )
            .await?;
            println!(
                "Added skill {} ({}) with {} script(s).",
                id_of(&result, "skill")?,
                result["name"].as_str().unwrap_or_default(),
                result["scripts"].as_array().map_or(0, std::vec::Vec::len)
            );
            if let Some(bot) = bot {
                println!(
                    "Give it to the Bot with: neha bot skill enable {} {}",
                    bot,
                    id_of(&result, "skill")?
                );
            }
        }
        SkillCommand::List { bot } => {
            let list = client::command(&credential, CommandCall::SkillList { bot }).await?;
            for row in skill_rows(&list)? {
                println!("{row}");
            }
        }
        SkillCommand::Remove { skill } => {
            let result = client::command(&credential, CommandCall::SkillRemove { skill }).await?;
            let shown = result
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("the skill");
            println!("Removed skill {shown}.");
        }
    }

    Ok(())
}

/// Installs and manages Agent Plugins packages.
async fn plugin_command(command: PluginCommand) -> Result<()> {
    let credential = Credential::load()?;
    match command {
        PluginCommand::Install { source } => {
            let result =
                client::command(&credential, CommandCall::PluginInstall { source }).await?;
            println!(
                "Installed package {} ({}{}).",
                id_of(&result, "plugin")?,
                result["name"].as_str().unwrap_or_default(),
                result["version"]
                    .as_str()
                    .map_or_else(String::new, |version| format!(" {version}"))
            );
            println!(
                "  digest: {}; source: {}",
                result["digest"].as_str().unwrap_or_default(),
                result["source"].as_str().unwrap_or_default()
            );
            if let Some(commit) = result["commit"].as_str() {
                println!("  commit: {commit}");
            }
            let servers = result["servers"]
                .as_array()
                .map_or(&[][..], std::vec::Vec::as_slice);
            if servers.is_empty() {
                println!("  servers: -");
            }
            for server in servers {
                // A server row carries its transport's fields: an HTTP
                // server's address, or a stdio server's command.
                println!(
                    "  server: {} ({})",
                    server["name"].as_str().unwrap_or_default(),
                    mcp_reach_label(server)
                );
            }
            let skills = result["skills"]
                .as_array()
                .map_or(&[][..], std::vec::Vec::as_slice);
            if skills.is_empty() {
                println!("  skills: -");
            }
            for skill in skills {
                println!("  skill: {}", skill["name"].as_str().unwrap_or_default());
            }
            for skipped in result["skipped"]
                .as_array()
                .map_or(&[][..], std::vec::Vec::as_slice)
            {
                println!("  skipped: {}", skipped.as_str().unwrap_or_default());
            }
            print_cleanup(&result, CleanupStyle::Detail);
        }
        PluginCommand::List => {
            let list = client::command(&credential, CommandCall::PluginList).await?;
            for row in plugin_rows(&list)? {
                println!("{row}");
            }
        }
        PluginCommand::Remove { plugin } => {
            let result = client::command(&credential, CommandCall::PluginRemove { plugin }).await?;
            println!(
                "Removed package {}.",
                result["name"].as_str().unwrap_or_default()
            );
            print_cleanup(&result, CleanupStyle::Retry);
        }
        PluginCommand::Enable { plugin } | PluginCommand::Disable { plugin } => {
            let enabled = matches!(command, PluginCommand::Enable { .. });
            let call = if enabled {
                CommandCall::PluginEnable { plugin }
            } else {
                CommandCall::PluginDisable { plugin }
            };
            let result = client::command(&credential, call).await?;
            println!(
                "Package {plugin} is {}.",
                if enabled { "enabled" } else { "disabled" }
            );
            print_cleanup(&result, CleanupStyle::Retry);
        }
    }

    Ok(())
}

/// How a command reports the cleanup warning its result may carry.
#[derive(Clone, Copy)]
enum CleanupStyle {
    /// One detail line under the install's report.
    Detail,
    /// The sentence a package command prints, which names the retry that
    /// finishes the cleanup.
    Retry,
}

/// The value the Space uses for a cleanup a retried command just finished.
const CLEANUP_FINISHED: &str = "finished";

/// Formats the top-level `cleanup` field a result may carry, or `None` when it
/// carries none.
///
/// The Space writes `incomplete: …` while it still owes a cleanup, and
/// `finished` when a retried command completed one it had reported. The Space
/// sanitizes that text and never puts a secret in it; escaping it here too
/// keeps a hostile package's words from rewriting the terminal, and one line
/// keeps the report a row.
fn cleanup_notice(result: &Value, style: CleanupStyle) -> Option<String> {
    let cleanup = result.get("cleanup")?.as_str()?.trim();
    if cleanup.is_empty() {
        return None;
    }
    Some(match style {
        CleanupStyle::Detail => format!("  cleanup: {}", escape_text(cleanup)),
        CleanupStyle::Retry if cleanup == CLEANUP_FINISHED => "Cleanup finished.".to_owned(),
        CleanupStyle::Retry => {
            // The Space's warning already begins `incomplete: `; naming the
            // state once keeps the line short.
            let warning = escape_text(cleanup.strip_prefix("incomplete: ").unwrap_or(cleanup));
            format!("Cleanup incomplete: {warning}. Run the command again to finish it.")
        }
    })
}

/// Prints the cleanup warning a command's result carries, when it carries one.
fn print_cleanup(result: &Value, style: CleanupStyle) {
    if let Some(notice) = cleanup_notice(result, style) {
        println!("{notice}");
    }
}

/// One row per package: package, name, version, commit, digest, enabled, and
/// what it brought.
fn plugin_rows(list: &Value) -> Result<Vec<String>> {
    list.as_array()
        .context("no packages in the answer")?
        .iter()
        .map(|row| {
            let names = |field: &str, key: &str| -> String {
                let names: Vec<&str> = row[field]
                    .as_array()
                    .map_or(&[][..], std::vec::Vec::as_slice)
                    .iter()
                    .filter_map(|entry| entry[key].as_str())
                    .collect();
                if names.is_empty() {
                    "-".to_owned()
                } else {
                    names.join(", ")
                }
            };
            Ok(format!(
                "{}  {}  {}  {}  digest: {}  {}  servers: {}  skills: {}",
                id_of(row, "plugin")?,
                row["name"].as_str().unwrap_or_default(),
                row["version"].as_str().unwrap_or("-"),
                row["commit"].as_str().unwrap_or("-"),
                row["digest"].as_str().unwrap_or("-"),
                if row["enabled"].as_bool().unwrap_or(false) {
                    "enabled"
                } else {
                    "disabled"
                },
                names("servers", "name"),
                names("skills", "name")
            ))
        })
        .collect()
}

/// Connects, lists, and disconnects MCP servers.
async fn mcp_command(command: McpCommand) -> Result<()> {
    let credential = Credential::load()?;
    match command {
        McpCommand::Add { name, url, scope } => {
            let scope = scope.unwrap_or(McpScope::Space);
            // An omitted address names a package's existing stdio server: no
            // server is created, and only the Space's own credential can be
            // attached to it.
            let url = url.unwrap_or_default();
            let token = match mcp_credential() {
                Some(token) => Some(token),
                // A stdio server has no address to sign in to, so the attach
                // asks for the token instead of opening a browser.
                None if url.trim().is_empty() => Some(read_mcp_credential()?),
                None => None,
            };
            match mcp_add_plan(&name, &url, scope, token) {
                McpAddPlan::Add(arguments) => {
                    let result =
                        client::command(&credential, CommandCall::McpAdd(arguments)).await?;
                    if result["url"].as_str().is_none_or(str::is_empty) {
                        println!(
                            "Attached the Space credential to MCP server {} ({}).",
                            id_of(&result, "server")?,
                            result["name"].as_str().unwrap_or_default()
                        );
                    } else {
                        println!(
                            "Added MCP server {} ({}) at {}.",
                            id_of(&result, "server")?,
                            result["name"].as_str().unwrap_or_default(),
                            result["url"].as_str().unwrap_or_default()
                        );
                    }
                    println!(
                        "  scope: {}; {}",
                        mcp_scope_label(&result),
                        mcp_connection_label(&result)
                    );
                }
                McpAddPlan::SignIn => {
                    // A tokenless add binds its own loopback receiver before it
                    // starts the sign-in: the authorization server then brings
                    // the browser back to this command, not to the Space.
                    //
                    // One deadline covers everything the tokenless add does:
                    // listing the server it may attach to, starting the
                    // sign-in, waiting for the browser, and finishing the
                    // exchange. Each step gets only the time that is left, so
                    // no step can hold the command past the sign-in budget.
                    let deadline = tokio::time::Instant::now() + SIGN_IN_TIMEOUT;
                    let existing = mcp_existing_server(&credential, &name, &url, deadline).await?;
                    let receiver = SignInReceiver::bind().await?;
                    let start =
                        mcp_sign_in_start(&name, &url, scope, receiver.redirect_uri(), existing);
                    // An existing server takes the caller's own credential; a
                    // new server keeps the scope the command asked for.
                    let reported_scope = if existing.is_some() {
                        McpScope::User
                    } else {
                        scope
                    };
                    mcp_sign_in(
                        &credential,
                        deadline,
                        start,
                        &name,
                        &url,
                        reported_scope,
                        receiver,
                    )
                    .await?;
                }
            }
        }
        McpCommand::List => {
            let list = client::command(&credential, CommandCall::McpList).await?;
            for row in mcp_rows(&list)? {
                println!("{row}");
            }
        }
        McpCommand::Remove { server } => {
            let result = client::command(&credential, CommandCall::McpRemove { server }).await?;
            for line in mcp_remove_lines(&result) {
                println!("{line}");
            }
        }
    }

    Ok(())
}

/// The lines `neha mcp remove` prints: the removal, and the cleanup its answer
/// still owes or has finished.
///
/// The cleanup sentence is the one the package commands print, so an operator
/// reads the same words and the same retry instruction wherever a Space
/// reports a cleanup.
fn mcp_remove_lines(result: &Value) -> Vec<String> {
    let shown = result
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("the server");
    let mut lines = vec![format!("Removed MCP server {shown}.")];
    if let Some(cleanup) = cleanup_notice(result, CleanupStyle::Retry) {
        lines.push(cleanup);
    }
    lines
}

/// How often `neha mcp add` asks how a browser sign-in is doing.
const SIGN_IN_POLL: Duration = Duration::from_secs(1);
/// How long `neha mcp add` waits for a browser sign-in to finish.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(300);
/// How long the receiver waits for the browser to read its last page.
///
/// The page is a few hundred bytes on loopback and is written only once the
/// sign-in has ended, so a browser that stopped reading must not hold the
/// command: the outcome is already decided, and only the page is lost.
const SIGN_IN_PAGE_TIMEOUT: Duration = Duration::from_secs(5);
/// The most of one browser callback the loopback receiver reads.
///
/// The redirect carries one request line and a few headers; the bound keeps a
/// local connection from growing the receiver's memory before the deadline.
const MAX_CALLBACK_REQUEST_BYTES: usize = 8 * 1024;

/// The program this platform opens an address with.
#[cfg(target_os = "linux")]
const BROWSER_OPEN: Option<&str> = Some("xdg-open");
#[cfg(target_os = "macos")]
const BROWSER_OPEN: Option<&str> = Some("open");
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
const BROWSER_OPEN: Option<&str> = None;

/// Opens one address in the browser, and reports whether the browser launched.
///
/// The opener is detached: the browser may stay open while `neha mcp add` waits
/// on its loopback receiver and finishes the sign-in, so the command never waits
/// for the browser process. A missing command or a refused launch leaves the
/// browser closed, and the caller prints the address for the User instead.
fn open_browser(url: &str) -> bool {
    let Some(program) = BROWSER_OPEN else {
        return false;
    };
    // The browser's own streams are not this command's output.
    std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

/// What one `neha mcp add` sends: the server itself when the environment
/// carries a credential, and a browser sign-in when it does not.
#[derive(Debug, PartialEq)]
enum McpAddPlan {
    /// Registers the server with the credential the environment carries.
    Add(McpAdd),
    /// Starts a browser sign-in bound to the command's loopback receiver.
    SignIn,
}

/// Decides how one `neha mcp add` connects its server.
///
/// A credential argument would outlive the command in a shell history and a
/// process listing, so `NEHA_MCP_TOKEN` (or, for an attach without an address,
/// the terminal prompt) is its only source; without one the browser sign-in
/// supplies the credential instead. An empty `url` is passed through: the Space
/// reads it as the existing package stdio server of that name.
fn mcp_add_plan(name: &str, url: &str, scope: McpScope, credential: Option<String>) -> McpAddPlan {
    match credential {
        Some(credential) => McpAddPlan::Add(McpAdd {
            name: name.to_owned(),
            url: url.to_owned(),
            scope,
            credential: Some(credential),
        }),
        None => McpAddPlan::SignIn,
    }
}

/// The start command of a tokenless `neha mcp add`.
///
/// A server the Space already holds under `name` and `url` takes the caller's
/// own credential, so the sign-in names only that server; a new server carries
/// the name, address, and scope the Space registers when the sign-in finishes.
/// `redirect_uri` is the loopback receiver this command bound for the browser.
fn mcp_sign_in_start(
    name: &str,
    url: &str,
    scope: McpScope,
    redirect_uri: &str,
    existing: Option<Id>,
) -> McpSignInStart {
    let redirect_uri = Some(redirect_uri.to_owned());
    match existing {
        Some(server) => McpSignInStart {
            server: Some(server),
            name: None,
            url: None,
            // The sign-in attaches the caller's own credential; the Space's
            // own credential is what the token path registers.
            scope: None,
            redirect_uri,
        },
        None => McpSignInStart {
            server: None,
            name: Some(name.to_owned()),
            url: Some(url.to_owned()),
            scope: Some(scope),
            redirect_uri,
        },
    }
}

/// The server a tokenless `neha mcp add` names, when the Space already holds it.
///
/// A sign-in attaches the caller's credential to the server the Space already
/// holds under the same name and address; it never registers a second server
/// under that name, so a name held at another address is a refusal. The list is
/// one step of the sign-in, so it is bounded by the sign-in's deadline like the
/// commands that follow it.
async fn mcp_existing_server(
    credential: &Credential,
    name: &str,
    url: &str,
    deadline: tokio::time::Instant,
) -> Result<Option<Id>> {
    let list = sign_in_command(credential, deadline, CommandCall::McpList).await?;
    existing_server(&list, name, url)
}

/// Reads the server a `mcp_list` answer holds under `name`.
fn existing_server(list: &Value, name: &str, url: &str) -> Result<Option<Id>> {
    let rows = list.as_array().context("no MCP servers in the answer")?;
    let Some(row) = rows
        .iter()
        .find(|row| row.get("name").and_then(Value::as_str) == Some(name))
    else {
        return Ok(None);
    };
    if row.get("url").and_then(Value::as_str) != Some(url.trim()) {
        bail!("the Space already holds an MCP server named {name:?} at another address");
    }
    Ok(Some(id_of(row, "server")?))
}

/// What one browser callback carries.
#[derive(Debug, PartialEq, Eq)]
enum Callback {
    /// The authorization server returned a code bound to the sign-in's state,
    /// and the issuer it named when RFC 9207 named one.
    Code {
        code: String,
        state: String,
        /// The issuer identifier the authorization response named, when it did.
        iss: Option<String>,
    },
    /// The sign-in did not return a code: the authorization server refused it,
    /// or the browser came back without one. Its own text is never kept.
    Refused,
}

/// The loopback receiver one browser sign-in comes back to.
struct SignInReceiver {
    listener: tokio::net::TcpListener,
    /// The address the authorization server redirects the browser to.
    redirect_uri: String,
}

impl SignInReceiver {
    /// Binds a loopback receiver on a free port.
    async fn bind() -> Result<Self> {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("cannot bind the sign-in receiver on loopback")?;
        let port = listener
            .local_addr()
            .context("cannot read the sign-in receiver's address")?
            .port();
        Ok(Self {
            listener,
            redirect_uri: format!("http://127.0.0.1:{port}/callback"),
        })
    }

    /// The redirect URI the sign-in binds the browser to.
    fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// Waits for the browser's one callback, and keeps it for the reply.
    ///
    /// The receiver serves one request: the redirect is single use, so a
    /// second request has nothing left to find, and the listener is dropped as
    /// soon as the one request is read. The wait has one deadline, so a browser
    /// that never comes back cannot hold the command forever.
    async fn receive(self, deadline: tokio::time::Instant) -> Result<SignInCallback> {
        match tokio::time::timeout_at(deadline, self.accept_one()).await {
            Ok(callback) => callback,
            Err(_) => Err(sign_in_timeout(SIGN_IN_TIMEOUT)),
        }
    }

    /// Accepts the one request the browser makes.
    async fn accept_one(self) -> Result<SignInCallback> {
        let Self {
            listener,
            redirect_uri: _,
        } = self;
        let (mut stream, _) = listener
            .accept()
            .await
            .context("cannot accept the sign-in callback")?;
        // The receiver serves one request: the listener is dropped before the
        // page is written, so no second connection is one this command serves.
        drop(listener);
        let request = read_callback_request(&mut stream).await?;
        let CallbackRequest { path, callback } = callback_of(&request);
        Ok(SignInCallback {
            callback,
            path,
            stream,
        })
    }
}

/// The one browser callback the receiver read, and the reply it still owes.
///
/// The page is held until the Space has finished the sign-in, so a browser can
/// never read success before the credential is registered. The callback itself
/// is only handed to the Space; it is never rendered.
#[derive(Debug)]
struct SignInCallback {
    /// What the browser's redirect carried.
    callback: Callback,
    /// The raw path the browser requested, when the target was origin-form.
    ///
    /// The receiver accepts any path because it cannot know the issuer key the
    /// Space registered; the Space compares this path with the redirect it
    /// registered for the flow.
    path: Option<String>,
    /// The browser connection its last page is written to.
    stream: tokio::net::TcpStream,
}

impl SignInCallback {
    /// What the browser's redirect carried.
    fn callback(&self) -> &Callback {
        &self.callback
    }

    /// The raw path the browser requested, when the request line named one.
    fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// Answers the browser with the fixed page its sign-in ended on.
    ///
    /// `finished` is true only once the Space registered the credential; a
    /// refusal and every failure get the fixed failure page. The wait for the
    /// browser to take the page is bounded, because the sign-in is already
    /// decided: a browser that stopped reading loses its page, not the command.
    async fn answer(mut self, finished: bool) -> Result<()> {
        match tokio::time::timeout(
            SIGN_IN_PAGE_TIMEOUT,
            answer_callback(&mut self.stream, finished),
        )
        .await
        {
            Ok(answered) => answered,
            Err(_) => bail!("the browser did not read the sign-in page"),
        }
    }
}

/// The page a browser shows once the Space finished the sign-in it carried.
///
/// The page is fixed: the browser's history must not keep the code, the state,
/// or the authorization server's own words.
const SIGN_IN_FINISHED_PAGE: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<title>MCP sign-in finished</title></head><body><h1>MCP sign-in finished</h1>\
<p>You can close this window and return to Neha.</p></body></html>";

/// The page a browser shows once its callback did not end a sign-in.
const SIGN_IN_FAILED_PAGE: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
<title>MCP sign-in failed</title></head><body><h1>MCP sign-in failed</h1>\
<p>The sign-in did not finish. You can close this window and try again from Neha.</p></body></html>";

/// Reads one callback request's head, bounded.
///
/// The receiver answers the browser only after the request's headers end, so
/// the page is not cut off by a half-read request; a request without its end is
/// bounded by [`MAX_CALLBACK_REQUEST_BYTES`] and by the receiver's deadline.
async fn read_callback_request(stream: &mut tokio::net::TcpStream) -> Result<Vec<u8>> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        let read = stream
            .read(&mut buffer)
            .await
            .context("cannot read the sign-in callback")?;
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        if request.len() > MAX_CALLBACK_REQUEST_BYTES {
            bail!("the sign-in callback request is too large");
        }
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    Ok(request)
}

/// Answers the browser's callback with the page that ends its part of the flow.
async fn answer_callback(stream: &mut tokio::net::TcpStream, finished: bool) -> Result<()> {
    let (status, page) = if finished {
        ("200 OK", SIGN_IN_FINISHED_PAGE)
    } else {
        ("400 Bad Request", SIGN_IN_FAILED_PAGE)
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{page}",
        page.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .context("cannot answer the sign-in callback")?;
    stream
        .flush()
        .await
        .context("cannot answer the sign-in callback")?;
    stream
        .shutdown()
        .await
        .context("cannot answer the sign-in callback")?;
    Ok(())
}

/// What one callback request line carries.
#[derive(Debug, PartialEq, Eq)]
struct CallbackRequest {
    /// The raw path the browser requested, when the target was origin-form.
    ///
    /// It is exactly the request line's path segment: percent-encoding is kept
    /// as received, and neither the query nor a fragment is part of it. The
    /// Space compares it with the issuer-keyed redirect it registered, so the
    /// receiver never decodes or rewrites it.
    path: Option<String>,
    /// What the browser's redirect carried.
    callback: Callback,
}

/// Reads the callback one request line carries.
///
/// The request target must be origin-form (`/...`): the browser sends the
/// redirect as it was registered, so an absolute URL, an authority form, or a
/// fragment is a request this receiver did not register for, and it is refused
/// with the callback it would otherwise read. Only `code`, `state`, and RFC
/// 9207's `iss` are kept from the query; the authorization server's `error` and
/// `error_description` are reduced to the fixed refusal, so no provider text
/// can reach the command's output.
fn callback_of(request: &[u8]) -> CallbackRequest {
    let refused = CallbackRequest {
        path: None,
        callback: Callback::Refused,
    };
    let line = request
        .split(|byte| *byte == b'\r' || *byte == b'\n')
        .next()
        .unwrap_or_default();
    let Ok(line) = std::str::from_utf8(line) else {
        return refused;
    };
    let mut parts = line.split_ascii_whitespace();
    if parts.next() != Some("GET") {
        return refused;
    }
    let Some(target) = parts.next() else {
        return refused;
    };
    let Some((path, query)) = origin_form_target(target) else {
        return refused;
    };
    CallbackRequest {
        path: Some(path.to_owned()),
        callback: callback_of_query(query),
    }
}

/// Splits an origin-form request target into its raw path and its query.
///
/// A fragment is never part of a request target, so one is refused rather than
/// silently dropped, and an empty path names no redirect. The path is returned
/// byte for byte, percent-encoding included.
fn origin_form_target(target: &str) -> Option<(&str, &str)> {
    if target.contains('#') {
        return None;
    }
    let (path, query) = target
        .split_once('?')
        .map_or((target, ""), |(path, query)| (path, query));
    if !path.starts_with('/') {
        return None;
    }
    Some((path, query))
}

/// Reads the callback values one redirect query carries.
fn callback_of_query(query: &str) -> Callback {
    let mut code = None;
    let mut state = None;
    let mut iss = None;
    let mut refused = false;
    for pair in query.split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        match name {
            "code" => code = decode_query_value(value),
            "state" => state = decode_query_value(value),
            "iss" => iss = decode_query_value(value).filter(|iss| !iss.is_empty()),
            "error" => refused = true,
            _ => {}
        }
    }
    match (refused, code, state) {
        (false, Some(code), Some(state)) if !code.is_empty() && !state.is_empty() => {
            Callback::Code { code, state, iss }
        }
        _ => Callback::Refused,
    }
}

/// Decodes one percent-encoded query value; `+` stands for a space.
fn decode_query_value(value: &str) -> Option<String> {
    let mut decoded = Vec::with_capacity(value.len());
    let mut bytes = value.as_bytes().iter().copied();
    while let Some(byte) = bytes.next() {
        match byte {
            b'+' => decoded.push(b' '),
            b'%' => {
                let hex = [bytes.next()?, bytes.next()?];
                let hex = std::str::from_utf8(&hex).ok()?;
                decoded.push(u8::from_str_radix(hex, 16).ok()?);
            }
            byte => decoded.push(byte),
        }
    }
    String::from_utf8(decoded).ok()
}

/// What a started browser sign-in is doing, as the Space reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SignInState {
    /// The browser has not finished the sign-in.
    Pending,
    /// The Space registered the server and sealed a credential.
    Finished {
        /// The server the Space registered.
        server: Id,
        /// Whether the Space holds a credential a call would send.
        connected: bool,
    },
    /// The sign-in cannot finish.
    Failed {
        /// The Space's account of the failure, free of any credential.
        reason: String,
    },
}

/// Reads one `mcp_sign_in_status` answer.
fn sign_in_state(answer: &Value) -> Result<SignInState> {
    match answer.get("state").and_then(Value::as_str) {
        Some("pending") => Ok(SignInState::Pending),
        Some("finished") => Ok(SignInState::Finished {
            server: id_of(answer, "server")?,
            connected: answer
                .get("connected")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        Some("failed") => Ok(SignInState::Failed {
            reason: answer
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("the Space gave no reason")
                .to_owned(),
        }),
        Some(state) => bail!("the Space reported an unknown sign-in state {state:?}"),
        None => bail!("the answer has no sign-in state"),
    }
}

/// Polls one browser sign-in until it ends, and returns how it ended.
///
/// `ask` sends one `mcp_sign_in_status` and returns its answer; the caller owns
/// the connection, so the wait is exercised without a Space. The wait shares
/// the sign-in's one deadline: it is checked before every ask, and every ask is
/// bounded by the time that is left, because a command that never answers must
/// not hold the wait past its deadline. A pending answer sleeps only for the
/// time left.
async fn poll_sign_in<F, Fut>(
    sign_in: Id,
    deadline: tokio::time::Instant,
    poll: Duration,
    mut ask: F,
) -> Result<SignInState>
where
    F: FnMut(Id) -> Fut,
    Fut: std::future::Future<Output = Result<Value>>,
{
    loop {
        let left = sign_in_left(deadline)?;
        let answer = match tokio::time::timeout(left, ask(sign_in)).await {
            Ok(answer) => answer?,
            Err(_) => return Err(sign_in_timeout(SIGN_IN_TIMEOUT)),
        };
        match sign_in_state(&answer)? {
            SignInState::Pending => {}
            ended => return Ok(ended),
        }
        let left = sign_in_left(deadline)?;
        tokio::time::sleep(poll.min(left)).await;
    }
}

/// The time left before the sign-in's one deadline, or its fixed timeout error.
fn sign_in_left(deadline: tokio::time::Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(tokio::time::Instant::now());
    if left.is_zero() {
        return Err(sign_in_timeout(SIGN_IN_TIMEOUT));
    }

    Ok(left)
}

/// Runs one command of a browser sign-in with the time left before its deadline.
///
/// Every step of a tokenless `neha mcp add` shares the one deadline: opening the
/// connection, the Space's greeting, sending the command, and reading its answer
/// must all fit in what remains, so a Space that accepts a socket and never
/// greets it cannot hold the command past the sign-in budget.
async fn sign_in_command(
    credential: &Credential,
    deadline: tokio::time::Instant,
    command: CommandCall,
) -> Result<Value> {
    let left = sign_in_left(deadline)?;
    match tokio::time::timeout(left, client::command(credential, command)).await {
        Ok(result) => result,
        Err(_) => Err(sign_in_timeout(SIGN_IN_TIMEOUT)),
    }
}

/// The error a browser sign-in that outlasts its wait produces.
fn sign_in_timeout(timeout: Duration) -> anyhow::Error {
    anyhow!("the browser sign-in did not finish within {timeout:?}; run the command again to retry")
}

/// The finish command one browser callback sends.
///
/// The raw path the browser requested rides with the code and state: the Space
/// compares it with the issuer-keyed redirect it registered, so a callback
/// delivered to another path cannot finish this sign-in. A callback that
/// returned no code finishes nothing, and has no command to send.
fn sign_in_finish_command(
    sign_in: Id,
    callback: &Callback,
    path: Option<&str>,
) -> Option<CommandCall> {
    let Callback::Code { code, state, iss } = callback else {
        return None;
    };
    Some(CommandCall::McpSignInFinish {
        sign_in,
        code: code.clone(),
        state: state.clone(),
        iss: iss.clone(),
        path: path.map(str::to_owned),
    })
}

/// Hands one browser callback to the Space and waits for the sign-in to end.
///
/// The finish command is the token exchange: the Space answers like a poll once
/// it sealed the credential. A `pending` answer is polled until the Space
/// settles it; every ask and every pause shares the sign-in's deadline. The
/// authorization server's own words never reach any answer: a refused callback
/// is one fixed failure sentence.
async fn finish_sign_in(
    credential: &Credential,
    deadline: tokio::time::Instant,
    sign_in: Id,
    callback: &SignInCallback,
) -> Result<SignInState> {
    let Some(command) = sign_in_finish_command(sign_in, callback.callback(), callback.path())
    else {
        return Ok(SignInState::Failed {
            reason: "the authorization server refused the sign-in".to_owned(),
        });
    };
    let answered = sign_in_command(credential, deadline, command).await?;
    match sign_in_state(&answered)? {
        // The finish answers like a poll; should the Space hand the exchange
        // to its own work, keep asking until it ends.
        SignInState::Pending => {
            poll_sign_in(sign_in, deadline, SIGN_IN_POLL, |sign_in| {
                client::command(credential, CommandCall::McpSignInStatus { sign_in })
            })
            .await
        }
        ended => Ok(ended),
    }
}

/// Answers the browser's last page from how the sign-in ended.
///
/// Success is shown only once the Space answered that the sign-in finished: the
/// page never claims a credential the Space has not registered, and every error
/// shows the fixed failure page. A finished sign-in the Space reports as not
/// connected still registered the credential; that reachability report is the
/// command line's to print, not a sign-in that did not finish. A browser that
/// left early loses its page; its answer changes no command's output.
async fn answer_sign_in_page(callback: SignInCallback, ended: &Result<SignInState>) {
    let finished = matches!(ended, Ok(SignInState::Finished { .. }));
    let _ = callback.answer(finished).await;
}

/// The lines a browser sign-in prints once it ends.
///
/// A finished sign-in the Space reports as not connected prints the two lines
/// `neha mcp add` prints for a server it registered but could not reach. A
/// failed sign-in is the command's error: the Space's reason, escaped to one
/// line so the authorization server's text cannot rewrite the terminal.
fn sign_in_lines(ended: SignInState, name: &str, url: &str, scope: &str) -> Result<Vec<String>> {
    match ended {
        SignInState::Pending => bail!("the browser sign-in has not finished"),
        SignInState::Failed { reason } => bail!("the sign-in failed: {}", escape_text(&reason)),
        SignInState::Finished {
            connected: true, ..
        } => Ok(vec![format!("Connected MCP server {name}.")]),
        SignInState::Finished {
            server,
            connected: false,
        } => Ok(vec![
            format!("Added MCP server {server} ({name}) at {url}."),
            format!("  scope: {scope}; not connected"),
        ]),
    }
}

/// Starts a browser sign-in and waits for its loopback receiver to end it.
///
/// The command opens the address the Space answers with and does not wait for
/// the browser. The browser comes back to this command's receiver instead of
/// the Space, so the code and state the redirect carries are handed to the
/// Space with the raw callback path the browser requested: the Space compares
/// that path with the issuer-keyed redirect it registered, exchanges the code
/// for the credential, and answers like a poll. The browser's page is written
/// only after that answer, so it reports success only when the Space registered
/// the credential. Every step shares the sign-in's one deadline.
async fn mcp_sign_in(
    credential: &Credential,
    deadline: tokio::time::Instant,
    start: McpSignInStart,
    name: &str,
    url: &str,
    scope: McpScope,
    receiver: SignInReceiver,
) -> Result<()> {
    let started = sign_in_command(credential, deadline, CommandCall::McpSignInStart(start)).await?;
    let sign_in = id_of(&started, "sign_in")?;
    let address = text_of(&started, "url")?;
    if open_browser(address) {
        println!("Opening the browser to sign in.");
    } else {
        println!("Could not open a browser; open this address in your browser:");
        println!("{address}");
    }
    let callback = receiver.receive(deadline).await?;
    let outcome = finish_sign_in(credential, deadline, sign_in, &callback).await;
    // The page reports what the Space answered, and is written only now: it
    // cannot claim success before the Space registered the credential. An error
    // is still the command's to report after the page is written.
    answer_sign_in_page(callback, &outcome).await;
    let ended = outcome?;
    let scope = mcp_scope_text(scope, Some(credential.account.id));
    for line in sign_in_lines(ended, name, url, &scope)? {
        println!("{line}");
    }

    Ok(())
}

/// The largest `SKILL.md` a skill directory carries: its frontmatter and its
/// body. The Space holds the body to its own limit; this bound refuses a stray
/// file before its bytes are read.
const MAX_SKILL_DOCUMENT_BYTES: u64 = 256 * 1024;
/// The largest script a skill directory carries. The Space refuses the same
/// size, so the CLI reports it before it uploads.
const MAX_SKILL_SCRIPT_BYTES: u64 = 64 * 1024;
/// The most scripts a skill directory carries. The Space refuses the same
/// count, so the CLI reports it before it uploads.
const MAX_SKILL_SCRIPTS: usize = 16;

/// Reads a skill's directory: its `SKILL.md`, and every file directly in its
/// `scripts/` folder.
///
/// Nothing here follows a symlink. A link at any level — the directory itself,
/// `SKILL.md`, `scripts/`, or an entry in it — could carry a file the user
/// never picked, so every level must be the real thing.
///
/// On Unix the directory is opened once, and every member is opened relative
/// to that descriptor through `/proc/self/fd/<dir_fd>/<name>`. The metadata
/// that authorizes a read is the opened handle's own metadata, and the bytes
/// come from that same handle, so replacing a path between the check and the
/// read cannot redirect the upload. Every read is bounded on the stream, so a
/// file that grows after its metadata is taken is still refused.
#[cfg(unix)]
fn read_skill_directory(path: &std::path::Path) -> Result<(String, Vec<SkillAddScript>)> {
    let directory = open_skill_directory(path)?;
    Ok((
        read_skill_document(&directory, path)?,
        read_skill_scripts(&directory, path)?,
    ))
}

/// A platform with no directory descriptor to read through refuses the
/// import instead of checking a pathname and later reopening it.
///
/// The Unix path above opens the directory once and reads every member through
/// its descriptor, so the metadata that authorizes a read and the bytes read
/// are the same object. This build has no such handle elsewhere, so it fails
/// closed rather than upload whatever a racing writer put at the path.
#[cfg(not(unix))]
fn read_skill_directory(_path: &std::path::Path) -> Result<(String, Vec<SkillAddScript>)> {
    bail!("adding a skill from a directory needs a Unix platform yet")
}

/// The path of an opened descriptor. It names the object the descriptor holds,
/// whatever happens to the pathname it was opened from.
#[cfg(unix)]
fn descriptor_path(file: &std::fs::File) -> std::path::PathBuf {
    use std::os::fd::AsRawFd as _;

    std::path::PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

/// The path of one member of an opened directory, resolved through the
/// directory's descriptor rather than through its pathname.
#[cfg(unix)]
fn descriptor_member_path(directory: &std::fs::File, name: &std::ffi::OsStr) -> std::path::PathBuf {
    descriptor_path(directory).join(name)
}

/// Opens one member of an opened directory with `flags`, never following a
/// final symlink. The caller checks and reads the returned handle, so a
/// replacement after the check cannot change what is read.
#[cfg(unix)]
fn open_directory_member(
    directory: &std::fs::File,
    name: &std::ffi::OsStr,
    flags: libc::c_int,
) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(descriptor_member_path(directory, name))
}

/// Returns whether the pathname itself is a symlink, without following it.
/// A final symlink opened with `O_DIRECTORY | O_NOFOLLOW` is reported as
/// `ENOTDIR` on Linux, so this only picks the message of a refusal that already
/// happened; nothing is read through it.
#[cfg(unix)]
fn is_symlink(path: &std::path::Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink())
}

/// Opens the skill directory itself as a directory, never a symlink to one.
/// It is the descriptor every member below is opened through.
#[cfg(unix)]
fn open_skill_directory(path: &std::path::Path) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;

    match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(directory) => Ok(directory),
        Err(error) => match error.raw_os_error() {
            Some(libc::ELOOP) => bail!("{} is a symlink, which is not followed", path.display()),
            Some(libc::ENOTDIR) if is_symlink(path) => {
                bail!("{} is a symlink, which is not followed", path.display())
            }
            Some(libc::ENOTDIR | libc::ENOENT) => {
                bail!("{} is not a directory", path.display())
            }
            _ => Err(error).with_context(|| format!("{} cannot be read", path.display())),
        },
    }
}

/// Reads the skill's `SKILL.md` through the skill directory's descriptor: one
/// regular file, checked and bounded on the same opened handle that is read.
#[cfg(unix)]
fn read_skill_document(directory: &std::fs::File, path: &std::path::Path) -> Result<String> {
    let document = path.join("SKILL.md");
    let mut skill_file = match open_directory_member(
        directory,
        std::ffi::OsStr::new("SKILL.md"),
        libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
    ) {
        Ok(file) => file,
        Err(error) => match error.raw_os_error() {
            Some(libc::ENOENT) => bail!("{} has no SKILL.md", path.display()),
            Some(libc::ELOOP) => {
                bail!("{} is a symlink, which is not followed", document.display())
            }
            // A directory, or a socket that cannot be opened, is not a file.
            Some(libc::EISDIR | libc::ENXIO) => {
                bail!("{} is not a regular file", document.display())
            }
            _ => {
                return Err(error)
                    .with_context(|| format!("{} cannot be read", document.display()));
            }
        },
    };
    let metadata = skill_file
        .metadata()
        .with_context(|| format!("{} cannot be read", document.display()))?;
    if !metadata.is_file() {
        bail!("{} is not a regular file", document.display());
    }
    if metadata.len() > MAX_SKILL_DOCUMENT_BYTES {
        bail!(
            "{} is {} bytes; a skill document is at most {MAX_SKILL_DOCUMENT_BYTES} bytes",
            document.display(),
            metadata.len()
        );
    }
    let Some(bytes) = read_within_limit(&mut skill_file, MAX_SKILL_DOCUMENT_BYTES)
        .with_context(|| format!("{} cannot be read", document.display()))?
    else {
        bail!(
            "{} holds more than {MAX_SKILL_DOCUMENT_BYTES} bytes; a skill document is at most \
             {MAX_SKILL_DOCUMENT_BYTES} bytes",
            document.display()
        );
    };
    String::from_utf8(bytes).with_context(|| format!("{} is not readable text", document.display()))
}

/// Reads every file directly in the skill's `scripts/` folder, bounded the way
/// the Space bounds them.
///
/// A `scripts/` that is absent carries no scripts; one that is a symlink, or is
/// not a directory, is refused. Every entry is opened relative to the
/// `scripts/` descriptor and checked by its opened handle: a folder is not a
/// script, a symlink and a special file are refused, and a regular file's bytes
/// are read from the handle that was checked.
#[cfg(unix)]
fn read_skill_scripts(
    directory: &std::fs::File,
    path: &std::path::Path,
) -> Result<Vec<SkillAddScript>> {
    let scripts_dir = path.join("scripts");
    let Some(scripts) = open_scripts_directory(directory, &scripts_dir)? else {
        return Ok(Vec::new());
    };
    let mut names = Vec::new();
    for entry in std::fs::read_dir(descriptor_path(&scripts))
        .with_context(|| format!("{} cannot be read", scripts_dir.display()))?
    {
        let entry = entry.with_context(|| format!("{} cannot be read", scripts_dir.display()))?;
        names.push(entry.file_name());
    }
    names.sort();
    // Open every entry first: the opened handle's metadata decides what is a
    // script and what is refused, and the handles that pass are the ones read
    // below. No script's bytes are read until the count is within the limit.
    let mut opened: Vec<(std::ffi::OsString, std::fs::File, std::fs::Metadata)> = Vec::new();
    for name in names {
        let shown = scripts_dir.join(&name);
        let file = match open_directory_member(
            &scripts,
            &name,
            libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        ) {
            Ok(file) => file,
            Err(error) => match error.raw_os_error() {
                // The entry vanished between the listing and the open.
                Some(libc::ENOENT) => continue,
                Some(libc::ELOOP) => {
                    bail!("{} is a symlink, which is not followed", shown.display())
                }
                // A folder in `scripts/` is not a script, as it has never been one.
                Some(libc::EISDIR) => continue,
                // A socket cannot be opened, and is not a regular file.
                Some(libc::ENXIO) => bail!("{} is not a regular file", shown.display()),
                _ => {
                    return Err(error)
                        .with_context(|| format!("{} cannot be read", shown.display()));
                }
            },
        };
        let metadata = file
            .metadata()
            .with_context(|| format!("{} cannot be read", shown.display()))?;
        // A directory does not open through `O_NOFOLLOW` on every platform, so
        // the opened handle may be the first thing to say it is a folder.
        if metadata.is_dir() {
            continue;
        }
        if !metadata.is_file() {
            bail!("{} is not a regular file", shown.display());
        }
        opened.push((name, file, metadata));
        if opened.len() > MAX_SKILL_SCRIPTS {
            bail!("a skill carries at most {MAX_SKILL_SCRIPTS} scripts");
        }
    }
    let mut scripts = Vec::with_capacity(opened.len());
    for (name, mut file, metadata) in opened {
        let shown = scripts_dir.join(&name);
        if metadata.len() > MAX_SKILL_SCRIPT_BYTES {
            bail!(
                "{} is {} bytes; a script is at most {MAX_SKILL_SCRIPT_BYTES} bytes",
                shown.display(),
                metadata.len()
            );
        }
        let Some(bytes) = read_within_limit(&mut file, MAX_SKILL_SCRIPT_BYTES)
            .with_context(|| format!("{} cannot be read", shown.display()))?
        else {
            bail!(
                "{} holds more than {MAX_SKILL_SCRIPT_BYTES} bytes; a script is at most \
                 {MAX_SKILL_SCRIPT_BYTES} bytes",
                shown.display()
            );
        };
        let content = String::from_utf8(bytes)
            .with_context(|| format!("{} is not readable text", shown.display()))?;
        let name = name
            .to_str()
            .context("a script name is not text")?
            .to_owned();
        scripts.push(SkillAddScript { name, content });
    }
    Ok(scripts)
}

/// Opens the skill's `scripts/` folder relative to the skill directory.
/// `None` means there is no `scripts/` entry; a symlink there, or anything else
/// that is not a directory, is refused.
#[cfg(unix)]
fn open_scripts_directory(
    directory: &std::fs::File,
    shown: &std::path::Path,
) -> Result<Option<std::fs::File>> {
    let name = std::ffi::OsStr::new("scripts");
    match open_directory_member(
        directory,
        name,
        libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
    ) {
        Ok(scripts) => Ok(Some(scripts)),
        Err(error) => match error.raw_os_error() {
            Some(libc::ENOENT) => Ok(None),
            Some(libc::ELOOP) => bail!("{} is a symlink, which is not followed", shown.display()),
            Some(libc::ENOTDIR) if is_symlink(&descriptor_member_path(directory, name)) => {
                bail!("{} is a symlink, which is not followed", shown.display())
            }
            Some(libc::ENOTDIR) => bail!("{} is not a directory", shown.display()),
            _ => Err(error).with_context(|| format!("{} cannot be read", shown.display())),
        },
    }
}

/// Reads at most `limit` bytes from an opened file.
///
/// `None` means the file held more than `limit` bytes: its bytes are refused
/// rather than truncated or uploaded. The bound applies to the stream, not only
/// to the size the metadata reported before the read, so a file that grows
/// after that snapshot cannot grow past the limit.
#[cfg(unix)]
fn read_within_limit(file: &mut std::fs::File, limit: u64) -> std::io::Result<Option<Vec<u8>>> {
    use std::io::Read as _;

    let mut bytes = Vec::new();
    let mut bounded = std::io::Read::take(file, limit + 1);
    bounded.read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= limit).then_some(bytes))
}

/// One row per skill: skill, library, name, and what it does.
fn skill_rows(list: &Value) -> Result<Vec<String>> {
    list.as_array()
        .context("no skills in the answer")?
        .iter()
        .map(skill_row)
        .collect()
}

/// Formats one skill as a single line.
fn skill_row(skill: &Value) -> Result<String> {
    let library = match skill.get("library").and_then(Value::as_str) {
        Some("bot") => skill
            .get("bot")
            .and_then(Value::as_str)
            .map_or_else(|| "bot".to_owned(), |bot| format!("bot {bot}")),
        _ => "space".to_owned(),
    };
    let scripts = skill
        .get("scripts")
        .and_then(Value::as_array)
        .map_or(0, std::vec::Vec::len);
    Ok(format!(
        "{}  {library}  {}  {} script(s)  {}",
        entity_id(skill, &["id", "skill"])?,
        skill
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        scripts,
        skill
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
    ))
}

/// One row per MCP server: server, name, scope, who connected it, how the
/// Space reaches it, and connection state.
fn mcp_rows(list: &Value) -> Result<Vec<String>> {
    list.as_array()
        .context("no MCP servers in the answer")?
        .iter()
        .map(mcp_row)
        .collect()
}

/// Formats one MCP server as a single line.
fn mcp_row(server: &Value) -> Result<String> {
    Ok(format!(
        "{}  {}  {}  by {}  {}  {}",
        id_of(server, "server")?,
        server
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        mcp_scope_label(server),
        server.get("by").and_then(Value::as_str).unwrap_or("-"),
        mcp_reach_label(server),
        mcp_connection_label(server)
    ))
}

/// The way one MCP server view is reached: an HTTP server's address, or a
/// stdio server's transport and command line.
///
/// Every field the package supplies is escaped: a command, an argument, or an
/// address may hold a newline or a terminal escape, and the row must stay one
/// line the reader's terminal cannot rewrite.
fn mcp_reach_label(server: &Value) -> String {
    if server.get("transport").and_then(Value::as_str) != Some("stdio") {
        return escape_text(
            server
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
    }
    let mut line = String::from("stdio");
    if let Some(command) = server.get("command").and_then(Value::as_str) {
        line.push(' ');
        line.push_str(&escape_text(command));
    }
    if let Some(arguments) = server.get("args").and_then(Value::as_array) {
        for argument in arguments.iter().filter_map(Value::as_str) {
            line.push(' ');
            line.push_str(&escape_text(argument));
        }
    }
    line
}

/// The scope of one MCP server view: the Space, or the User who connected it.
fn mcp_scope_label(server: &Value) -> String {
    let scope = match server.get("scope").and_then(Value::as_str) {
        Some("user") => McpScope::User,
        _ => McpScope::Space,
    };
    let user = server
        .get("user")
        .and_then(Value::as_str)
        .and_then(|user| user.parse().ok());
    mcp_scope_text(scope, user)
}

/// Names where one MCP server's credential belongs: the Space, or the User who
/// connected it.
fn mcp_scope_text(scope: McpScope, user: Option<Id>) -> String {
    match (scope, user) {
        (McpScope::User, Some(user)) => format!("user {user}"),
        (McpScope::User, None) => "user".to_owned(),
        (McpScope::Space, _) => "space".to_owned(),
    }
}

/// Whether one MCP server view says the Space reached the server.
fn mcp_connection_label(server: &Value) -> &'static str {
    if server
        .get("connected")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        "connected"
    } else {
        "not connected"
    }
}

/// Gives a Bot a skill, or takes one away.
async fn bot_skill(command: BotSkillCommand) -> Result<()> {
    let credential = Credential::load()?;
    let (bot, skill, enabled) = match command {
        BotSkillCommand::Enable { bot, skill } => (bot, skill, true),
        BotSkillCommand::Disable { bot, skill } => (bot, skill, false),
    };
    let call = if enabled {
        CommandCall::BotSkillEnable { bot, skill }
    } else {
        CommandCall::BotSkillDisable { bot, skill }
    };
    let result = client::command(&credential, call).await?;
    let name = result
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("the skill");
    if enabled {
        println!("Bot {bot} may use skill {skill} ({name}).");
    } else {
        println!("Bot {bot} no longer uses skill {skill} ({name}).");
    }

    Ok(())
}

/// Gives a Bot an MCP server, or takes one away.
async fn bot_mcp(command: BotMcpCommand) -> Result<()> {
    let credential = Credential::load()?;
    let (bot, server, enabled) = match command {
        BotMcpCommand::Enable { bot, server } => (bot, server, true),
        BotMcpCommand::Disable { bot, server } => (bot, server, false),
    };
    let call = if enabled {
        CommandCall::BotMcpEnable { bot, server }
    } else {
        CommandCall::BotMcpDisable { bot, server }
    };
    let result = client::command(&credential, call).await?;
    let name = result
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("the server");
    if enabled {
        println!("Bot {bot} may use MCP server {server} ({name}).");
    } else {
        println!("Bot {bot} no longer uses MCP server {server} ({name}).");
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
    if let Some(server) = rule["server"].as_str() {
        let shown = rule["server_name"].as_str().unwrap_or(server);
        row.push_str(&format!("  server {shown}"));
    }
    if let Some(script) = rule["script"].as_str() {
        let skill = rule["skill_name"]
            .as_str()
            .or_else(|| rule["skill"].as_str())
            .unwrap_or_default();
        row.push_str(&format!("  skill {skill}  script {script}"));
    }

    Ok(row)
}

/// The line that reports one added rule.
#[allow(clippy::too_many_arguments)]
fn rule_add_line(
    rule: Id,
    bot: Id,
    effect: &str,
    tool: &str,
    command_prefix: Option<&str>,
    path_prefix: Option<&str>,
    skill: Option<&str>,
    script: Option<&str>,
    server: Option<&str>,
) -> String {
    let mut line = format!("Added rule {rule} for Bot {bot}: {effect} {tool}");
    if let Some(prefix) = command_prefix {
        line.push_str(&format!(" (command-prefix {prefix:?})"));
    }
    if let Some(prefix) = path_prefix {
        line.push_str(&format!(" (path-prefix {prefix:?})"));
    }
    if let (Some(skill), Some(script)) = (skill, script) {
        line.push_str(&format!(" (skill {skill:?}, script {script:?})"));
    }
    if let Some(server) = server {
        line.push_str(&format!(" (server {server:?})"));
    }

    line
}

/// The line that reports one added rule with the values the Space stored.
///
/// The Space normalizes a prefix before it records the rule; printing what the
/// Space returned shows the rule that actually exists, not the text typed.
#[allow(clippy::too_many_arguments)]
fn rule_add_report(
    result: &Value,
    bot: Id,
    effect: &str,
    tool: &str,
    command_prefix: Option<&str>,
    path_prefix: Option<&str>,
    skill: Option<&str>,
    script: Option<&str>,
    server: Option<&str>,
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

    // The answer names the server when the rule was about one; the typed
    // name is the fallback for an answer that predates the field.
    let server = result.get("server_name").and_then(Value::as_str).or(server);

    Ok(rule_add_line(
        rule,
        bot,
        effect,
        tool,
        command_prefix.as_deref(),
        path_prefix.as_deref(),
        skill,
        script,
        server,
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
    skill: Option<&str>,
    script: Option<&str>,
    server: Option<&str>,
) -> Result<()> {
    if command_prefix.is_some() && path_prefix.is_some() {
        bail!("pass at most one of --command-prefix and --path-prefix");
    }
    if let Some(server) = server {
        // A server's tool rule names the server and the tool and carries no
        // prefix, so a prefix with --server can only ever match nothing.
        if command_prefix.is_some() || path_prefix.is_some() {
            bail!("--server names a server's tool and takes no --command-prefix or --path-prefix");
        }
        if server.trim().is_empty() {
            bail!("--server must not be empty");
        }
    }
    let tool = tool.trim();
    if tool.is_empty() {
        bail!("--tool must not be empty");
    }
    let script_rule = skill.is_some() || script.is_some();
    if script_rule && tool != "run_skill_script" {
        bail!("--skill and --script match a skill's script; {tool} is not run_skill_script");
    }
    // A server's tool may be named anything, a built-in spelling included;
    // the server's id tells it from the built-in.
    if server.is_none() && tool == "run_skill_script" && !script_rule {
        bail!("--tool run_skill_script needs --skill and --script");
    }
    if server.is_some() && script_rule {
        bail!("a server rule names a server's tool, not a skill's script");
    }
    if script_rule && (command_prefix.is_some() || path_prefix.is_some()) {
        bail!("--skill and --script take no command or path prefix");
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
        if self.reason != "rule"
            && self.reason != "grant"
            && self.reason != "first_script_run"
            && self.reason != "first_mcp_call"
        {
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
        } else if self.reason == "first_script_run" {
            "the first run of a skill's script".to_owned()
        } else if self.reason == "first_mcp_call" {
            "the first call of an MCP server's tool".to_owned()
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

/// The credential an `mcp add` sends, which only the environment carries.
///
/// A credential argument would outlive the command in a shell history and a
/// process listing, so `NEHA_MCP_TOKEN` is its only source. Absent, the command
/// opens a browser sign-in for the credential instead; an attach without an
/// address asks for the token on the terminal, because a stdio server has no
/// address to sign in to.
fn mcp_credential() -> Option<String> {
    std::env::var("NEHA_MCP_TOKEN")
        .ok()
        .filter(|token| !token.is_empty())
}

/// The MCP credential an `mcp add` attach asks for when the environment
/// carries none.
///
/// Attaching the Space's credential to a package's stdio server has no address
/// to sign in to, so the command prompts for the token. A non-interactive run
/// must set `NEHA_MCP_TOKEN`: a prompt would read nothing and block.
fn read_mcp_credential() -> Result<String> {
    if !std::io::stdin().is_terminal() {
        bail!(
            "attaching the Space's MCP credential needs NEHA_MCP_TOKEN when stdin is not a \
             terminal"
        );
    }
    let token = rpassword::prompt_password("MCP access token: ")
        .context("cannot read the MCP credential")?;
    let token = token.trim().to_owned();
    if token.is_empty() {
        bail!("an MCP server's credential is not empty");
    }
    Ok(token)
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
    use std::time::Duration;

    use futures_util::{SinkExt as _, StreamExt as _};
    use neha::id::Id;
    use neha::record::McpScope;
    use neha::surface::{
        ClientFrame, CommandCall, McpAdd, McpSignInStart, PROTOCOL_VERSION, ServerFrame,
    };
    use serde_json::json;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio_tungstenite::tungstenite::Message;

    use crate::client::{Credential, SignedIn};

    use super::{space_file_name, write_run};

    use clap::CommandFactory as _;
    use clap::Parser as _;

    use super::{
        Cli, ComputerCommand, Top, computer_kind_label, pairing_hint, parse_bot_computer,
        parse_protocol, parse_thinking,
    };

    use super::{
        ApprovalCard, ApprovalCommand, Decision, LastingScope, RuleCommand, RuleEffectChoice,
        approval_row, decision_from_input, decision_line, escape_text, mcp_row, mcp_rows,
        read_decision, rule_add_line, rule_add_report, rule_rows, validate_rule_arguments,
        waiting_advice, write_approval_details,
    };

    use super::{
        BotMcpCommand, BotSkillCommand, CleanupStyle, McpCommand, SkillCommand, cleanup_notice,
        mcp_remove_lines, plugin_rows, read_skill_directory,
    };

    use super::{
        Callback, CallbackRequest, McpAddPlan, SIGN_IN_TIMEOUT, SignInReceiver, SignInState,
        answer_sign_in_page, callback_of, existing_server, finish_sign_in, mcp_add_plan,
        mcp_scope_text, mcp_sign_in_start, poll_sign_in, sign_in_command, sign_in_finish_command,
        sign_in_lines, sign_in_state,
    };

    use super::{
        BotCommand, MemoryCommand, byte_size, chat_read_rows, confirm_full_access, memory_rows,
        normalized_grant, visible_text, write_access,
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
    fn the_skill_commands_parse() {
        let bot = Id::new();
        let bot_text = bot.to_string();
        let skill = Id::new();
        let skill_text = skill.to_string();

        match parse(&["skill", "add", "/tmp/skills/weekly"]) {
            Top::Skill {
                command: SkillCommand::Add { path, bot: None },
            } => assert_eq!(path, std::path::Path::new("/tmp/skills/weekly")),
            _ => panic!("skill add did not parse"),
        }
        match parse(&[
            "skill",
            "add",
            "/tmp/skills/weekly",
            "--bot",
            bot_text.as_str(),
        ]) {
            Top::Skill {
                command:
                    SkillCommand::Add {
                        bot: Some(shown), ..
                    },
            } => assert_eq!(shown, bot),
            _ => panic!("skill add --bot did not parse"),
        }
        match parse(&["skill", "list"]) {
            Top::Skill {
                command: SkillCommand::List { bot: None },
            } => {}
            _ => panic!("skill list did not parse"),
        }
        match parse(&["skill", "remove", skill_text.as_str()]) {
            Top::Skill {
                command: SkillCommand::Remove { skill: shown },
            } => assert_eq!(shown, skill),
            _ => panic!("skill remove did not parse"),
        }
        match parse(&[
            "bot",
            "skill",
            "enable",
            bot_text.as_str(),
            skill_text.as_str(),
        ]) {
            Top::Bot {
                command:
                    BotCommand::Skill {
                        command:
                            BotSkillCommand::Enable {
                                bot: shown_bot,
                                skill: shown_skill,
                            },
                    },
            } => {
                assert_eq!(shown_bot, bot);
                assert_eq!(shown_skill, skill);
            }
            _ => panic!("bot skill enable did not parse"),
        }
        match parse(&[
            "bot",
            "skill",
            "disable",
            bot_text.as_str(),
            skill_text.as_str(),
        ]) {
            Top::Bot {
                command:
                    BotCommand::Skill {
                        command:
                            BotSkillCommand::Disable {
                                bot: shown_bot,
                                skill: shown_skill,
                            },
                    },
            } => {
                assert_eq!(shown_bot, bot);
                assert_eq!(shown_skill, skill);
            }
            _ => panic!("bot skill disable did not parse"),
        }
        assert!(Cli::try_parse_from(["neha", "skill", "add"]).is_err());
        assert!(Cli::try_parse_from(["neha", "bot", "skill", "enable"]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn the_mcp_commands_parse() {
        match parse(&["mcp", "add", "weather", "https://mcp.example.com/mcp"]) {
            Top::Mcp {
                command: McpCommand::Add { name, url, scope },
            } => {
                assert_eq!(name, "weather");
                assert_eq!(url.as_deref(), Some("https://mcp.example.com/mcp"));
                assert_eq!(scope.unwrap_or(McpScope::Space), McpScope::Space);
            }
            _ => panic!("mcp add did not parse"),
        }
        // The address is optional: an omitted one names an existing stdio
        // server the Admin attaches the Space's credential to.
        match parse(&["mcp", "add", "weather"]) {
            Top::Mcp {
                command: McpCommand::Add { name, url, .. },
            } => {
                assert_eq!(name, "weather");
                assert_eq!(url, None);
            }
            _ => panic!("mcp add without an address did not parse"),
        }
        match parse(&[
            "mcp",
            "add",
            "weather",
            "https://mcp.example.com/mcp",
            "--scope",
            "user",
        ]) {
            Top::Mcp {
                command: McpCommand::Add { scope, .. },
            } => assert_eq!(scope, Some(McpScope::User)),
            _ => panic!("mcp add --scope user did not parse"),
        }
        assert!(matches!(
            parse(&["mcp", "list"]),
            Top::Mcp {
                command: McpCommand::List
            }
        ));
        let server = Id::new();
        let server_text = server.to_string();
        match parse(&["mcp", "remove", server_text.as_str()]) {
            Top::Mcp {
                command: McpCommand::Remove { server: shown },
            } => assert_eq!(shown, server),
            _ => panic!("mcp remove did not parse"),
        }
        for line in [
            vec!["mcp", "remove"],
            vec![
                "mcp",
                "add",
                "weather",
                "https://mcp.example.com/mcp",
                "--scope",
                "space",
            ],
        ] {
            let refused = Cli::try_parse_from(std::iter::once("neha").chain(line.iter().copied()));
            assert!(refused.is_err(), "{line:?} must be refused");
        }
    }

    #[test]
    fn the_bot_mcp_commands_parse() {
        let bot = Id::new();
        let bot_text = bot.to_string();
        let server = Id::new();
        let server_text = server.to_string();
        match parse(&[
            "bot",
            "mcp",
            "enable",
            bot_text.as_str(),
            server_text.as_str(),
        ]) {
            Top::Bot {
                command:
                    BotCommand::Mcp {
                        command:
                            BotMcpCommand::Enable {
                                bot: shown_bot,
                                server: shown_server,
                            },
                    },
            } => {
                assert_eq!(shown_bot, bot);
                assert_eq!(shown_server, server);
            }
            _ => panic!("bot mcp enable did not parse"),
        }
        match parse(&[
            "bot",
            "mcp",
            "disable",
            bot_text.as_str(),
            server_text.as_str(),
        ]) {
            Top::Bot {
                command:
                    BotCommand::Mcp {
                        command:
                            BotMcpCommand::Disable {
                                bot: shown_bot,
                                server: shown_server,
                            },
                    },
            } => {
                assert_eq!(shown_bot, bot);
                assert_eq!(shown_server, server);
            }
            _ => panic!("bot mcp disable did not parse"),
        }
        assert!(Cli::try_parse_from(["neha", "bot", "mcp", "enable"]).is_err());
    }

    #[test]
    fn a_skill_directory_reads_its_document_and_scripts() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("SKILL.md"),
            "---\nname: hello\ndescription: Say hello.\n---\nRun it.",
        )
        .unwrap();
        std::fs::create_dir(directory.path().join("scripts")).unwrap();
        std::fs::write(
            directory.path().join("scripts").join("hello.sh"),
            "printf hi",
        )
        .unwrap();
        std::fs::write(directory.path().join("notes.txt"), "not a script").unwrap();
        let (document, scripts) = read_skill_directory(directory.path()).unwrap();
        assert!(document.contains("name: hello"));
        assert_eq!(scripts.len(), 1, "only scripts/ holds scripts: {scripts:?}");
        assert_eq!(scripts[0].name, "hello.sh");
        assert_eq!(scripts[0].content, "printf hi");

        // A directory without SKILL.md is not a skill.
        let empty = tempfile::tempdir().unwrap();
        assert!(read_skill_directory(empty.path()).is_err());
        assert!(read_skill_directory(&directory.path().join("missing")).is_err());
    }

    /// A platform without directory descriptors refuses a directory import
    /// instead of uploading whatever a racing writer put at a path.
    #[cfg(not(unix))]
    #[test]
    fn a_skill_directory_import_is_refused_off_unix() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("SKILL.md"),
            "---\nname: hello\ndescription: Say hello.\n---\nRun it.",
        )
        .unwrap();
        let error = read_skill_directory(directory.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("Unix platform"), "{error}");
    }

    /// A skill directory with a `SKILL.md` and one script in `scripts/`.
    #[cfg(unix)]
    fn skill_directory() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("SKILL.md"),
            "---\nname: hello\ndescription: Say hello.\n---\nRun it.",
        )
        .unwrap();
        std::fs::create_dir(directory.path().join("scripts")).unwrap();
        std::fs::write(
            directory.path().join("scripts").join("hello.sh"),
            "printf hi",
        )
        .unwrap();
        directory
    }

    /// A skill directory that is itself a symlink is refused: the link could
    /// name a folder the user never picked.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_skill_directory_is_refused() {
        let real = skill_directory();
        let holder = tempfile::tempdir().unwrap();
        let link = holder.path().join("skill");
        std::os::unix::fs::symlink(real.path(), &link).unwrap();

        let error = read_skill_directory(&link).unwrap_err().to_string();
        assert!(error.contains("is a symlink"), "{error}");
    }

    /// A `SKILL.md` that is a symlink is refused, so a skill cannot carry a
    /// document that lives outside the directory the user picked.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_skill_document_is_refused() {
        let directory = skill_directory();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(
            outside.path().join("elsewhere.md"),
            "---\nname: away\ndescription: Not picked.\n---\n",
        )
        .unwrap();
        std::fs::remove_file(directory.path().join("SKILL.md")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("elsewhere.md"),
            directory.path().join("SKILL.md"),
        )
        .unwrap();

        let error = read_skill_directory(directory.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("is a symlink"), "{error}");
    }

    /// A `scripts/` that is a symlink is refused, so a skill cannot carry
    /// scripts that live outside the directory the user picked.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_scripts_folder_is_refused() {
        let directory = skill_directory();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("away.sh"), "printf away").unwrap();
        std::fs::remove_dir_all(directory.path().join("scripts")).unwrap();
        std::os::unix::fs::symlink(outside.path(), directory.path().join("scripts")).unwrap();

        let error = read_skill_directory(directory.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("is a symlink"), "{error}");
    }

    /// A script that is a symlink is refused, whatever it points at.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_script_is_refused() {
        let directory = skill_directory();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("away.sh"), "printf away").unwrap();
        std::fs::remove_file(directory.path().join("scripts").join("hello.sh")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("away.sh"),
            directory.path().join("scripts").join("hello.sh"),
        )
        .unwrap();

        let error = read_skill_directory(directory.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("is a symlink"), "{error}");
    }

    /// A script that is not a regular file is refused: reading a FIFO can block
    /// forever, and a socket or a device is not a file the user picked.
    #[test]
    #[cfg(unix)]
    fn a_script_that_is_not_a_regular_file_is_refused() {
        let directory = skill_directory();
        let fifo = directory.path().join("scripts").join("pipe.sh");
        let Ok(made) = std::process::Command::new("mkfifo").arg(&fifo).status() else {
            return; // No mkfifo here, so the check is not exercised.
        };
        assert!(made.success(), "mkfifo did not make a FIFO");

        let error = read_skill_directory(directory.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("is not a regular file"), "{error}");
    }

    /// A `SKILL.md` past the bound is refused before it is read, and the
    /// message names the limit.
    #[test]
    #[cfg(unix)]
    fn an_oversized_skill_document_is_refused() {
        let directory = skill_directory();
        std::fs::write(
            directory.path().join("SKILL.md"),
            "x".repeat(super::MAX_SKILL_DOCUMENT_BYTES as usize + 1),
        )
        .unwrap();

        let error = read_skill_directory(directory.path())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&format!(
                "at most {} bytes",
                super::MAX_SKILL_DOCUMENT_BYTES
            )),
            "{error}"
        );
    }

    /// A script past the bound is refused before it is read, and the message
    /// names the limit.
    #[test]
    #[cfg(unix)]
    fn an_oversized_script_is_refused() {
        let directory = skill_directory();
        std::fs::write(
            directory.path().join("scripts").join("hello.sh"),
            "x".repeat(super::MAX_SKILL_SCRIPT_BYTES as usize + 1),
        )
        .unwrap();

        let error = read_skill_directory(directory.path())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&format!("at most {} bytes", super::MAX_SKILL_SCRIPT_BYTES)),
            "{error}"
        );
    }

    /// A file that holds more than the limit is refused by the read bound
    /// itself, not only by the size the metadata reported before the read, so a
    /// file that grows after that snapshot cannot be uploaded past the bound.
    #[test]
    #[cfg(unix)]
    fn a_file_that_grows_past_the_limit_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let grown = directory.path().join("grown.sh");
        std::fs::write(
            &grown,
            "x".repeat(super::MAX_SKILL_SCRIPT_BYTES as usize + 1),
        )
        .unwrap();
        let mut file = std::fs::File::open(&grown).unwrap();
        assert!(
            super::read_within_limit(&mut file, super::MAX_SKILL_SCRIPT_BYTES)
                .unwrap()
                .is_none(),
            "a file holding more than the limit is refused"
        );

        // A file exactly at the limit is still read whole.
        let exact = directory.path().join("exact.sh");
        std::fs::write(&exact, "x".repeat(super::MAX_SKILL_SCRIPT_BYTES as usize)).unwrap();
        let mut file = std::fs::File::open(&exact).unwrap();
        let bytes = super::read_within_limit(&mut file, super::MAX_SKILL_SCRIPT_BYTES)
            .unwrap()
            .expect("a file at the limit is read");
        assert_eq!(bytes.len() as u64, super::MAX_SKILL_SCRIPT_BYTES);
    }

    /// More scripts than a skill carries is refused before any of them is read.
    #[test]
    #[cfg(unix)]
    fn too_many_scripts_are_refused() {
        let directory = skill_directory();
        for index in 0..=super::MAX_SKILL_SCRIPTS {
            std::fs::write(
                directory
                    .path()
                    .join("scripts")
                    .join(format!("script-{index:02}.sh")),
                "printf hi",
            )
            .unwrap();
        }

        let error = read_skill_directory(directory.path())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&format!("at most {} scripts", super::MAX_SKILL_SCRIPTS)),
            "{error}"
        );
    }

    /// A skill with no `scripts/` folder, or one holding only folders, carries
    /// no scripts.
    #[test]
    #[cfg(unix)]
    fn a_skill_without_scripts_carries_none() {
        let bare = skill_directory();
        std::fs::remove_dir_all(bare.path().join("scripts")).unwrap();
        let (_, scripts) = read_skill_directory(bare.path()).unwrap();
        assert!(scripts.is_empty(), "{scripts:?}");

        let folders = skill_directory();
        std::fs::create_dir(folders.path().join("scripts").join("helpers")).unwrap();
        std::fs::write(
            folders
                .path()
                .join("scripts")
                .join("helpers")
                .join("helper.sh"),
            "printf help",
        )
        .unwrap();
        let (_, scripts) = read_skill_directory(folders.path()).unwrap();
        assert_eq!(scripts.len(), 1, "only scripts/ holds scripts: {scripts:?}");
        assert_eq!(scripts[0].name, "hello.sh");
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
                        skill,
                        script,
                        server,
                    },
            } => {
                assert_eq!(shown, bot);
                assert_eq!(effect, RuleEffectChoice::Ask);
                assert_eq!(tool, "shell");
                assert_eq!(command_prefix.as_deref(), Some("git"));
                assert_eq!(path_prefix, None);
                assert_eq!((skill, script), (None, None));
                assert_eq!(server, None);
            }
            _ => panic!("rule add did not parse"),
        }
        match parse(&[
            "rule",
            "add",
            "--bot",
            bot_text.as_str(),
            "--effect",
            "allow",
            "--tool",
            "get_weather",
            "--server",
            "weather",
        ]) {
            Top::Rule {
                command: RuleCommand::Add { tool, server, .. },
            } => {
                assert_eq!(tool, "get_weather");
                assert_eq!(server.as_deref(), Some("weather"));
            }
            _ => panic!("rule add --server did not parse"),
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
    fn the_bot_memory_commands_parse_their_contract_shapes() {
        let bot = Id::new();
        let bot_text = bot.to_string();
        let entry = Id::new();
        let entry_text = entry.to_string();

        // `neha bot memory <bot>` lists one Bot's memory.
        match parse(&["bot", "memory", bot_text.as_str()]) {
            Top::Bot {
                command:
                    BotCommand::Memory {
                        bot: shown,
                        command,
                    },
            } => {
                assert_eq!(shown, Some(bot));
                assert!(command.is_none(), "the list shape names no subcommand");
            }
            _ => panic!("bot memory did not parse"),
        }

        // `neha bot memory remove <bot> <entry>` removes one entry.
        match parse(&[
            "bot",
            "memory",
            "remove",
            bot_text.as_str(),
            entry_text.as_str(),
        ]) {
            Top::Bot {
                command:
                    BotCommand::Memory {
                        bot: listed,
                        command:
                            Some(MemoryCommand::Remove {
                                bot: shown,
                                entry: removed,
                            }),
                    },
            } => {
                assert_eq!(listed, None, "the nested shape names the Bot after remove");
                assert_eq!(shown, bot);
                assert_eq!(removed, entry);
            }
            _ => panic!("bot memory remove did not parse"),
        }

        // `remove` needs both ids; a missing one is refused by the parser.
        assert!(Cli::try_parse_from(["neha", "bot", "memory", "remove"]).is_err());
        assert!(
            Cli::try_parse_from(["neha", "bot", "memory", "remove", bot_text.as_str()]).is_err()
        );
    }

    #[test]
    fn a_rule_prefix_is_checked_against_its_tool() {
        validate_rule_arguments("shell", None, None, None, None, None).unwrap();
        validate_rule_arguments("shell", Some("git"), None, None, None, None).unwrap();
        validate_rule_arguments("read_file", None, Some("/home/ada"), None, None, None).unwrap();
        validate_rule_arguments("write_file", None, Some("."), None, None, None).unwrap();
        validate_rule_arguments("get_weather", None, None, None, None, Some("weather")).unwrap();
        validate_rule_arguments(
            "run_skill_script",
            None,
            None,
            Some("weekly-report"),
            Some("collect.sh"),
            None,
        )
        .unwrap();

        for (tool, command_prefix, path_prefix, expected) in [
            ("shell", Some("git"), Some("/tmp"), "at most one"),
            ("read_file", Some("git"), None, "--command-prefix"),
            ("shell", None, Some("/tmp"), "--path-prefix"),
            ("shell", None, Some("/tmp"), "--path-prefix"),
            ("shell", Some("  "), None, "must not be empty"),
            ("read_file", None, Some(" "), "must not be empty"),
            ("   ", None, None, "--tool"),
        ] {
            let error =
                validate_rule_arguments(tool, command_prefix, path_prefix, None, None, None)
                    .unwrap_err()
                    .to_string();
            assert!(error.contains(expected), "{tool:?}: {error}");
        }
        for (tool, command_prefix, path_prefix, skill, script, expected) in [
            (
                "shell",
                None,
                None,
                Some("weekly-report"),
                Some("collect.sh"),
                "not run_skill_script",
            ),
            ("run_skill_script", None, None, None, None, "needs --skill"),
            (
                "run_skill_script",
                Some("git"),
                None,
                Some("weekly-report"),
                Some("collect.sh"),
                "no command or path prefix",
            ),
        ] {
            let error =
                validate_rule_arguments(tool, command_prefix, path_prefix, skill, script, None)
                    .unwrap_err()
                    .to_string();
            assert!(error.contains(expected), "{tool:?}: {error}");
        }
        // A server's tool rule names the server and the tool and carries no
        // prefix; a prefix can only ever match nothing, so it is refused here.
        for (server, command_prefix, path_prefix, expected) in [
            (Some("weather"), Some("git"), None, "no --command-prefix"),
            (Some("weather"), None, Some("/tmp"), "no --command-prefix"),
            (Some("  "), None, None, "must not be empty"),
        ] {
            let error = validate_rule_arguments(
                "get_weather",
                command_prefix,
                path_prefix,
                None,
                None,
                server,
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains(expected), "{server:?}: {error}");
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
        // A control character renders as its escaped form, and the escape
        // character itself is escaped so a literal `\n` cannot pass for a
        // newline.
        assert_eq!(
            escape_text("a\nb\rc\td\u{001b}[31m\\e\u{0007}f"),
            "a\\nb\\rc\\td\\u{001b}[31m\\\\e\\u{0007}f"
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
        let skill = Id::new();
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
        let script_rule = json!([{
            "rule": rule,
            "bot": bot,
            "effect": "allow",
            "tool": "run_skill_script",
            "skill": skill,
            "skill_name": "weekly-report",
            "script": "collect.sh",
        }]);
        assert_eq!(
            rule_rows(&script_rule).unwrap(),
            vec![format!(
                "{rule}  {bot}  allow  run_skill_script  skill weekly-report  script collect.sh"
            )]
        );
        assert_eq!(rule_rows(&json!([])).unwrap(), Vec::<String>::new());
        assert_eq!(
            rule_add_line(
                rule,
                bot,
                "ask",
                "shell",
                Some("git"),
                None,
                None,
                None,
                None
            ),
            format!("Added rule {rule} for Bot {bot}: ask shell (command-prefix \"git\")")
        );
        assert_eq!(
            rule_add_line(
                rule,
                bot,
                "allow",
                "read_file",
                None,
                Some("/tmp"),
                None,
                None,
                None
            ),
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
    fn plugin_install_help_states_the_limits_and_source_rules() {
        let mut cli = Cli::command();
        let plugin = cli.find_subcommand_mut("plugin").expect("a plugin command");
        let install = plugin
            .find_subcommand_mut("install")
            .expect("a plugin install command");
        // Help wraps to the terminal; compare phrases, not line breaks.
        let help = install
            .render_long_help()
            .to_string()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for stated in [
            "200 files",
            "1024 entries in all (files and directories)",
            "100 KiB per file",
            "4 MiB per package",
            "<url>@<40-or-64-hex-commit>",
            "40- or 64-character lowercase hexadecimal object id",
            "A directory source is a plain path",
            "supported only on Linux",
            "install from a repository",
            "[plugins] sources",
        ] {
            assert!(
                help.contains(stated),
                "`plugin install --help` does not state {stated:?}:\n{help}"
            );
        }
    }

    #[test]
    fn a_cleanup_warning_is_reported_escaped_on_one_line() {
        // A result with no cleanup, or a blank or non-text one, has nothing to
        // report.
        let clean = json!({"plugin": Id::new()});
        assert_eq!(cleanup_notice(&clean, CleanupStyle::Detail), None);
        assert_eq!(cleanup_notice(&clean, CleanupStyle::Retry), None);
        assert_eq!(
            cleanup_notice(&json!({"cleanup": "  "}), CleanupStyle::Detail),
            None
        );
        assert_eq!(
            cleanup_notice(&json!({"cleanup": 7}), CleanupStyle::Retry),
            None
        );

        // The Space sanitizes the text; the CLI still escapes it, so a warning
        // stays one line and cannot rewrite the terminal.
        let warning = "incomplete: a package's files could not be deleted\n\u{1b}[31m";
        let result = json!({"cleanup": warning});
        assert_eq!(
            cleanup_notice(&result, CleanupStyle::Detail).unwrap(),
            "  cleanup: incomplete: a package's files could not be deleted\\n\\u{001b}[31m"
        );
        assert_eq!(
            cleanup_notice(&result, CleanupStyle::Retry).unwrap(),
            "Cleanup incomplete: a package's files could not be deleted\\n\\u{001b}[31m. Run the command again to finish it."
        );
        // A warning that does not carry the Space's prefix is still a warning.
        assert_eq!(
            cleanup_notice(
                &json!({"cleanup": "a package's files could not be deleted"}),
                CleanupStyle::Retry
            )
            .unwrap(),
            "Cleanup incomplete: a package's files could not be deleted. Run the command again to finish it."
        );
        // A retried removal that finished its cleanup says so, and does not ask
        // for another retry.
        assert_eq!(
            cleanup_notice(&json!({"cleanup": "finished"}), CleanupStyle::Retry).unwrap(),
            "Cleanup finished."
        );
    }

    #[test]
    fn plugin_rows_show_what_a_package_brought() {
        let plugin = Id::new();
        let server = Id::new();
        let skill = Id::new();
        let digest = "a".repeat(64);
        let commit = "3f2a1c9d8e4b5a6f7c8d9e0f1a2b3c4d5e6f7a8b";
        let list = json!([{
            "plugin": plugin,
            "name": "reports",
            "version": "1.2.0",
            "digest": digest,
            "commit": commit,
            "source": format!("https://github.com/acme/reports@{commit}"),
            "enabled": true,
            "servers": [{"server": server, "name": "reports-docs"}],
            "skills": [{"skill": skill, "name": "weekly-report"}],
        }]);
        assert_eq!(
            plugin_rows(&list).unwrap(),
            vec![format!(
                "{plugin}  reports  1.2.0  {commit}  digest: {digest}  enabled  servers: reports-docs  skills: weekly-report"
            )]
        );
        // A package that brought nothing, and a disabled one with no version.
        let bare = json!([{
            "plugin": plugin,
            "name": "bare",
            "enabled": false,
            "servers": [],
            "skills": [],
        }]);
        assert_eq!(
            plugin_rows(&bare).unwrap(),
            vec![format!(
                "{plugin}  bare  -  -  digest: -  disabled  servers: -  skills: -"
            )]
        );
        assert!(plugin_rows(&json!({})).is_err());
    }

    #[test]
    fn mcp_rows_show_the_server_scope_reach_and_connection() {
        let server = Id::new();
        let local = Id::new();
        let user_server = Id::new();
        let user = Id::new();
        let by = Id::new();
        let list = json!([
            {
                "server": server,
                "name": "weather",
                "transport": "streamable-http",
                "url": "https://mcp.example.com/mcp",
                "scope": "space",
                "by": by,
                "connected": true,
            },
            {
                "server": local,
                "name": "notes",
                "transport": "stdio",
                "url": null,
                "command": "./bin/notes",
                "args": ["--root", "${PLUGIN_DATA}", "--label", "My notes"],
                "scope": "space",
                "by": by,
                "connected": false,
            },
            {
                "server": user_server,
                "name": "mine",
                "transport": "streamable-http",
                "url": "http://127.0.0.1:9000/mcp",
                "scope": "user",
                "user": user,
                "by": user,
                "connected": false,
            },
        ]);
        assert_eq!(
            mcp_rows(&list).unwrap(),
            vec![
                format!(
                    "{server}  weather  space  by {by}  https://mcp.example.com/mcp  connected"
                ),
                format!(
                    "{local}  notes  space  by {by}  stdio ./bin/notes --root ${{PLUGIN_DATA}} --label My notes  not connected"
                ),
                format!(
                    "{user_server}  mine  user {user}  by {user}  http://127.0.0.1:9000/mcp  not connected"
                ),
            ]
        );
        // A row written before the transport was named still reads as an
        // address, and a stdio row without arguments prints its command alone.
        assert_eq!(
            mcp_row(&json!({"server": server, "name": "bare", "url": "https://x/"})).unwrap(),
            format!("{server}  bare  space  by -  https://x/  not connected")
        );
        assert_eq!(
            mcp_row(&json!({"server": local, "name": "solo", "transport": "stdio", "command": "./bin/solo"}))
                .unwrap(),
            format!("{local}  solo  space  by -  stdio ./bin/solo  not connected")
        );
        assert_eq!(mcp_rows(&json!([])).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn mcp_rows_escape_the_package_controlled_reach_fields() {
        let server = Id::new();
        // A command and an argument are package data: they may hold a
        // newline, a terminal escape, or a backslash that would otherwise
        // stand for one. The row stays one plain line.
        let row = mcp_row(&json!({
            "server": server,
            "name": "notes",
            "transport": "stdio",
            "command": "./bin/no\ntes",
            "args": ["--title", "a\nb\u{001b}[2Jc\\d", "\u{0007}bell"],
        }))
        .unwrap();
        assert_eq!(
            row,
            format!(
                "{server}  notes  space  by -  stdio ./bin/no\\ntes --title a\\nb\\u{{001b}}[2Jc\\\\d \\u{{0007}}bell  not connected"
            )
        );
        assert!(
            row.chars().all(|ch| !ch.is_control()),
            "no raw control character reaches the terminal: {row:?}"
        );

        // An address is package data too, and an ordinary one is unchanged.
        assert_eq!(
            mcp_row(&json!({
                "server": server,
                "name": "web",
                "transport": "streamable-http",
                "url": "https://x/\u{001b}[2J\\",
            }))
            .unwrap(),
            format!("{server}  web  space  by -  https://x/\\u{{001b}}[2J\\\\  not connected")
        );
        assert_eq!(
            mcp_row(&json!({"server": server, "name": "docs", "url": "https://x/mcp"})).unwrap(),
            format!("{server}  docs  space  by -  https://x/mcp  not connected")
        );
    }

    #[test]
    fn an_mcp_removal_reports_the_cleanup_its_answer_carries() {
        // A removal the Space finished its cleanup for leaves one line.
        let clean = json!({"server": Id::new(), "name": "weather"});
        assert_eq!(
            mcp_remove_lines(&clean),
            vec!["Removed MCP server weather.".to_owned()]
        );

        // A cleanup the Space still owes is reported on the next line, escaped
        // into one line so a warning cannot rewrite the terminal, and names the
        // retry that finishes it.
        let owes = json!({
            "server": Id::new(),
            "name": "weather",
            "cleanup": "incomplete: an MCP server's Vault credential could not be discarded\n\u{1b}[31m",
        });
        let lines = mcp_remove_lines(&owes);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "Removed MCP server weather.");
        assert_eq!(
            lines[1],
            "Cleanup incomplete: an MCP server's Vault credential could not be discarded\\n\\u{001b}[31m. Run the command again to finish it."
        );

        // A retry that finished the cleanup says so, and asks for no other.
        let finished = json!({"server": Id::new(), "name": "weather", "cleanup": "finished"});
        assert_eq!(
            mcp_remove_lines(&finished),
            vec![
                "Removed MCP server weather.".to_owned(),
                "Cleanup finished.".to_owned(),
            ]
        );

        // The Space may hold no name for the server; the line still says which
        // server was removed.
        assert_eq!(
            mcp_remove_lines(&json!({"server": Id::new()})),
            vec!["Removed MCP server the server.".to_owned()]
        );
    }

    #[test]
    fn an_mcp_add_sends_the_environment_token_or_starts_a_sign_in() {
        // The environment's token connects at once, as it did before the
        // browser sign-in existed.
        assert_eq!(
            mcp_add_plan(
                "weather",
                "https://mcp.example.com/mcp",
                McpScope::Space,
                Some("secret".to_owned()),
            ),
            McpAddPlan::Add(McpAdd {
                name: "weather".to_owned(),
                url: "https://mcp.example.com/mcp".to_owned(),
                scope: McpScope::Space,
                credential: Some("secret".to_owned()),
            })
        );

        // An omitted address is passed through empty: the Space reads it as
        // the existing package stdio server of that name, so the plan names no
        // address and the command creates nothing.
        assert_eq!(
            mcp_add_plan("local", "", McpScope::Space, Some("secret".to_owned())),
            McpAddPlan::Add(McpAdd {
                name: "local".to_owned(),
                url: String::new(),
                scope: McpScope::Space,
                credential: Some("secret".to_owned()),
            })
        );

        // Without one the command binds its loopback receiver and opens the
        // browser.
        assert_eq!(
            mcp_add_plan("weather", "http://127.0.0.1:9000/mcp", McpScope::User, None),
            McpAddPlan::SignIn
        );

        // A new server: the sign-in names the server and scope the Space
        // registers when it finishes, and binds the command's loopback
        // receiver as the browser's way back.
        let redirect = "http://127.0.0.1:4711/callback";
        assert_eq!(
            mcp_sign_in_start(
                "weather",
                "http://127.0.0.1:9000/mcp",
                McpScope::User,
                redirect,
                None,
            ),
            McpSignInStart {
                server: None,
                name: Some("weather".to_owned()),
                url: Some("http://127.0.0.1:9000/mcp".to_owned()),
                scope: Some(McpScope::User),
                redirect_uri: Some(redirect.to_owned()),
            }
        );

        // A server the Space already holds under the same name and address
        // takes the caller's own credential, so the sign-in names only that
        // server.
        let existing = Id::new();
        assert_eq!(
            mcp_sign_in_start(
                "weather",
                "http://127.0.0.1:9000/mcp",
                McpScope::User,
                redirect,
                Some(existing),
            ),
            McpSignInStart {
                server: Some(existing),
                name: None,
                url: None,
                scope: None,
                redirect_uri: Some(redirect.to_owned()),
            }
        );
    }

    #[test]
    fn an_mcp_add_address_is_optional_and_its_help_states_the_attach() {
        let Top::Mcp {
            command: McpCommand::Add { name, url, .. },
        } = parse(&["mcp", "add", "local"])
        else {
            panic!("`mcp add <name>` parses");
        };
        assert_eq!(name, "local");
        assert_eq!(url, None, "an omitted address is an attach");

        let Top::Mcp {
            command: McpCommand::Add { url, .. },
        } = parse(&["mcp", "add", "docs", "https://docs.example.com/mcp"])
        else {
            panic!("`mcp add <name> <url>` parses");
        };
        assert_eq!(url.as_deref(), Some("https://docs.example.com/mcp"));

        // The help says what an omitted address attaches, and who may do it.
        let mut cli = Cli::command();
        let mcp = cli.find_subcommand_mut("mcp").expect("an mcp command");
        let add = mcp.find_subcommand_mut("add").expect("an mcp add command");
        let help = add
            .render_long_help()
            .to_string()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for stated in [
            "omit it to attach the Space credential",
            "package's existing stdio server of this name",
            "only the Admin may do that",
            "asked for when the environment carries none",
        ] {
            assert!(
                help.contains(stated),
                "`mcp add --help` does not state {stated:?}:\n{help}"
            );
        }
    }

    #[test]
    fn an_existing_mcp_server_is_matched_by_name_and_address() {
        let server = Id::new();
        let list = json!([
            {"server": Id::new(), "name": "weather", "url": "https://mcp.example.com/mcp"},
            {"server": server, "name": "notes", "url": "http://127.0.0.1:9000/mcp"},
        ]);

        // The same name and address is the server the sign-in attaches to; the
        // command line's address is trimmed like the Space trims it.
        assert_eq!(
            existing_server(&list, "notes", "http://127.0.0.1:9000/mcp").unwrap(),
            Some(server)
        );
        assert_eq!(
            existing_server(&list, "notes", " http://127.0.0.1:9000/mcp ").unwrap(),
            Some(server)
        );

        // A name the Space does not hold starts a new server.
        assert_eq!(
            existing_server(&list, "docs", "http://127.0.0.1:9001/mcp").unwrap(),
            None
        );

        // A name held at another address is a refusal: the sign-in would
        // attach to a server the command did not name.
        let error = existing_server(&list, "weather", "https://other.example.com/mcp")
            .unwrap_err()
            .to_string();
        assert!(error.contains("already holds"), "{error}");
        assert!(error.contains("weather"), "{error}");
    }

    #[test]
    fn a_sign_in_finish_command_carries_the_keyed_callback_path() {
        let sign_in = Id::new();
        let callback = Callback::Code {
            code: "the-code".to_owned(),
            state: "the-state".to_owned(),
            iss: Some("https://auth.example.com".to_owned()),
        };

        // The command sends the raw path the browser requested with the code
        // and state: the Space compares it with the issuer-keyed redirect it
        // registered.
        assert_eq!(
            sign_in_finish_command(sign_in, &callback, Some("/0123456789abcdef/callback")),
            Some(CommandCall::McpSignInFinish {
                sign_in,
                code: "the-code".to_owned(),
                state: "the-state".to_owned(),
                iss: Some("https://auth.example.com".to_owned()),
                path: Some("/0123456789abcdef/callback".to_owned()),
            })
        );

        // A callback that returned no code finishes nothing: no finish command
        // is sent, so the receiver's path is forwarded only with a code.
        assert_eq!(
            sign_in_finish_command(sign_in, &Callback::Refused, Some("/callback")),
            None
        );

        // The path stays absent when the request line named none.
        assert_eq!(
            sign_in_finish_command(
                sign_in,
                &Callback::Code {
                    code: "a".to_owned(),
                    state: "b".to_owned(),
                    iss: None,
                },
                None,
            ),
            Some(CommandCall::McpSignInFinish {
                sign_in,
                code: "a".to_owned(),
                state: "b".to_owned(),
                iss: None,
                path: None,
            })
        );
    }

    #[test]
    fn a_browser_callback_reads_the_keyed_path_and_values_or_refuses() {
        // The redirect is the issuer-keyed callback path the Space registered,
        // and its query carries the code and the state; both are
        // percent-decoded.
        assert_eq!(
            callback_of(
                b"GET /0123456789abcdef/callback?code=the%2Dcode&state=the%2Dstate HTTP/1.1\r\n\
                  Host: 127.0.0.1:4711\r\n\r\n"
            ),
            CallbackRequest {
                path: Some("/0123456789abcdef/callback".to_owned()),
                callback: Callback::Code {
                    code: "the-code".to_owned(),
                    state: "the-state".to_owned(),
                    iss: None,
                },
            }
        );

        // The receiver accepts any path: the issuer key is the Space's to
        // compare, so the path is kept exactly as received.
        assert_eq!(
            callback_of(b"GET /callback?code=a&state=b HTTP/1.1\r\n\r\n"),
            CallbackRequest {
                path: Some("/callback".to_owned()),
                callback: Callback::Code {
                    code: "a".to_owned(),
                    state: "b".to_owned(),
                    iss: None,
                },
            }
        );

        // The path is not decoded: percent-encoding reaches the Space as the
        // browser sent it.
        assert_eq!(
            callback_of(b"GET /a%2Fb/callback?code=a&state=b HTTP/1.1\r\n\r\n"),
            CallbackRequest {
                path: Some("/a%2Fb/callback".to_owned()),
                callback: Callback::Code {
                    code: "a".to_owned(),
                    state: "b".to_owned(),
                    iss: None,
                },
            }
        );

        // A refusal is one fixed outcome: the authorization server's own
        // description never survives it. The path is still the one requested.
        assert_eq!(
            callback_of(
                b"GET /0123456789abcdef/callback?error=access_denied&\
                  error_description=the%20user%20said%20no HTTP/1.1\r\n\r\n"
            ),
            CallbackRequest {
                path: Some("/0123456789abcdef/callback".to_owned()),
                callback: Callback::Refused,
            }
        );

        // A callback without both values ends the wait as a failure, and its
        // request target still names the path the Space compares.
        for request in [
            &b"GET /callback?code=the-code HTTP/1.1\r\n\r\n"[..],
            &b"GET /callback?state=the-state HTTP/1.1\r\n\r\n"[..],
        ] {
            assert_eq!(
                callback_of(request),
                CallbackRequest {
                    path: Some("/callback".to_owned()),
                    callback: Callback::Refused,
                }
            );
        }

        // A request the receiver did not register for is refused with no path
        // at all: only origin-form targets name the redirect a browser sends.
        for request in [
            &b"POST /callback?code=a&state=b HTTP/1.1\r\n\r\n"[..],
            &b"not a request\r\n\r\n"[..],
            // An absolute-form target is what a proxy sends, not a browser.
            &b"GET http://127.0.0.1:4711/callback?code=a&state=b HTTP/1.1\r\n\r\n"[..],
            // A target without a leading slash is no origin-form path.
            &b"GET callback?code=a&state=b HTTP/1.1\r\n\r\n"[..],
            &b"GET ?code=a&state=b HTTP/1.1\r\n\r\n"[..],
            &b"GET * HTTP/1.1\r\n\r\n"[..],
            // A fragment is never part of a request target.
            &b"GET /callback#fragment?code=a&state=b HTTP/1.1\r\n\r\n"[..],
            &b"GET /callback?code=a&state=b#fragment HTTP/1.1\r\n\r\n"[..],
        ] {
            assert_eq!(
                callback_of(request),
                CallbackRequest {
                    path: None,
                    callback: Callback::Refused,
                },
                "{}",
                String::from_utf8_lossy(request)
            );
        }

        // A `+` in the query is a space, as it is in every form-encoded value,
        // and RFC 9207's issuer is read like the other values.
        assert_eq!(
            callback_of(
                b"GET /0123456789abcdef/callback?code=a+b&state=c%2Bd&\
                  iss=https%3A%2F%2Fauth.example.com HTTP/1.1\r\n\r\n"
            ),
            CallbackRequest {
                path: Some("/0123456789abcdef/callback".to_owned()),
                callback: Callback::Code {
                    code: "a b".to_owned(),
                    state: "c+d".to_owned(),
                    iss: Some("https://auth.example.com".to_owned()),
                },
            }
        );
    }

    #[tokio::test]
    async fn a_sign_in_receiver_holds_the_page_until_the_sign_in_ends() {
        let receiver = SignInReceiver::bind().await.unwrap();
        let address = receiver.listener.local_addr().unwrap();
        assert_eq!(
            receiver.redirect_uri(),
            format!("http://127.0.0.1:{}/callback", address.port())
        );
        let serving = tokio::spawn(receiver.receive(tokio::time::Instant::now() + SIGN_IN_TIMEOUT));
        let mut browser = tokio::net::TcpStream::connect(address).await.unwrap();
        browser
            .write_all(
                b"GET /0123456789abcdef/callback?code=the-code&state=the-state HTTP/1.1\r\n\
                  Host: 127.0.0.1:4711\r\n\r\n",
            )
            .await
            .unwrap();
        let callback = serving.await.unwrap().unwrap();
        // The receiver accepts any path, and keeps the raw one it read: the
        // Space compares it with the issuer-keyed redirect it registered.
        assert_eq!(callback.path(), Some("/0123456789abcdef/callback"));
        assert_eq!(
            callback.callback(),
            &Callback::Code {
                code: "the-code".to_owned(),
                state: "the-state".to_owned(),
                iss: None,
            }
        );

        // The receiver used its one request: it is consumed, and the listener
        // was dropped, so no second connection is one this command serves.
        // The released port may belong to another process at any moment, so
        // the test does not connect to it.

        // The page is held until the sign-in ends. The browser reads nothing
        // before the outcome is known, so the page can never claim a success
        // the Space has not reported.
        let mut held = [0_u8; 1];
        assert!(
            tokio::time::timeout(Duration::from_millis(100), browser.peek(&mut held))
                .await
                .is_err(),
            "the page is written only after the sign-in ends"
        );

        // Once the outcome is known the browser gets the fixed page, with
        // neither the code nor the state.
        callback.answer(true).await.unwrap();
        let mut response = String::new();
        browser.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(
            response.contains("Content-Type: text/html; charset=utf-8"),
            "{response}"
        );
        assert!(response.contains("MCP sign-in finished"), "{response}");
        assert!(
            !response.contains("the-code") && !response.contains("the-state"),
            "the page echoes neither value: {response}"
        );
    }

    #[tokio::test]
    async fn a_refused_sign_in_receiver_answers_the_failed_page() {
        let receiver = SignInReceiver::bind().await.unwrap();
        let address = receiver.listener.local_addr().unwrap();
        let serving = tokio::spawn(receiver.receive(tokio::time::Instant::now() + SIGN_IN_TIMEOUT));
        let mut browser = tokio::net::TcpStream::connect(address).await.unwrap();
        browser
            .write_all(
                b"GET /callback?error=access_denied&error_description=the%20user%20said%20no \
                  HTTP/1.1\r\n\r\n",
            )
            .await
            .unwrap();
        let callback = serving.await.unwrap().unwrap();
        assert_eq!(callback.callback(), &Callback::Refused);
        callback.answer(false).await.unwrap();
        let mut response = String::new();
        browser.read_to_string(&mut response).await.unwrap();

        // The page says the sign-in failed and repeats no provider text.
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request"),
            "{response}"
        );
        assert!(response.contains("MCP sign-in failed"), "{response}");
        assert!(!response.contains("access_denied"), "{response}");
        assert!(!response.contains("user said no"), "{response}");
    }

    #[tokio::test]
    async fn a_sign_in_receiver_gives_up_when_the_browser_never_comes() {
        // The wait has one deadline: a browser that never returns cannot hold
        // the command past it.
        let started = std::time::Instant::now();
        let error = SignInReceiver::bind()
            .await
            .unwrap()
            .receive(tokio::time::Instant::now() + Duration::from_millis(100))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("did not finish within"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the receiver ended at its deadline: {:?}",
            started.elapsed()
        );
    }

    /// The credential the CLI uses for one fake Space.
    fn credential_at(address: std::net::SocketAddr) -> Credential {
        Credential {
            url: format!("http://{address}"),
            token: "token".to_owned(),
            account: SignedIn {
                id: Id::new(),
                display_name: "Ada".to_owned(),
                is_admin: false,
            },
        }
    }

    /// Serves a Space that opens the WebSocket and never greets the CLI.
    async fn ungreeting_space(listener: tokio::net::TcpListener) {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        // The client's hello is read, and never answered.
        let _ = socket.next().await;
        tokio::time::sleep(Duration::from_secs(60)).await;
    }

    /// Accepts the CLI's socket, greets it, and reads the one command it sends.
    async fn greeting_space(
        listener: tokio::net::TcpListener,
    ) -> (
        tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        Id,
        CommandCall,
    ) {
        let (stream, _) = listener.accept().await.unwrap();
        let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
        let hello = socket.next().await.unwrap().unwrap();
        assert!(matches!(
            serde_json::from_str::<ClientFrame>(hello.to_text().unwrap()).unwrap(),
            ClientFrame::Hello { .. }
        ));
        socket
            .send(Message::Text(
                serde_json::to_string(&ServerFrame::Hello {
                    version: PROTOCOL_VERSION,
                    capabilities: Vec::new(),
                })
                .unwrap()
                .into(),
            ))
            .await
            .unwrap();
        let command = socket.next().await.unwrap().unwrap();
        let ClientFrame::Command { id, command } =
            serde_json::from_str(command.to_text().unwrap()).unwrap()
        else {
            panic!("the Space expected one command");
        };
        (socket, id, command)
    }

    /// Serves a Space that greets the CLI, reads one command, and answers it
    /// with `result` only once the test releases the answer.
    async fn held_space(
        listener: tokio::net::TcpListener,
        reached: tokio::sync::oneshot::Sender<()>,
        release: tokio::sync::oneshot::Receiver<()>,
        result: serde_json::Value,
    ) {
        let (mut socket, id, _command) = greeting_space(listener).await;
        reached.send(()).unwrap();
        release.await.unwrap();
        socket
            .send(Message::Text(
                serde_json::to_string(&ServerFrame::Confirmed { id, result })
                    .unwrap()
                    .into(),
            ))
            .await
            .unwrap();
        let _ = socket.close(None).await;
    }

    /// Opens the receiver's browser side and writes the callback it waits for.
    ///
    /// `target` is the request target the browser asks for: the callback path
    /// the authorization server's redirect names, and its query.
    async fn callback_browser(
        receiver: SignInReceiver,
        target: &str,
    ) -> (super::SignInCallback, tokio::net::TcpStream) {
        let address = receiver.listener.local_addr().unwrap();
        let waiting = tokio::spawn(receiver.receive(tokio::time::Instant::now() + SIGN_IN_TIMEOUT));
        let mut browser = tokio::net::TcpStream::connect(address).await.unwrap();
        browser
            .write_all(format!("GET {target} HTTP/1.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        (waiting.await.unwrap().unwrap(), browser)
    }

    #[tokio::test]
    async fn a_silent_space_cannot_hold_the_sign_in_past_its_deadline() {
        // Each sign-in command opens its own WebSocket, and the client's
        // greeting wait has no bound of its own: the sign-in's deadline must
        // bound it, so a Space that accepts a socket and never greets cannot
        // hold the command.
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let space = tokio::spawn(ungreeting_space(listener));
        let started = std::time::Instant::now();
        let error = sign_in_command(
            &credential_at(address),
            tokio::time::Instant::now() + Duration::from_millis(100),
            CommandCall::McpSignInStatus { sign_in: Id::new() },
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("did not finish within"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the command ended at the sign-in deadline, not at the greeting: {:?}",
            started.elapsed()
        );
        space.abort();
    }

    #[tokio::test]
    async fn a_sign_in_page_reports_success_only_after_the_space_finishes() {
        let sign_in = Id::new();
        let server = Id::new();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let (reached, reached_rx) = tokio::sync::oneshot::channel();
        let (release, release_rx) = tokio::sync::oneshot::channel();
        let space = tokio::spawn(held_space(
            listener,
            reached,
            release_rx,
            json!({"state": "finished", "server": server, "connected": true}),
        ));
        let credential = credential_at(address);
        let (callback, mut browser) = callback_browser(
            SignInReceiver::bind().await.unwrap(),
            "/0123456789abcdef/callback?code=the-code&state=the-state",
        )
        .await;
        let finishing = tokio::spawn(async move {
            let outcome = finish_sign_in(
                &credential,
                tokio::time::Instant::now() + SIGN_IN_TIMEOUT,
                sign_in,
                &callback,
            )
            .await;
            answer_sign_in_page(callback, &outcome).await;
            outcome
        });

        // The Space holds the finish command unanswered, so the browser must
        // still hold no page: success is never claimed before the Space says
        // the sign-in finished.
        reached_rx.await.unwrap();
        let mut held = [0_u8; 1];
        assert!(
            tokio::time::timeout(Duration::from_millis(100), browser.peek(&mut held))
                .await
                .is_err(),
            "the page is written only after the Space answers"
        );

        release.send(()).unwrap();
        let outcome = finishing.await.unwrap().unwrap();
        assert_eq!(
            outcome,
            SignInState::Finished {
                server,
                connected: true,
            }
        );
        let mut response = String::new();
        browser.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.contains("MCP sign-in finished"), "{response}");
        assert!(
            !response.contains("the-code") && !response.contains("the-state"),
            "the page echoes neither value: {response}"
        );
        space.await.unwrap();
    }

    #[tokio::test]
    async fn a_sign_in_page_reports_a_failed_space_answer() {
        let sign_in = Id::new();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let (reached, reached_rx) = tokio::sync::oneshot::channel();
        let (release, release_rx) = tokio::sync::oneshot::channel();
        let space = tokio::spawn(held_space(
            listener,
            reached,
            release_rx,
            json!({"state": "failed", "reason": "the sign-in expired"}),
        ));
        let credential = credential_at(address);
        let (callback, mut browser) = callback_browser(
            SignInReceiver::bind().await.unwrap(),
            "/0123456789abcdef/callback?code=the-code&state=the-state",
        )
        .await;
        let finishing = tokio::spawn(async move {
            let outcome = finish_sign_in(
                &credential,
                tokio::time::Instant::now() + SIGN_IN_TIMEOUT,
                sign_in,
                &callback,
            )
            .await;
            answer_sign_in_page(callback, &outcome).await;
            outcome
        });
        reached_rx.await.unwrap();
        release.send(()).unwrap();
        let outcome = finishing.await.unwrap().unwrap();
        assert_eq!(
            outcome,
            SignInState::Failed {
                reason: "the sign-in expired".to_owned(),
            }
        );
        let mut response = String::new();
        browser.read_to_string(&mut response).await.unwrap();
        assert!(
            response.starts_with("HTTP/1.1 400 Bad Request"),
            "{response}"
        );
        assert!(response.contains("MCP sign-in failed"), "{response}");
        assert!(
            !response.contains("expired"),
            "the page repeats no Space text: {response}"
        );
        space.await.unwrap();
    }

    #[tokio::test]
    async fn a_finished_sign_in_sends_the_keyed_callback_path_to_the_space() {
        // The browser comes back to the issuer-keyed path the Space registered,
        // and the finish command carries that raw path: the Space is the side
        // that compares it with the redirect it planned.
        let sign_in = Id::new();
        let server = Id::new();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let space = tokio::spawn(async move {
            let (mut socket, id, command) = greeting_space(listener).await;
            socket
                .send(Message::Text(
                    serde_json::to_string(&ServerFrame::Confirmed {
                        id,
                        result: json!({"state": "finished", "server": server, "connected": true}),
                    })
                    .unwrap()
                    .into(),
                ))
                .await
                .unwrap();
            let _ = socket.close(None).await;
            command
        });
        let credential = credential_at(address);
        let (callback, _browser) = callback_browser(
            SignInReceiver::bind().await.unwrap(),
            "/0123456789abcdef/callback?code=the-code&state=the-state",
        )
        .await;
        let outcome = finish_sign_in(
            &credential,
            tokio::time::Instant::now() + SIGN_IN_TIMEOUT,
            sign_in,
            &callback,
        )
        .await
        .unwrap();
        assert_eq!(
            outcome,
            SignInState::Finished {
                server,
                connected: true,
            }
        );
        assert_eq!(
            space.await.unwrap(),
            CommandCall::McpSignInFinish {
                sign_in,
                code: "the-code".to_owned(),
                state: "the-state".to_owned(),
                iss: None,
                path: Some("/0123456789abcdef/callback".to_owned()),
            }
        );
    }

    #[test]
    fn a_sign_in_answer_reads_pending_finished_or_failed() {
        let server = Id::new();
        assert_eq!(
            sign_in_state(&json!({"state": "pending", "server": null, "connected": false}))
                .unwrap(),
            SignInState::Pending
        );
        assert_eq!(
            sign_in_state(&json!({
                "state": "finished",
                "server": server,
                "connected": true,
                "reason": null,
            }))
            .unwrap(),
            SignInState::Finished {
                server,
                connected: true,
            }
        );
        // A finished sign-in the Space could not reach still ends the wait.
        assert_eq!(
            sign_in_state(&json!({
                "state": "finished",
                "server": server,
                "connected": false,
            }))
            .unwrap(),
            SignInState::Finished {
                server,
                connected: false,
            }
        );
        assert_eq!(
            sign_in_state(&json!({
                "state": "failed",
                "server": null,
                "connected": false,
                "reason": "the sign-in expired",
            }))
            .unwrap(),
            SignInState::Failed {
                reason: "the sign-in expired".to_owned(),
            }
        );
        // A failure the Space explained with nothing still ends the wait.
        assert_eq!(
            sign_in_state(&json!({"state": "failed", "reason": null})).unwrap(),
            SignInState::Failed {
                reason: "the Space gave no reason".to_owned(),
            }
        );
        // An answer this CLI does not understand is an error, not a wait.
        for answer in [json!({"state": "expired"}), json!({})] {
            let error = sign_in_state(&answer).unwrap_err().to_string();
            assert!(error.contains("sign-in state"), "{answer}: {error}");
        }
        // A finished answer without its server names nothing to report.
        assert!(
            sign_in_state(&json!({"state": "finished", "connected": true})).is_err(),
            "a finished sign-in names its server"
        );
    }

    #[test]
    fn a_finished_sign_in_prints_the_connection_or_the_registration() {
        let server = Id::new();
        let user = Id::new();
        assert_eq!(mcp_scope_text(McpScope::Space, Some(user)), "space");
        assert_eq!(
            mcp_scope_text(McpScope::User, Some(user)),
            format!("user {user}")
        );
        assert_eq!(mcp_scope_text(McpScope::User, None), "user");

        // A reachable server reads as a connection.
        assert_eq!(
            sign_in_lines(
                SignInState::Finished {
                    server,
                    connected: true,
                },
                "weather",
                "https://mcp.example.com/mcp",
                "space",
            )
            .unwrap(),
            vec!["Connected MCP server weather.".to_owned()]
        );

        // Registered but unreachable prints `neha mcp add`'s two lines.
        assert_eq!(
            sign_in_lines(
                SignInState::Finished {
                    server,
                    connected: false,
                },
                "notes",
                "http://127.0.0.1:9000/mcp",
                &format!("user {user}"),
            )
            .unwrap(),
            vec![
                format!("Added MCP server {server} (notes) at http://127.0.0.1:9000/mcp."),
                format!("  scope: user {user}; not connected"),
            ]
        );

        // A failed sign-in is the command's error, escaped to one line so the
        // authorization server's text cannot rewrite the terminal.
        let error = sign_in_lines(
            SignInState::Failed {
                reason: "access_denied: no\n\u{1b}[31m".to_owned(),
            },
            "weather",
            "https://mcp.example.com/mcp",
            "space",
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            error,
            "the sign-in failed: access_denied: no\\n\\u{001b}[31m"
        );

        // A wait that is not done has no lines to print yet.
        assert!(
            sign_in_lines(
                SignInState::Pending,
                "weather",
                "https://mcp.example.com/mcp",
                "space",
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn a_sign_in_wait_polls_until_it_ends_or_times_out() {
        let sign_in = Id::new();
        let server = Id::new();

        // A pending answer asks again, and a finished one ends the wait.
        let mut answers = [
            json!({"state": "pending"}),
            json!({"state": "pending"}),
            json!({"state": "finished", "server": server, "connected": true}),
        ]
        .into_iter();
        let mut asks = 0;
        let ended = poll_sign_in(
            sign_in,
            tokio::time::Instant::now() + Duration::from_secs(5),
            Duration::ZERO,
            |asked| {
                asks += 1;
                assert_eq!(asked, sign_in);
                let answer = answers.next().expect("the wait asked past its answers");
                async move { Ok(answer) }
            },
        )
        .await
        .unwrap();
        assert_eq!(
            ended,
            SignInState::Finished {
                server,
                connected: true,
            }
        );
        assert_eq!(asks, 3);

        // A failed answer ends the wait too, so its reason is reported instead
        // of waiting for a sign-in that cannot finish.
        let failed = poll_sign_in(
            sign_in,
            tokio::time::Instant::now() + Duration::from_secs(5),
            Duration::ZERO,
            |_| async { Ok(json!({"state": "failed", "reason": "the sign-in expired"})) },
        )
        .await
        .unwrap();
        assert_eq!(
            failed,
            SignInState::Failed {
                reason: "the sign-in expired".to_owned(),
            }
        );

        // A sign-in that never ends is the timeout, and it outlives no retry.
        let error = poll_sign_in(
            sign_in,
            tokio::time::Instant::now(),
            Duration::ZERO,
            |_| async { Ok(json!({"state": "pending"})) },
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("did not finish within"), "{error}");
        assert!(error.contains("retry"), "{error}");

        // An ask whose answer never comes is bounded by the time left: the
        // WebSocket client may wait indefinitely, the poll may not.
        let started = std::time::Instant::now();
        let error = poll_sign_in(
            sign_in,
            tokio::time::Instant::now() + Duration::from_millis(100),
            Duration::ZERO,
            |_| async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok(json!({"state": "pending"}))
            },
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("did not finish within"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the wait ended at its deadline, not the ask's: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_server_rule_row_names_the_server_or_its_id() {
        let rule = Id::new();
        let bot = Id::new();
        let server = Id::new();
        let named = json!([{
            "rule": rule,
            "bot": bot,
            "effect": "ask",
            "tool": "get_weather",
            "server": server,
            "server_name": "weather",
        }]);
        assert_eq!(
            rule_rows(&named).unwrap(),
            vec![format!("{rule}  {bot}  ask  get_weather  server weather")]
        );
        // The Space may hold no name for the server; its id is the fallback.
        let unnamed = json!([{
            "rule": rule,
            "bot": bot,
            "effect": "allow",
            "tool": "get_weather",
            "server": server,
            "server_name": null,
        }]);
        assert_eq!(
            rule_rows(&unnamed).unwrap(),
            vec![format!(
                "{rule}  {bot}  allow  get_weather  server {server}"
            )]
        );
    }

    #[test]
    fn chat_read_marks_the_compacted_span_and_never_shows_a_summary() {
        let chat = Id::new();
        let file = Id::new();
        let messages = json!([
            {
                "id": Id::new(),
                "chat": chat,
                "seq": "1",
                "author": {"kind": "user"},
                "text": "the first note",
                "file": null,
                "compacted": false,
            },
            {
                "id": Id::new(),
                "chat": chat,
                "seq": "2",
                "author": {"kind": "bot"},
                "text": "the second note",
                "file": file,
                "compacted": true,
                // A summary is not a Message; a fixture that carries one must
                // not make the CLI print it.
                "summary": "an earlier summary standing in for the span",
            },
            {
                "id": Id::new(),
                "chat": chat,
                "seq": "3",
                "author": {"kind": "user"},
                "text": "a later note",
                "file": null,
                "compacted": false,
            },
        ]);

        let rows = chat_read_rows(&messages).unwrap();
        assert_eq!(
            rows,
            vec![
                "[1] User: the first note".to_owned(),
                "[2] Bot: the second note".to_owned(),
                format!("  file {file}"),
                "  (earlier Messages were compacted)".to_owned(),
                "[3] User: a later note".to_owned(),
            ]
        );
        let rendered = rows.join("\n");
        assert!(
            !rendered.contains("an earlier summary"),
            "the summary text is never printed:\n{rendered}"
        );
    }

    #[test]
    fn memory_rows_print_the_pinned_columns_and_metadata_only() {
        let memory = Id::new();
        let bot = Id::new();
        let user = Id::new();
        let source = Id::new();
        let list = json!({
            "entries": [
                {
                    "memory": memory,
                    "kind": "rule",
                    "scope": "owner",
                    "author": user,
                    "author_bot": false,
                    "source": source,
                    "source_kind": "message",
                    "size": 30,
                    "readable": true,
                    "text": "Keep the deploy notes current.",
                },
                {
                    "memory": Id::new(),
                    "kind": "note",
                    "scope": "user",
                    "author": bot,
                    "author_bot": true,
                    "source": Id::new(),
                    "source_kind": "run",
                    "size": 1536,
                    "readable": false,
                },
            ],
            "used": {"rule": 30, "note": 1536},
            "limits": {"rule": 4096, "note": 16384},
        });

        let rows = memory_rows(&list).unwrap();
        assert_eq!(rows.len(), 4, "{rows:?}");
        assert_eq!(
            rows[0],
            format!(
                "{memory}  rule  owner  author {user}  source message {source}  30 B  Keep the deploy notes current."
            )
        );
        // The owner sees an entry the owner does not read as metadata only.
        assert!(rows[1].contains("note  user  author "), "{rows:?}");
        assert!(rows[1].ends_with("1.5 KiB  (metadata only)"), "{rows:?}");
        assert_eq!(rows[2], "rule: 30 B used / 4.0 KiB limit");
        assert_eq!(rows[3], "note: 1.5 KiB used / 16 KiB limit");
    }

    #[test]
    fn a_memory_text_never_breaks_its_line() {
        let list = json!({
            "entries": [{
                "memory": Id::new(),
                "kind": "rule",
                "scope": "owner",
                "author": Id::new(),
                "author_bot": false,
                "source": Id::new(),
                "source_kind": "run",
                "size": 48,
                "readable": true,
                "text": "first line\nsecond\tone\rbell\u{7}\u{85} — 注意 📌",
            }],
            "used": {"rule": 48, "note": 0},
            "limits": {"rule": 4096, "note": 16384},
        });

        let rows = memory_rows(&list).unwrap();
        assert_eq!(rows.len(), 3, "{rows:?}");
        let row = &rows[0];
        assert_eq!(
            row.lines().count(),
            1,
            "one memory entry is one output line: {row:?}"
        );
        assert!(!row.contains('\n'), "a newline is escaped: {row:?}");
        assert!(!row.contains('\r'), "a carriage return is escaped: {row:?}");
        assert!(!row.contains('\t'), "a tab is escaped: {row:?}");
        assert!(
            row.ends_with(r"first line\nsecond\tone\rbell\u{7}\u{85} — 注意 📌"),
            "{row:?}"
        );
    }

    #[test]
    fn a_memory_text_escapes_controls_and_keeps_unicode() {
        assert_eq!(visible_text("plain text"), "plain text");
        assert_eq!(visible_text("one\ntwo"), r"one\ntwo");
        assert_eq!(visible_text("one\ttwo"), r"one\ttwo");
        assert_eq!(visible_text("one\rtwo"), r"one\rtwo");
        // Every other control character, including the C1 block, is named.
        assert_eq!(
            visible_text("\u{0}\u{7}\u{1b}\u{7f}\u{85}"),
            r"\u{0}\u{7}\u{1b}\u{7f}\u{85}"
        );
        // Unicode that is not a control character passes through unchanged.
        assert_eq!(
            visible_text("记住：部署笔记 — naïve 📌"),
            "记住：部署笔记 — naïve 📌"
        );
        // Only a control character changes; a literal backslash stays as typed.
        assert_eq!(visible_text(r"a\nb"), r"a\nb");
    }

    #[test]
    fn an_empty_memory_says_how_to_write_one() {
        let list = json!({
            "entries": [],
            "used": {"rule": 0, "note": 0},
            "limits": {"rule": 4096, "note": 16384},
        });

        assert_eq!(
            memory_rows(&list).unwrap(),
            vec![
                "No memory entries. Ask the Bot in a Chat to remember something.".to_owned(),
                "rule: 0 B used / 4.0 KiB limit".to_owned(),
                "note: 0 B used / 16 KiB limit".to_owned(),
            ]
        );
    }

    #[test]
    fn a_memory_size_is_human_readable() {
        assert_eq!(byte_size(0), "0 B");
        assert_eq!(byte_size(20), "20 B");
        assert_eq!(byte_size(1536), "1.5 KiB");
        assert_eq!(byte_size(4096), "4.0 KiB");
        assert_eq!(byte_size(16384), "16 KiB");
        assert_eq!(byte_size(32 * 1024 * 1024), "32 MiB");
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
            rule_add_report(
                &result,
                bot,
                "ask",
                "write_file",
                None,
                Some("./logs/"),
                None,
                None,
                None
            )
            .unwrap(),
            format!("Added rule {rule} for Bot {bot}: ask write_file (path-prefix \"logs\")")
        );

        // The same for shell commands: print what the Space kept.
        let result = json!({
            "rule": rule,
            "command_prefix": "git",
            "path_prefix": null,
        });
        assert_eq!(
            rule_add_report(
                &result,
                bot,
                "allow",
                "shell",
                Some("  git  "),
                None,
                None,
                None,
                None
            )
            .unwrap(),
            format!("Added rule {rule} for Bot {bot}: allow shell (command-prefix \"git\")")
        );

        // A server's tool rule names the server, so two servers that offer the
        // same tool are not reported as the same rule.
        let server = Id::new();
        let result = json!({
            "rule": rule,
            "server": server,
            "server_name": "weather",
        });
        assert_eq!(
            rule_add_report(
                &result,
                bot,
                "ask",
                "get_weather",
                None,
                None,
                None,
                None,
                Some("weather")
            )
            .unwrap(),
            format!("Added rule {rule} for Bot {bot}: ask get_weather (server \"weather\")")
        );

        // A Space answer without the fields falls back to what was typed.
        let result = json!({"rule": rule});
        assert_eq!(
            rule_add_report(
                &result,
                bot,
                "ask",
                "shell",
                Some("git"),
                None,
                None,
                None,
                None
            )
            .unwrap(),
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

/// What the integration tests need from the CLI module.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod test_support {
    use std::path::Path;

    /// Reads the folder a `grant` path argument names.
    pub fn checked_grant(path: &Path) -> anyhow::Result<String> {
        super::checked_grant(path)
    }

    /// Reads the folder a `revoke` path argument names.
    pub fn revocable_grant(path: &Path, shown: &[String]) -> anyhow::Result<String> {
        super::revocable_grant(path, shown)
    }
}
