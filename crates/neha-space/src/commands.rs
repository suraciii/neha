//! Command handlers.
//!
//! A handler validates the caller, writes the command's events, and returns
//! what the client should see. A Message that starts a run also hands back
//! the work the server must finish before it confirms the command.

use std::sync::Mutex;

use neha::id::Id;
use neha::record::{Author, EventDraft, EventKind};
use neha::seq::Seq;
use neha::surface::{CommandCall, MessageSend, ProviderAdd};
use serde_json::{Value, json};

use crate::home::Home;
use crate::model::{ChatMessage, ChatRole, ModelClient};
use crate::secrets::Secrets;
use crate::server::ApiError;
use crate::state::{BotView, ChatView, RunStatus};
use crate::store::{AppendRequest, AppendResult, Projection, Store};

/// What a command wrote and whether work follows.
pub struct Outcome {
    /// The cursor of the command's last event, when it wrote events.
    pub committed: Option<Seq>,
    /// The result to confirm now, when no work follows.
    pub result: Value,
    /// A run the server must finish before confirming.
    pub pending: Option<PendingRun>,
}

/// A one-shot run waiting for its Model reply.
pub struct PendingRun {
    /// The run.
    pub run: Id,
    /// The Chat.
    pub chat: ChatView,
    /// The Bot.
    pub bot: BotView,
    /// The User the Bot acts for.
    pub acting: Id,
    /// The Message that started the run.
    pub message: Id,
    /// The command to confirm when the run finishes.
    pub command: Id,
}

/// Runs one command for an account.
pub async fn execute(
    store: &Store,
    state: &Mutex<crate::state::State>,
    home: &Home,
    secrets: &Secrets,
    account: &crate::identity::Account,
    command_id: Id,
    command: CommandCall,
) -> Result<Outcome, ApiError> {
    let fingerprint =
        serde_json::to_string(&command).map_err(|_| ApiError::internal("bad command"))?;

    match command {
        CommandCall::ProviderAdd(arguments) => {
            if !account.is_admin {
                return Err(ApiError::forbidden("only the Admin connects a Provider"));
            }
            provider_add(store, state, secrets, account, command_id, arguments).await
        }
        CommandCall::ProviderList => read(state, |state| {
            serde_json::to_value(state.providers.values().collect::<Vec<_>>()).map_err(json_error)
        }),
        CommandCall::ModelList => read(state, |state| {
            serde_json::to_value(state.models.values().collect::<Vec<_>>()).map_err(json_error)
        }),
        CommandCall::BotCreate(arguments) => {
            bot_create(store, state, home, account, command_id, arguments.name).await
        }
        CommandCall::BotList => read(state, |state| {
            let bots: Vec<&BotView> = state
                .bots
                .values()
                .filter(|bot| bot.owner == account.id)
                .collect();
            serde_json::to_value(bots).map_err(json_error)
        }),
        CommandCall::BotShow { bot } => read(state, |state| {
            let bot = state
                .owned_bot(bot, account.id)
                .ok_or_else(|| ApiError::not_found("no such Bot"))?;
            let computer = state
                .computers
                .get(&bot.computer)
                .ok_or_else(|| ApiError::internal("the Bot has no Computer"))?;
            let chat = state
                .chats
                .get(&bot.chat)
                .ok_or_else(|| ApiError::internal("the Bot has no Chat"))?;
            Ok(json!({"bot": bot, "computer": computer, "chat": chat}))
        }),
        CommandCall::ChatList => read(state, |state| {
            let chats: Vec<&ChatView> = state
                .chats
                .values()
                .filter(|chat| chat.owner == account.id)
                .collect();
            serde_json::to_value(chats).map_err(json_error)
        }),
        CommandCall::ChatRead { chat, from } => read(state, |state| {
            state
                .owned_chat(chat, account.id)
                .ok_or_else(|| ApiError::not_found("no such Chat"))?;
            serde_json::to_value(state.messages_of(chat, from)).map_err(json_error)
        }),
        CommandCall::MessageSend(arguments) => {
            message_send(store, state, account, command_id, fingerprint, arguments).await
        }
        CommandCall::RunShow { run } => read(state, |state| {
            let run = state
                .runs
                .get(&run)
                .filter(|run| run.acting == account.id)
                .ok_or_else(|| ApiError::not_found("no such run"))?;
            serde_json::to_value(run).map_err(json_error)
        }),
    }
}

