//! Command handlers.
//!
//! A handler validates the caller, writes the command's events, and returns
//! what the client should see. A Message that starts a run also hands back
//! the work the server must finish before it confirms the command.
//!
//! A command that is already recorded never writes or creates anything
//! again: its recorded result is the answer.

use std::sync::Mutex;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use neha::id::Id;
use neha::record::{Author, Event, EventDraft, EventKind};
use neha::seq::Seq;
use neha::surface::{CommandCall, MessageSend, ProviderAdd};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::home::Home;
use crate::model::{ChatMessage, ChatRole, ModelClient};
use crate::secrets::Secrets;
use crate::server::ApiError;
use crate::state::{BotView, ChatView, ModelView, ProviderView, RunStatus, State};
use crate::store::{AppendRequest, AppendResult, CommandRecord, Projection, Store};

const RESULT_RUNNING: &str = "running";
const RUN_POLL: Duration = Duration::from_millis(100);

/// What a command wrote and whether work follows.
pub struct Outcome {
    /// The cursor of the command's last event.
    pub committed: Option<Seq>,
    /// The result to confirm now, when no work follows.
    pub result: Value,
    /// A run the server must finish before confirming.
    pub pending: Option<PendingRun>,
    /// The events this command just wrote.
    pub events: Vec<Event>,
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
    /// The Model to call.
    pub model: ModelView,
    /// The Provider that offers the Model.
    pub provider: ProviderView,
    /// The Provider's API key.
    pub api_key: String,
}

