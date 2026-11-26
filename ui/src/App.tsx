import { Show, createSignal, createStore, onSettled } from "solid-js";

import AuthView from "./components/AuthView";
import ChatView from "./components/ChatView";
import Rail from "./components/Rail";
import SetupView from "./components/SetupView";
import Sidebar from "./components/Sidebar";
import { t, type MessageKey } from "./i18n";
import { api, ApiError } from "./lib/api";
import { MAX_FILE_BYTES,
  MAX_FILE_LABEL, formatBytes, indexFiles, mediaTypeFor } from "./lib/files";
import { Commands, ProtocolError } from "./lib/ws";
import type {
  Account,
  Bot,
  Chat,
  Computer,
  FileSummary,
  Message,
  Model,
  Provider,
  ProviderResult,
  BotResult,
  Preview,
  RecordEvent,
  RunView,
  SendResult,
} from "./types";

type AuthMode = "join" | "login" | "missing-invite";

interface State {
  account: Account | null;
  providers: Provider[];
  models: Model[];
  bots: Bot[];
  chats: Chat[];
  previews: Record<string, Preview>;
  activeChat: Chat | null;
  messages: Message[];
  files: Record<string, FileSummary>;
  activeBot: Bot | null;
  activeComputer: Computer | null;
  working: boolean;
  busy: boolean;
  error: unknown | null;
}

const emptyState: State = {
  account: null,
  providers: [],
  models: [],
  bots: [],
  chats: [],
  previews: {},
  activeChat: null,
  messages: [],
  files: {},
  activeBot: null,
  activeComputer: null,
  working: false,
  busy: false,
  error: null,
};