async fn provider_add(
    store: &Store,
    state: &Mutex<crate::state::State>,
    secrets: &Secrets,
    account: &crate::identity::Account,
    command_id: Id,
    arguments: ProviderAdd,
) -> Result<Outcome, ApiError> {
    if arguments.models.is_empty() {
        return Err(ApiError::invalid_argument(
            "a Provider needs at least one Model",
        ));
    }
    let provider = Id::new();
    let models: Vec<Value> = arguments
        .models
        .iter()
        .map(|model| {
            json!({
                "id": Id::new(),
                "name": model.name,
                "context_limit": model.context_limit,
                "max_output": model.max_output,
                "reads_images": model.reads_images,
            })
        })
        .collect();
    let default_model = arguments
        .default_model
        .as_ref()
        .and_then(|name| {
            arguments
                .models
                .iter()
                .zip(&models)
                .find(|(model, _)| &model.name == name)
                .map(|(_, data)| data["id"].clone())
        })
        .or_else(|| models.first().map(|model| model["id"].clone()))
        .ok_or_else(|| ApiError::invalid_argument("the default Model is not declared"))?;
    let data = json!({
        "provider": provider,
        "protocol": "chat_completions",
        "base_url": arguments.base_url,
        "models": models,
        "default_model": default_model,
    });
    let result = json!({"provider": provider, "models": models});
    let draft = EventDraft::new(EventKind::ProviderAdded, Author::User(account.id), data);
    let appended = append(
        store,
        state,
        command_id,
        serde_json::to_string(&arguments).map_err(json_error)?,
        vec![draft],
        Some(result),
    )
    .await?;
    secrets
        .store_provider_key(provider, &arguments.api_key)
        .map_err(|error| ApiError::internal(error.to_string()))?;
    if let Some(result) = appended.result.clone() {
        return Ok(Outcome {
            committed: appended.events.last().map(|event| event.seq),
            result,
            pending: None,
        });
    }
    let result = json!({"provider": provider, "models": models});

    Ok(Outcome {
        committed: appended.events.last().map(|event| event.seq),
        result,
        pending: None,
    })
}

async fn bot_create(
    store: &Store,
    state: &Mutex<crate::state::State>,
    home: &Home,
    account: &crate::identity::Account,
    command_id: Id,
    name: String,
) -> Result<Outcome, ApiError> {
    let name = name.trim().to_owned();
    if name.is_empty() {
        return Err(ApiError::invalid_argument("a Bot needs a name"));
    }
    let bot = Id::new();
    let computer = Id::new();
    let chat = Id::new();
    std::fs::create_dir_all(home.workspace_dir(computer))
        .map_err(|error| ApiError::internal(error.to_string()))?;
    let drafts = vec![
        EventDraft::new(
            EventKind::ComputerCreated,
            Author::User(account.id),
            json!({
                "computer": computer,
                "owner": account.id,
                "kind": "managed",
                "workspace": home.workspace_dir(computer).to_string_lossy(),
            }),
        ),
        EventDraft::new(
            EventKind::BotCreated,
            Author::User(account.id),
            json!({"bot": bot, "owner": account.id, "name": name, "computer": computer, "chat": chat}),
        ),
        EventDraft::new(
            EventKind::ChatCreated,
            Author::User(account.id),
            json!({"chat": chat, "owner": account.id, "bot": bot}),
        ),
    ];
    let result = json!({
        "bot": {"id": bot, "owner": account.id, "name": name, "computer": computer, "chat": chat},
        "computer": {"id": computer, "owner": account.id, "kind": "managed", "online": true},
        "chat": {"id": chat, "owner": account.id, "bot": bot},
    });
    let appended = append(
        store,
        state,
        command_id,
        format!("bot.create:{name}"),
        drafts,
        Some(result),
    )
    .await?;
    if let Some(result) = appended.result.clone() {
        return Ok(Outcome {
            committed: appended.events.last().map(|event| event.seq),
            result,
            pending: None,
        });
    }
    let result = json!({
        "bot": {"id": bot, "owner": account.id, "name": name, "computer": computer, "chat": chat},
        "computer": {"id": computer, "owner": account.id, "kind": "managed", "online": true},
        "chat": {"id": chat, "owner": account.id, "bot": bot},
    });

    Ok(Outcome {
        committed: appended.events.last().map(|event| event.seq),
        result,
        pending: None,
    })
}