/// Runs one command for an account.
pub async fn execute(
    store: &Store,
    state: &Mutex<State>,
    home: &Home,
    secrets: &Secrets,
    account: &crate::identity::Account,
    command_id: Id,
    command: CommandCall,
) -> Result<Outcome, ApiError> {
    let fingerprint = fingerprint(&command)?;

    match command {
        CommandCall::ProviderAdd(arguments) => {
            if !account.is_admin {
                return Err(ApiError::forbidden("only the Admin connects a Provider"));
            }
            provider_add(
                store,
                state,
                secrets,
                account,
                command_id,
                fingerprint,
                arguments,
            )
            .await
        }
        CommandCall::ProviderList => read(state, |state| {
            serde_json::to_value(state.providers.values().collect::<Vec<_>>()).map_err(json_error)
        }),
        CommandCall::ModelList => read(state, |state| {
            serde_json::to_value(state.models.values().collect::<Vec<_>>()).map_err(json_error)
        }),
        CommandCall::BotCreate(arguments) => {
            bot_create(
                store,
                state,
                home,
                account,
                command_id,
                fingerprint,
                arguments.name,
            )
            .await
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
            message_send(
                store,
                state,
                secrets,
                account,
                command_id,
                fingerprint,
                arguments,
            )
            .await
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
    state: &Mutex<State>,
    secrets: &Secrets,
    account: &crate::identity::Account,
    command_id: Id,
    fingerprint: String,
    arguments: ProviderAdd,
) -> Result<Outcome, ApiError> {
    if arguments.models.is_empty() {
        return Err(ApiError::invalid_argument(
            "a Provider needs at least one Model",
        ));
    }
    if let Some(record) = existing(store, command_id, &fingerprint).await? {
        return existing_outcome(store, command_id, record).await;
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
    secrets
        .store_provider_key(provider, &arguments.api_key)
        .map_err(|_| ApiError::internal("cannot store the Provider key"))?;
    let draft = EventDraft::new(EventKind::ProviderAdded, Author::User(account.id), data);
    let appended = match append(
        store,
        state,
        command_id,
        fingerprint,
        vec![draft],
        Some(result.clone()),
        None,
    )
    .await
    {
        Ok(appended) => appended,
        Err(error) => {
            secrets.discard_provider_key(provider);
            return Err(error);
        }
    };
    if appended.existing {
        secrets.discard_provider_key(provider);
        return recorded_outcome(store, command_id).await;
    }

    Ok(Outcome {
        committed: appended.events.last().map(|event| event.seq),
        result,
        pending: None,
        events: appended.events,
    })
}

async fn bot_create(
    store: &Store,
    state: &Mutex<State>,
    home: &Home,
    account: &crate::identity::Account,
    command_id: Id,
    fingerprint: String,
    name: String,
) -> Result<Outcome, ApiError> {
    let name = name.trim().to_owned();
    if name.is_empty() {
        return Err(ApiError::invalid_argument("a Bot needs a name"));
    }
    if let Some(record) = existing(store, command_id, &fingerprint).await? {
        return existing_outcome(store, command_id, record).await;
    }

    let bot = Id::new();
    let computer = Id::new();
    let chat = Id::new();
    let workspace = home.workspace_dir(computer);
    std::fs::create_dir_all(&workspace)
        .map_err(|_| ApiError::internal("cannot create the Computer workspace"))?;
    let drafts = vec![
        EventDraft::new(
            EventKind::ComputerCreated,
            Author::User(account.id),
            json!({
                "computer": computer,
                "owner": account.id,
                "kind": "managed",
                "workspace": workspace.to_string_lossy(),
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
        "computer": {"id": computer, "owner": account.id, "kind": "managed", "online": true, "workspace": workspace.to_string_lossy()},
        "chat": {"id": chat, "owner": account.id, "bot": bot},
    });
    let appended = match append(
        store,
        state,
        command_id,
        fingerprint,
        drafts,
        Some(result.clone()),
        None,
    )
    .await
    {
        Ok(appended) => appended,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&workspace);
            return Err(error);
        }
    };
    if appended.existing {
        let _ = std::fs::remove_dir_all(&workspace);
        return recorded_outcome(store, command_id).await;
    }

    Ok(Outcome {
        committed: appended.events.last().map(|event| event.seq),
        result,
        pending: None,
        events: appended.events,
    })
}

async fn message_send(
    store: &Store,
    state: &Mutex<State>,
    secrets: &Secrets,
    account: &crate::identity::Account,
    command_id: Id,
    fingerprint: String,
    arguments: MessageSend,
) -> Result<Outcome, ApiError> {
    if let Some(record) = existing(store, command_id, &fingerprint).await? {
        return existing_outcome(store, command_id, record).await;
    }

    let (bot, model, provider, api_key) = preflight(state, arguments.chat, account.id, secrets)?;
    if arguments.text.trim().is_empty() {
        return Err(ApiError::invalid_argument("a Message needs text"));
    }

    let message = Id::new();
    let run = Id::new();
    let drafts = vec![
        EventDraft::new(
            EventKind::MessageAdded,
            Author::User(account.id),
            json!({"message": message, "chat": arguments.chat, "text": arguments.text, "run": run}),
        ),
        EventDraft::new(
            EventKind::RunStarted,
            Author::Bot(bot.id),
            json!({"run": run, "chat": arguments.chat, "bot": bot.id, "message": message, "command": command_id}),
        )
        .acting(account.id),
    ];
    let provisional = json!({"message": message, "run": run, "status": RESULT_RUNNING});
    let appended = append(
        store,
        state,
        command_id,
        fingerprint,
        drafts,
        Some(provisional.clone()),
        None,
    )
    .await?;
    if appended.existing {
        return recorded_outcome(store, command_id).await;
    }

    Ok(Outcome {
        committed: appended.events.last().map(|event| event.seq),
        result: provisional,
        pending: Some(PendingRun {
            run,
            chat: ChatView {
                id: arguments.chat,
                owner: account.id,
                bot: bot.id,
            },
            bot,
            acting: account.id,
            message,
            command: command_id,
            model,
            provider,
            api_key,
        }),
        events: appended.events,
    })
}

/// Runs the Model once and records the reply or the failure.
///
/// The completion events and the starting command's result are written in
/// one transaction, so a retry never sees a finished run without a result.
pub async fn finish_run(
    store: &Store,
    state: &Mutex<State>,
    model: &ModelClient,
    pending: PendingRun,
) -> Result<(Value, Vec<Event>), ApiError> {
    let messages = build_messages(state, &pending)?;
    let outcome = model
        .chat(crate::model::ChatRequest {
            base_url: &pending.provider.base_url,
            api_key: &pending.api_key,
            model: &pending.model.name,
            messages,
            max_output: pending.model.max_output,
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
        Some((pending.command, result.clone())),
    )
    .await?;

    Ok((result, completion.events))
}

fn preflight(
    state: &Mutex<State>,
    chat: Id,
    owner: Id,
    secrets: &Secrets,
) -> Result<(BotView, ModelView, ProviderView, String), ApiError> {
    let state = state.lock().map_err(|_| ApiError::internal("state lock"))?;
    state
        .owned_chat(chat, owner)
        .ok_or_else(|| ApiError::not_found("no such Chat"))?;
    let chat_view = state.chats.get(&chat).expect("checked above");
    let bot = state
        .bots
        .get(&chat_view.bot)
        .cloned()
        .ok_or_else(|| ApiError::internal("the Chat has no Bot"))?;
    let model = state
        .default_model()
        .cloned()
        .ok_or_else(|| ApiError::invalid_argument("the Space has no default Model"))?;
    let provider = state
        .providers
        .get(&model.provider)
        .cloned()
        .ok_or_else(|| ApiError::invalid_argument("the Model has no Provider"))?;
    let api_key = secrets
        .provider_key(provider.id)
        .map_err(|_| ApiError::invalid_argument("the Provider key is missing"))?;

    Ok((bot, model, provider, api_key))
}

fn build_messages(
    state: &Mutex<State>,
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
    state: &Mutex<State>,
    command: Id,
    fingerprint: String,
    drafts: Vec<EventDraft>,
    result: Option<Value>,
    confirmation: Option<(Id, Value)>,
) -> Result<AppendResult, ApiError> {
    let mut request = AppendRequest::new(command, fingerprint, drafts);
    if let Some(result) = result {
        request = request.with_result(result);
    }
    if let Some((confirmed, result)) = confirmation {
        request = request.with_confirmation(confirmed, result);
    }
    let appended = store.append(request).await.map_err(store_error)?;
    if !appended.existing {
        apply(state, &appended.events)?;
    }

    Ok(appended)
}

fn apply(state: &Mutex<State>, events: &[Event]) -> Result<(), ApiError> {
    let mut state = state.lock().map_err(|_| ApiError::internal("state lock"))?;
    for event in events {
        state.apply(event).map_err(ApiError::internal)?;
    }

    Ok(())
}

fn read(
    state: &Mutex<State>,
    read: impl FnOnce(&State) -> Result<Value, ApiError>,
) -> Result<Outcome, ApiError> {
    let state = state.lock().map_err(|_| ApiError::internal("state lock"))?;
    let result = read(&state)?;

    Ok(Outcome {
        committed: None,
        result,
        pending: None,
        events: Vec::new(),
    })
}

async fn existing(
    store: &Store,
    command_id: Id,
    fingerprint: &str,
) -> Result<Option<CommandRecord>, ApiError> {
    let Some(record) = store.command(command_id).await.map_err(store_error)? else {
        return Ok(None);
    };
    if record.fingerprint != fingerprint {
        return Err(ApiError::idempotency_conflict(format!(
            "command {command_id} was reused with different parameters"
        )));
    }

    Ok(Some(record))
}

async fn existing_outcome(
    store: &Store,
    command_id: Id,
    record: CommandRecord,
) -> Result<Outcome, ApiError> {
    let result = match record.result {
        Some(result) if !is_running(&result) => result,
        Some(_) => await_completion(store, command_id).await?,
        None => await_completion(store, command_id).await?,
    };

    Ok(Outcome {
        committed: Some(record.last_seq),
        result,
        pending: None,
        events: Vec::new(),
    })
}

async fn recorded_outcome(store: &Store, command_id: Id) -> Result<Outcome, ApiError> {
    let record = store
        .command(command_id)
        .await
        .map_err(store_error)?
        .ok_or_else(|| ApiError::internal("the recorded command vanished"))?;
    existing_outcome(store, command_id, record).await
}

async fn await_completion(store: &Store, command_id: Id) -> Result<Value, ApiError> {
    for _ in 0..1500 {
        tokio::time::sleep(RUN_POLL).await;
        let Some(record) = store.command(command_id).await.map_err(store_error)? else {
            return Err(ApiError::conflict("the command is not recorded"));
        };
        if let Some(result) = record.result
            && !is_running(&result)
        {
            return Ok(result);
        }
    }

    Err(ApiError::conflict("the command is still running"))
}

fn is_running(result: &Value) -> bool {
    result["status"].as_str() == Some(RESULT_RUNNING)
}

fn fingerprint(command: &CommandCall) -> Result<String, ApiError> {
    let bytes = serde_json::to_vec(command).map_err(|_| ApiError::internal("bad command"))?;
    let digest = Sha256::digest(&bytes);

    Ok(format!("v1:{}", URL_SAFE_NO_PAD.encode(digest)))
}

fn json_error(error: serde_json::Error) -> ApiError {
    ApiError::internal(error.to_string())
}

fn store_error(error: crate::store::StoreError) -> ApiError {
    match error {
        crate::store::StoreError::IdempotencyConflict(id) => ApiError::idempotency_conflict(
            format!("command {id} was reused with different parameters"),
        ),
        crate::store::StoreError::UpgradeRequired { kind } => {
            ApiError::unsupported_version(format!("this Space must be upgraded to read {kind}"))
        }
        other => ApiError::internal(other.to_string()),
    }
}