export default function App() {
  const [state, setState] = createStore<State>({ ...emptyState });
  function update(patch: Partial<State>) {
    setState((draft) => {
      Object.assign(draft, patch);
    });
  }
  function setPreview(chatId: string, next: Preview) {
    setState((draft) => {
      const current = draft.previews[chatId];
      if (current === next) return;
      if (current && next && "text" in current && "text" in next) {
        if (current.text === next.text && current.file === next.file) return;
        current.text = next.text;
        current.file = next.file;
        return;
      }
      if (current && next && "error" in current && "error" in next) return;
      draft.previews[chatId] = next;
    });
  }
  const [ready, setReady] = createSignal(false);
  const [authMode, setAuthMode] = createSignal<AuthMode>("login");
  const [authSecret, setAuthSecret] = createSignal("");
  const [isMobile, setIsMobile] = createSignal(false);
  const commands = new Commands();
  let runGeneration = 0;
  let following = false;
  let refreshTimer: number | null = null;
  let refreshAgain = false;

  onSettled(() => {
    const query = window.matchMedia("(max-width: 900px)");
    setIsMobile(query.matches);
    query.addEventListener("change", (event) => setIsMobile(event.matches));

    const secret = location.hash.replace(/^#/, "");
    if (location.pathname === "/invite") {
      setAuthSecret(secret);
      setAuthMode(secret ? "join" : "missing-invite");
    } else {
      setAuthMode("login");
    }

    void boot();
  });

  async function startFollowing() {
    if (following) return;
    following = true;
    try {
      await commands.subscribe(
        {
          onSnapshot: (_events, cursor, resumed) => {
            saveCursor(cursor);
            if (resumed) queueRefresh();
          },
          onEvent: (event) => {
            saveCursor(event.seq);
            handleRecordEvent(event);
          },
          onCursor: saveCursor,
          onError: (reason) => update({ error: reason }),
        },
        { cursor: readCursor() },
      );
    } catch (reason) {
      following = false;
      update({ error: reason });
    }
  }

  function handleRecordEvent(event: RecordEvent) {
    const data = event.data ?? {};
    const chat = typeof data.chat === "string" ? data.chat : null;
    if (
      event.kind === "neha.bot.created" ||
      event.kind === "neha.chat.created" ||
      event.kind === "neha.computer.created" ||
      event.kind === "neha.provider.added"
    ) {
      void loadWorkspace();
      return;
    }
    if (event.kind === "neha.message.added" && chat) {
      const text = typeof data.text === "string" ? data.text : "";
      const file = typeof data.file === "string";
      setPreview(chat, { text, file });
    }
    if (chat && state.activeChat?.id === chat) {
      if (event.kind === "neha.run.started") update({ working: true });
      if (event.kind === "neha.run.finished") update({ working: false });
      queueRefresh();
      return;
    }
    if (event.kind === "neha.run.finished") update({ working: false });
    if (event.kind.startsWith("neha.run.") && state.working) queueRefresh();
  }

  function queueRefresh() {
    if (refreshTimer !== null) {
      refreshAgain = true;
      return;
    }
    refreshTimer = window.setTimeout(() => {
      refreshTimer = null;
      void refreshActive().finally(() => {
        if (refreshAgain) {
          refreshAgain = false;
          queueRefresh();
        }
      });
    }, 50);
  }

  async function boot() {
    try {
      const account = await api.me();
      await enter(account);
    } catch (reason) {
      if (!(reason instanceof ApiError && reason.status === 401)) {
        setAuthMode("login");
      }
      setReady(true);
    }
  }

  async function enter(account: Account) {
    update({ account });
    setReady(true);
    void startFollowing();
    await loadWorkspace();
  }

  async function loadWorkspace() {
    update({ busy: true });
    update({ error: null });
    try {
      const [providers, models, bots, chats] = await Promise.all([
        commands.command<Provider[]>("provider_list"),
        commands.command<Model[]>("model_list"),
        commands.command<Bot[]>("bot_list"),
        commands.command<Chat[]>("chat_list"),
      ]);
      update({
        providers,
        models,
        bots: [...bots].sort((left, right) => left.id.localeCompare(right.id)),
        chats: [...chats].sort((left, right) => left.id.localeCompare(right.id)),
      });
      await loadPreviews(chats);
      const active = state.activeChat ? chats.find((chat) => chat.id === state.activeChat?.id) : undefined;
      const target = active ?? (!isMobile() ? chats[0] : undefined);
      if (target) await selectChat(target);
    } catch (reason) {
      update({ error: reason });
    } finally {
      update({ busy: false });
    }
  }

  async function loadPreviews(chats: Chat[]) {
    if (chats.length === 0) {
      update({ previews: {} });
      return;
    }
    const previews: Record<string, Preview> = {};
    for (const chat of chats) {
      try {
        const messages = await commands.command<Message[]>("chat_read", { chat: chat.id });
        const last = messages.at(-1);
        previews[chat.id] = last ? previewOf(last) : null;
      } catch {
        previews[chat.id] = { error: true };
      }
    }
    update({ previews });
  }

  async function selectChat(chat: Chat) {
    const generation = ++runGeneration;
    update({ activeChat: chat, messages: [], files: {}, activeBot: null, activeComputer: null, working: false, error: null });
    try {
      const [messages, files, bot] = await Promise.all([
        commands.command<Message[]>("chat_read", { chat: chat.id }),
        commands.command<FileSummary[]>("file_list", { chat: chat.id }),
        commands.command<BotResult>("bot_show", { bot: chat.bot }),
      ]);
      if (generation !== runGeneration) return;
      update({
        messages,
        files: indexFiles(files),
        activeBot: bot.bot,
        activeComputer: bot.computer,
      });
      await restoreRun(messages, generation);
    } catch (reason) {
      update({ error: reason });
    }
  }

  async function restoreRun(messages: Message[], generation: number) {
    const run = [...messages].reverse().find((message) => message.run)?.run;
    if (!run || generation !== runGeneration) return;
    try {
      const view = await commands.command<RunView>("run_show", { run }, 10_000);
      if (generation !== runGeneration) return;
      if (view.status === "running") {
        update({ working: true });
      } else {
        await refreshActive();
      }
    } catch {
      // A Run that is already gone leaves the recorded Messages in place.
    }
  }

  async function reconcileRun(messages: Message[]) {
    const run = [...messages].reverse().find((message) => message.run)?.run;
    if (!run) return;
    try {
      const view = await commands.command<RunView>("run_show", { run }, 10_000);
      update({ working: view.status === "running" });
    } catch {
      update({ working: false });
    }
  }

  async function refreshActive() {
    const chat = state.activeChat;
    if (!chat) return;
    try {
      const [messages, files] = await Promise.all([
        commands.command<Message[]>("chat_read", { chat: chat.id }),
        commands.command<FileSummary[]>("file_list", { chat: chat.id }),
      ]);
      update({ messages, files: indexFiles(files) });
      const last = messages.at(-1);
      if (last) setPreview(chat.id, previewOf(last));
      await reconcileRun(messages);
    } catch (reason) {
      update({ error: reason });
    }
  }

  // Resolves false when nothing left the composer, so it keeps the Message.
  async function sendMessage(text: string, file: File | null): Promise<boolean> {
    const chat = state.activeChat;
    const bot = state.activeBot;
    if (!chat || !bot || state.working) return false;
    if (!text && !file) return false;

    update({ working: true, error: null });
    let uploaded: FileSummary | null = null;
    if (file) {
      uploaded = await upload(chat.id, file);
      if (!uploaded) {
        update({ working: false });
        return false;
      }
      const saved = uploaded;
      setState((draft) => { draft.files[saved.id] = saved; });
    }

    const optimistic: Message = {
      id: `optimistic-${Date.now()}`,
      chat: chat.id,
      author: { kind: "user", id: state.account?.id ?? "" },
      acting: null,
      text,
      at: new Date().toISOString(),
      seq: "optimistic",
      run: null,
      file: uploaded?.id ?? null,
    };
    setState((draft) => {
      draft.messages.push(optimistic);
    });
    setPreview(chat.id, previewOf(optimistic));
    try {
      const result = await commands.command<SendResult>(
        "message_send",
        uploaded ? { chat: chat.id, text, file: uploaded.id } : { chat: chat.id, text },
        180_000,
      );
      await refreshActive();
      if (result.status === "failed") {
        update({ error: result.error ? result.error : { key: "error.botAnswer" } });
      }
    } catch (reason) {
      update({ error: reason });
      await refreshActive();
    } finally {
      update({ working: false });
    }

    return true;
  }

  // Uploads one File, or shows why that failed and resolves null.
  async function upload(chat: string, file: File): Promise<FileSummary | null> {
    const tooLarge = () => ({
      key: "error.fileTooLarge",
      params: { name: file.name, limit: MAX_FILE_LABEL },
    });
    if (file.size > MAX_FILE_BYTES) {
      update({ error: tooLarge() });
      return null;
    }
    try {
      return await api.uploadFile(chat, file, file.name, mediaTypeFor(file));
    } catch (reason) {
      update({ error: reason instanceof ApiError && reason.status === 413 ? tooLarge() : uploadFailure(reason) });
      return null;
    }
  }

  async function connectProvider(input: {
    baseUrl: string;
    apiKey: string;
    model: string;
    contextLimit: number;
    maxOutput: number;
  }) {
    update({ busy: true, error: null });
    try {
      await commands.command<ProviderResult>("provider_add", {
        protocol: "chat_completions",
        base_url: input.baseUrl,
        api_key: input.apiKey,
        models: [
          {
            name: input.model,
            context_limit: input.contextLimit,
            max_output: input.maxOutput,
            reads_images: false,
          },
        ],
        default_model: input.model,
      });
      const [providers, models] = await Promise.all([
        commands.command<Provider[]>("provider_list"),
        commands.command<Model[]>("model_list"),
      ]);
      update({ providers, models });
    } catch (reason) {
      update({ error: reason });
    } finally {
      update({ busy: false });
    }
  }

  async function createBot(name: string) {
    update({ busy: true, error: null });
    try {
      const result = await commands.command<BotResult>("bot_create", { name });
      const [bots, chats] = await Promise.all([
        commands.command<Bot[]>("bot_list"),
        commands.command<Chat[]>("chat_list"),
      ]);
      update({
        bots: [...bots].sort((left, right) => left.id.localeCompare(right.id)),
        chats: [...chats].sort((left, right) => left.id.localeCompare(right.id)),
      });
      const sortedChats = [...chats].sort((left, right) => left.id.localeCompare(right.id));
      await loadPreviews(sortedChats);
      const chat = sortedChats.find((candidate) => candidate.id === result.chat.id) ?? chats[0];
      if (chat) await selectChat(chat);
    } catch (reason) {
      update({ error: reason });
    } finally {
      update({ busy: false });
    }
  }

  function backToList() {
    runGeneration += 1;
    update({ activeChat: null, messages: [], files: {}, activeBot: null, activeComputer: null, working: false, error: null });
  }

  const errorText = () => (state.error ? messageFor(state.error) : null);
  const needsSetup = () => state.providers.length === 0 || state.chats.length === 0;
  const showChat = () => state.activeChat !== null && !needsSetup() && state.bots.length > 0;

  return (
    <Show when={ready()} fallback={<Boot />}>
      <Show
        when={state.account}
        fallback={
          <AuthView mode={authMode()} secret={authSecret()} onAuthed={(account) => void enter(account)} />
        }
      >
        {(account) => (
          <div class={`app${showChat() ? " app--chat" : ""}${needsSetup() ? " app--setup" : ""}`}>
            <Rail account={account()} />
            <Sidebar
              account={account()}
              chats={state.chats}
              bots={state.bots}
              activeChat={state.activeChat}
              previews={state.previews}
              onSelect={(chat) => void selectChat(chat)}
            />
            <main class="main">
              <Show
                when={showChat() ? state.activeChat?.id : null}
                keyed
                fallback={
                  <Show
                    when={needsSetup()}
                    fallback={
                      <div class="select-chat">
                        <p class="eyebrow">{t("select.eyebrow")}</p>
                        <h2>{t("select.title")}</h2>
                      </div>
                    }
                  >
                    <SetupView
                      account={account()}
                      providers={state.providers}
                      models={state.models}
                      bots={state.bots}
                      busy={state.busy}
                      error={errorText()}
                      onProvider={(input) => void connectProvider(input)}
                      onBot={(name) => void createBot(name)}
                    />
                  </Show>
                }
              >
                <ChatView
                  account={account()}
                  bot={state.activeBot}
                  computer={state.activeComputer}
                  messages={state.messages}
                  files={state.files}
                  working={state.working}
                  error={errorText()}
                  onSend={sendMessage}
                  onBack={backToList}
                />
              </Show>
            </main>
          </div>
        )}
      </Show>
    </Show>
  );
}

function Boot() {
  return (
    <div class="boot">
      <span class="boot__mark">
        <svg viewBox="0 0 32 32" aria-hidden="true">
          <path d="M16 4.5 27 16 16 27.5 5 16Z" />
          <path d="M16 11.4 20.6 16 16 20.6 11.4 16Z" />
        </svg>
      </span>
      <span>{t("boot.opening")}</span>
    </div>
  );
}

// What the sidebar shows for a Chat's last Message.
function previewOf(message: Message): Preview {
  return { text: message.text, file: message.file !== null };
}

// Names an upload failure the Space or the network reported.
function uploadFailure(reason: unknown): unknown {
  if (reason instanceof ApiError && reason.code !== "unauthorized" && reason.code !== "not_found") {
    return { key: "error.upload" };
  }
  return reason;
}

function messageFor(reason: unknown): string {
  if (
    reason !== null &&
    typeof reason === "object" &&
    "key" in reason &&
    typeof (reason as { key?: unknown }).key === "string"
  ) {
    const { key, params } = reason as { key: MessageKey; params?: Record<string, string | number> };
    return t(key, params);
  }
  if (reason instanceof ProtocolError) {
    if (reason.code === "unauthorized") return t("error.session");
    if (reason.code === "not_found") return t("error.record");
    if (
      reason.code === "closed" ||
      reason.code === "network" ||
      reason.code === "timeout" ||
      reason.code === "hello_timeout"
    ) {
      return t("error.unreachable");
    }
    return t("error.generic");
  }
  if (reason instanceof ApiError) {
    if (reason.code === "unauthorized") return t("error.session");
    if (reason.code === "not_found") return t("error.record");
    if (reason.code === "rate_limited") return t("error.rate");
    if (reason.code === "invalid_argument" && /password/i.test(reason.message)) {
      return t("error.password");
    }
    if (reason.code === "invalid_argument") return t("error.invalid");
    return t("error.generic");
  }
  if (reason instanceof TypeError) return t("error.unreachable");
  if (typeof reason === "string") return reason;
  return t("error.generic");
}

const CURSOR_KEY = "neha.record.cursor";

function readCursor(): string | null {
  try {
    const value = localStorage.getItem(CURSOR_KEY);
    return value && /^(0|[1-9][0-9]*)$/.test(value) ? value : null;
  } catch {
    return null;
  }
}

function saveCursor(cursor: string) {
  if (!/^(0|[1-9][0-9]*)$/.test(cursor)) return;
  try {
    localStorage.setItem(CURSOR_KEY, cursor);
  } catch {
    // A browser that blocks storage still follows events for this page.
  }
}
