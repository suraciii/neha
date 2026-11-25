//! `neha` runs a Space and talks to its Bots from a terminal.

mod client;

use anyhow::{Context as _, Result, bail};
use clap::{Parser, Subcommand};
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
    },
    /// Join a Space from its invitation link.
    Join {
        /// The invitation link printed by `neha serve`.
        link: String,
        /// Your display name in the Space.
        #[arg(long)]
        name: String,
        /// The password; prompted when omitted.
        #[arg(long)]
        password: Option<String>,
    },
    /// Sign in to a Space you already joined.
    Login {
        /// Your display name in the Space.
        #[arg(long)]
        name: String,
        /// The Space URL; defaults to the saved one.
        #[arg(long)]
        server: Option<String>,
        /// The password; prompted when omitted.
        #[arg(long)]
        password: Option<String>,
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
    /// List Chats and exchange Messages.
    Chat {
        #[command(subcommand)]
        command: ChatCommand,
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
        /// The Provider's API key.
        #[arg(long)]
        api_key: String,
        /// A Model name at the Provider; repeat for more.
        #[arg(long = "model", value_name = "NAME", required = true)]
        models: Vec<String>,
        /// The Model's context window in tokens.
        #[arg(long, default_value_t = 128_000)]
        context_limit: u32,
        /// The Model's largest reply in tokens.
        #[arg(long, default_value_t = 8_192)]
        max_output: u32,
        /// Whether the Model reads images.
        #[arg(long)]
        reads_images: bool,
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
        #[arg(long)]
        name: String,
    },
    /// List your Bots.
    List,
}

#[derive(Subcommand)]
enum ChatCommand {
    /// List your Chats.
    List,
    /// Send a Message and wait for the Bot's reply.
    Send {
        /// The Chat identifier.
        #[arg(long)]
        chat: Option<Id>,
        /// The Bot whose Chat to use instead of --chat.
        #[arg(long)]
        bot: Option<Id>,
        /// The Message text.
        text: String,
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
        Top::Serve { port } => serve(port).await,
        Top::Join {
            link,
            name,
            password,
        } => join(&link, &name, password).await,
        Top::Login {
            name,
            server,
            password,
        } => login(&name, server, &read_password(password)?).await,
        Top::Whoami => whoami(),
        Top::Provider { command } => provider(command).await,
        Top::Model { command } => model(command).await,
        Top::Bot { command } => bot(command).await,
        Top::Chat { command } => chat(command).await,
        Top::Run { command } => run(command).await,
    }
}

async fn serve(port: u16) -> Result<()> {
    let home = Home::from_env()
        .context("NEHA_HOME is not set; set it to the directory that holds this Space")?;
    let running = neha_space::start(home, port)
        .await
        .context("the Space cannot start")?;

    println!("Neha is listening on {}", running.url());
    if let Some(invite) = &running.invite {
        println!("Invite: {invite}");
        println!("Join with: neha join '{invite}' --name <your name>");
    } else {
        println!("Sign in with: neha login --name <your name>");
    }
    println!("Press Ctrl-C to stop.");

    tokio::signal::ctrl_c()
        .await
        .context("cannot listen for Ctrl-C")?;
    running.shutdown().await;

    Ok(())
}

async fn join(link: &str, name: &str, password: Option<String>) -> Result<()> {
    client::split_invite(link)?;
    let password = read_password(password)?;
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

async fn login(name: &str, server: Option<String>, password: &str) -> Result<()> {
    let server = match server {
        Some(server) => server,
        None => {
            Credential::load()
                .context("pass --server when this machine has no saved Space")?
                .url
        }
    };
    let credential = client::login(&server, name, password).await?;
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
            api_key,
            models,
            context_limit,
            max_output,
            reads_images,
        } => {
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
                            reads_images,
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
                println!(
                    "{}  {}  context {}  max output {}{default}",
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
        BotCommand::Create { name } => {
            let result = client::command(
                &credential,
                CommandCall::BotCreate(BotCreate { name: name.clone() }),
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

async fn chat(command: ChatCommand) -> Result<()> {
    let credential = Credential::load()?;
    match command {
        ChatCommand::List => {
            let result = client::command(&credential, CommandCall::ChatList).await?;
            for chat in result.as_array().context("no Chats in the answer")? {
                println!("{}  Bot {}", id_of(chat, "id")?, id_of(chat, "bot")?);
            }
        }
        ChatCommand::Send { chat, bot, text } => {
            let chat = match (chat, bot) {
                (Some(chat), _) => chat,
                (None, Some(bot)) => {
                    let shown = client::command(&credential, CommandCall::BotShow { bot }).await?;
                    id_of(&shown["chat"], "id")?
                }
                (None, None) => bail!("pass --chat or --bot"),
            };
            let result = client::command(
                &credential,
                CommandCall::MessageSend(MessageSend { chat, text }),
            )
            .await?;
            println!("{}", text_of(&result, "text")?);
            println!(
                "  run {} ({})",
                id_of(&result, "run")?,
                text_of(&result, "status")?
            );
        }
        ChatCommand::Read { chat, from } => {
            let result = client::command(&credential, CommandCall::ChatRead { chat, from }).await?;
            for message in result.as_array().context("no Messages in the answer")? {
                let author = match message["author"]["kind"].as_str().unwrap_or_default() {
                    "bot" => "Bot",
                    _ => "User",
                };
                println!(
                    "[{}] {author}: {}",
                    message["seq"].as_str().unwrap_or_default(),
                    text_of(message, "text")?
                );
            }
        }
    }

    Ok(())
}

async fn run(command: RunCommand) -> Result<()> {
    let credential = Credential::load()?;
    match command {
        RunCommand::Show { run } => {
            let result = client::command(&credential, CommandCall::RunShow { run }).await?;
            println!(
                "Run {}: {}",
                id_of(&result, "id")?,
                text_of(&result, "status")?
            );
            if let Some(reply) = result["reply"].as_str() {
                println!("  reply {}", reply);
            }
            if let Some(error) = result["error"].as_str() {
                println!("  error {error}");
            }
            if !result["usage"].is_null() {
                println!("  usage {}", result["usage"]);
            }
        }
    }

    Ok(())
}

fn read_password(given: Option<String>) -> Result<String> {
    match given {
        Some(password) => Ok(password),
        None => rpassword::prompt_password("Password: ").context("cannot read the password"),
    }
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