async fn message_send(
    store: &Store,
    state: &Mutex<crate::state::State>,
    account: &crate::identity::Account,
    command_id: Id,
    fingerprint: String,
    arguments: MessageSend,
) -> Result<Outcome, ApiError> {
    let (bot, text) = {
        let state = state.lock().map_err(|_| ApiError::internal("state lock"))?;
        let chat = state
            .owned_chat(arguments.chat, account.id)
            .ok_or_else(|| ApiError::not_found("no such Chat"))?;
        let bot = state
            .bots
            .get(&chat.bot)
            .cloned()
            .ok_or_else(|| ApiError::internal("the Chat has no Bot"))?;
        (bot, arguments.text)
    };
    if text.trim().is_empty() {
        return Err(ApiError::invalid_argument("a Message needs text"));
    }
    let message = Id::new();
    let run = Id::new();
    let drafts = vec![
        EventDraft::new(
            EventKind::MessageAdded,
            Author::User(account.id),
            json!({"message": message, "chat": arguments.chat, "text": text, "run": run}),
        ),
        EventDraft::new(
            EventKind::RunStarted,
            Author::Bot(bot.id),
            json!({"run": run, "chat": arguments.chat, "bot": bot.id, "message": message}),
        )
        .acting(account.id),
    ];
    let appended = append(store, state, command_id, fingerprint, drafts, None).await?;
    if appended.existing {
        let result = appended
            .result
            .unwrap_or_else(|| json!({"message": message, "run": run, "status": "running"}));

        return Ok(Outcome {
            committed: appended.events.last().map(|event| event.seq),
            result,
            pending: None,
        });
    }

    Ok(Outcome {
        committed: appended.events.last().map(|event| event.seq),
        result: json!({"message": message, "run": run, "status": "running"}),
        pending: Some(PendingRun {
            run,
            chat: state
                .lock()
                .map_err(|_| ApiError::internal("state lock"))?
                .chats
                .get(&arguments.chat)
                .cloned()
                .ok_or_else(|| ApiError::internal("the Chat vanished"))?,
            bot,
            acting: account.id,
            message,
            command: command_id,
        }),
    })
}

/// Runs the Model once and records the reply or the failure.
pub async fn finish_run(
    store: &Store,
    state: &Mutex<crate::state::State>,
    secrets: &Secrets,
    model: &ModelClient,
    pending: PendingRun,
) -> Result<Value, ApiError> {
    let (model_view, provider) = {
        let state = state.lock().map_err(|_| ApiError::internal("state lock"))?;
        let model = state
            .default_model()
            .cloned()
            .ok_or_else(|| ApiError::invalid_argument("the Space has no default Model"))?;
        let provider = state
            .providers
            .get(&model.provider)
            .cloned()
            .ok_or_else(|| ApiError::invalid_argument("the Model has no Provider"))?;
        (model, provider)
    };
    let api_key = secrets
        .provider_key(provider.id)
        .map_err(|_| ApiError::invalid_argument("the Provider key is missing"))?;
    let messages = build_messages(state, &pending)?;
    let outcome = model
        .chat(crate::model::ChatRequest {
            base_url: &provider.base_url,
            api_key: &api_key,
            model: &model_view.name,
            messages,
            max_output: model_view.max_output,
        })
        .await;

    let (drafts, result) = match outcome {
        Ok(reply) => {
            let reply_id = Id::new();
            let usage = reply.usage;
            let drafts = vec![
                EventDraft::new(
                    EventKind::MessageAdded,
                    Author::Bot(pending.bot.id),
                    json!({"message": reply_id, "chat": pending.chat.id, "text": reply.text, "run": pending.run}),
                )
                .acting(pending.acting),
                EventDraft::new(
                    EventKind::RunFinished,
                    Author::Bot(pending.bot.id),
                    json!({
                        "run": pending.run,
                        "status": RunStatus::Succeeded,
                        "reply": reply_id,
                        "usage": usage,
                    }),
                )
                .acting(pending.acting),
            ];
            let result = json!({
                "run": pending.run,
                "message": pending.message,
                "status": "succeeded",
                "reply": reply_id,
                "text": reply.text,
                "usage": usage,
            });
            (drafts, result)
        }
        Err(error) => {
            let reply_id = Id::new();
            let text = format!("I could not answer: {error}");
            let drafts = vec![
                EventDraft::new(
                    EventKind::MessageAdded,
                    Author::Bot(pending.bot.id),
                    json!({"message": reply_id, "chat": pending.chat.id, "text": text, "run": pending.run}),
                )
                .acting(pending.acting),
                EventDraft::new(
                    EventKind::RunFinished,
                    Author::Bot(pending.bot.id),
                    json!({
                        "run": pending.run,
                        "status": RunStatus::Failed,
                        "reply": reply_id,
                        "error": error.to_string(),
                    }),
                )
                .acting(pending.acting),
            ];
            let result = json!({
                "run": pending.run,
                "message": pending.message,
                "status": "failed",
                "reply": reply_id,
                "text": text,
                "error": error.to_string(),
            });
            (drafts, result)
        }
    };

    let completion = append(
        store,
        state,
        Id::new(),
        format!("run.finish:{}", pending.run),
        drafts,
        None,
    )
    .await?;
    store
        .set_result(pending.command, result.clone())
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?;

    Ok(completion.result.unwrap_or(result))
}

fn build_messages(
    state: &Mutex<crate::state::State>,
    pending: &PendingRun,
) -> Result<Vec<ChatMessage>, ApiError> {
    let state = state.lock().map_err(|_| ApiError::internal("state lock"))?;
    let mut messages = vec![ChatMessage::new(
        ChatRole::System,
        format!(
            "You are {}, a Bot working in Neha. Answer the User's Message in the Chat.",
            pending.bot.name
        ),
    )];
    for (author, text) in state.history(pending.chat.id) {
        let role = match author {
            Author::User(_) => ChatRole::User,
            Author::Bot(_) => ChatRole::Assistant,
        };
        messages.push(ChatMessage::new(role, text));
    }

    Ok(messages)
}

async fn append(
    store: &Store,
    state: &Mutex<crate::state::State>,
    command: Id,
    fingerprint: String,
    drafts: Vec<EventDraft>,
    result: Option<Value>,
) -> Result<AppendResult, ApiError> {
    let mut request = AppendRequest::new(command, fingerprint, drafts);
    if let Some(result) = result {
        request = request.with_result(result);
    }
    let appended = store
        .append(request)
        .await
        .map_err(|error| ApiError::internal(error.to_string()))?;
    apply(state, &appended.events)?;

    Ok(appended)
}

fn apply(
    state: &Mutex<crate::state::State>,
    events: &[neha::record::Event],
) -> Result<(), ApiError> {
    let mut state = state.lock().map_err(|_| ApiError::internal("state lock"))?;
    for event in events {
        state.apply(event).map_err(ApiError::internal)?;
    }

    Ok(())
}

fn read(
    state: &Mutex<crate::state::State>,
    read: impl FnOnce(&crate::state::State) -> Result<Value, ApiError>,
) -> Result<Outcome, ApiError> {
    let state = state.lock().map_err(|_| ApiError::internal("state lock"))?;
    let result = read(&state)?;

    Ok(Outcome {
        committed: None,
        result,
        pending: None,
    })
}

fn json_error(error: serde_json::Error) -> ApiError {
    ApiError::internal(error.to_string())
}
