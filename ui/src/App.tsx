import { Show, createSignal, onMount } from "solid-js";
import { createStore } from "solid-js/store";

import AuthView from "./components/AuthView";
import ChatView from "./components/ChatView";
import Rail from "./components/Rail";
import SetupView from "./components/SetupView";
import Sidebar from "./components/Sidebar";
import { api, ApiError } from "./lib/api";
import { Commands, ProtocolError } from "./lib/ws";
import type {
  Account,
  Bot,
  Chat,
  Computer,
  Message,
  Model,
  Provider,
  ProviderResult,
  BotResult,
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
  previews: Record<string, string>;
  activeChat: Chat | null;
  messages: Message[];
  activeBot: Bot | null;
  activeComputer: Computer | null;
  working: boolean;
  busy: boolean;
  error: string | null;
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
  activeBot: null,
  activeComputer: null,
  working: false,
  busy: false,
  error: null,
};

export default function App() {
  const [state, setState] = createStore<State>({ ...emptyState });
  const [ready, setReady] = createSignal(false);
  const [authMode, setAuthMode] = createSignal<AuthMode>("login");
  const [authSecret, setAuthSecret] = createSignal("");
  const [isMobile, setIsMobile] = createSignal(false);
  const commands = new Commands();
  let runGeneration = 0;

  onMount(() => {
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
    setState("account", account);
    setReady(true);
    await loadWorkspace();
  }

  async function loadWorkspace() {
    setState("busy", true);
    setState("error", null);
    try {
      const [providers, models, bots, chats] = await Promise.all([
        commands.command<Provider[]>("provider_list"),
        commands.command<Model[]>("model_list"),
        commands.command<Bot[]>("bot_list"),
        commands.command<Chat[]>("chat_list"),
      ]);
      setState({
        providers,
        models,
        bots: [...bots].sort((left, right) => left.id.localeCompare(right.id)),
        chats: [...chats].sort((left, right) => left.id.localeCompare(right.id)),
      });
      await loadPreviews(chats, bots);
      const active = state.activeChat ? chats.find((chat) => chat.id === state.activeChat?.id) : undefined;
      const target = active ?? (!isMobile() ? chats[0] : undefined);
      if (target) await selectChat(target);
    } catch (reason) {
      setState("error", messageFor(reason));
    } finally {
      setState("busy", false);
    }
  }

  async function loadPreviews(chats: Chat[], bots: Bot[]) {
    if (chats.length === 0) {
      setState("previews", {});
      return;
    }
    const previews: Record<string, string> = {};
    for (const chat of chats) {
      try {
        const messages = await commands.command<Message[]>("chat_read", { chat: chat.id });
        const last = messages.at(-1);
        const bot = bots.find((candidate) => candidate.id === chat.bot);
        previews[chat.id] = last?.text ?? (bot ? `Start a Chat with ${bot.name}` : "New Chat");
      } catch {
        previews[chat.id] = "Open Chat";
      }
    }
    setState("previews", previews);
  }

  async function selectChat(chat: Chat) {
    const generation = ++runGeneration;
    setState({ activeChat: chat, messages: [], activeBot: null, activeComputer: null, working: false, error: null });
    try {
      const [messages, bot] = await Promise.all([
        commands.command<Message[]>("chat_read", { chat: chat.id }),
        commands.command<BotResult>("bot_show", { bot: chat.bot }),
      ]);
      if (generation !== runGeneration) return;
      setState({
        messages,
        activeBot: bot.bot,
        activeComputer: bot.computer,
      });
      await restoreRun(chat, messages, generation);
    } catch (reason) {
      setState("error", messageFor(reason));
    }
  }

  async function restoreRun(chat: Chat, messages: Message[], generation: number) {
    const run = [...messages].reverse().find((message) => message.run)?.run;
    if (!run || generation !== runGeneration) return;
    try {
      const view = await commands.command<RunView>("run_show", { run }, 10_000);
      if (generation !== runGeneration) return;
      if (view.status === "running") {
        setState("working", true);
        void pollRun(chat, run, generation);
      } else {
        await refreshActive();
      }
    } catch {
      // A Run that is already gone leaves the recorded Messages in place.
    }
  }

  async function pollRun(chat: Chat, run: string, generation: number) {
    while (generation === runGeneration) {
      await new Promise((resolve) => setTimeout(resolve, 1_500));
      if (generation !== runGeneration) return;
      try {
        const view = await commands.command<RunView>("run_show", { run }, 10_000);
        if (generation !== runGeneration) return;
        if (view.status !== "running") {
          setState("working", false);
          if (state.activeChat?.id === chat.id) await refreshActive();
          return;
        }
      } catch {
        setState("working", false);
        return;
      }
    }
  }

  async function refreshActive() {
    const chat = state.activeChat;
    if (!chat) return;
    try {
      const messages = await commands.command<Message[]>("chat_read", { chat: chat.id });
      setState("messages", messages);
      const last = messages.at(-1);
      if (last) setState("previews", chat.id, last.text);
    } catch (reason) {
      setState("error", messageFor(reason));
    }
  }

  async function sendMessage(text: string) {
    const chat = state.activeChat;
    const bot = state.activeBot;
    if (!chat || !bot || state.working) return;
    const trimmed = text.trim();
    if (!trimmed) return;

    const optimistic: Message = {
      id: `optimistic-${Date.now()}`,
      chat: chat.id,
      author: { kind: "user", id: state.account?.id ?? "" },
      acting: null,
      text: trimmed,
      at: new Date().toISOString(),
      seq: "optimistic",
      run: null,
    };
    setState({ messages: [...state.messages, optimistic], working: true, error: null });
    setState("previews", chat.id, trimmed);
    try {
      const result = await commands.command<SendResult>("message_send", { chat: chat.id, text: trimmed }, 180_000);
      await refreshActive();
      if (result.status === "failed") {
        setState("error", result.error ?? "The Bot could not answer.");
      }
    } catch (reason) {
      setState("error", messageFor(reason));
      await refreshActive();
    } finally {
      setState("working", false);
    }
  }

  async function connectProvider(input: {
    baseUrl: string;
    apiKey: string;
    model: string;
    contextLimit: number;
    maxOutput: number;
  }) {
    setState({ busy: true, error: null });
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
      setState({ providers, models });
    } catch (reason) {
      setState("error", messageFor(reason));
    } finally {
      setState("busy", false);
    }
  }

  async function createBot(name: string) {
    setState({ busy: true, error: null });
    try {
      const result = await commands.command<BotResult>("bot_create", { name });
      const [bots, chats] = await Promise.all([
        commands.command<Bot[]>("bot_list"),
        commands.command<Chat[]>("chat_list"),
      ]);
      setState({
        bots: [...bots].sort((left, right) => left.id.localeCompare(right.id)),
        chats: [...chats].sort((left, right) => left.id.localeCompare(right.id)),
      });
      const sortedChats = [...chats].sort((left, right) => left.id.localeCompare(right.id));
      await loadPreviews(sortedChats, bots);
      const chat = sortedChats.find((candidate) => candidate.id === result.chat.id) ?? chats[0];
      if (chat) await selectChat(chat);
    } catch (reason) {
      setState("error", messageFor(reason));
    } finally {
      setState("busy", false);
    }
  }

  function backToList() {
    runGeneration += 1;
    setState({ activeChat: null, messages: [], activeBot: null, activeComputer: null, working: false, error: null });
  }

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
          <div class="app" classList={{ "app--chat": showChat(), "app--setup": needsSetup() }}>
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
                when={showChat()}
                fallback={
                  <Show
                    when={needsSetup()}
                    fallback={
                      <div class="select-chat">
                        <p class="eyebrow">Chats</p>
                        <h2>Select a Chat to open it.</h2>
                      </div>
                    }
                  >
                    <SetupView
                      account={account()}
                      providers={state.providers}
                      models={state.models}
                      bots={state.bots}
                      busy={state.busy}
                      error={state.error}
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
                  working={state.working}
                  error={state.error}
                  onSend={(text) => void sendMessage(text)}
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
      <span>Opening the Space…</span>
    </div>
  );
}

function messageFor(reason: unknown): string {
  if (reason instanceof ProtocolError) {
    if (reason.code === "unauthorized") return "The session ended. Reload the page and sign in again.";
    if (reason.code === "not_found") return "That record is no longer available.";
    if (reason.code === "closed" || reason.code === "network") {
      return "The browser cannot reach the Space. Is `neha serve` still running?";
    }
    return reason.message;
  }
  if (reason instanceof ApiError) return reason.message;
  if (reason instanceof TypeError) return "The browser cannot reach the Space. Is `neha serve` still running?";
  return "The Space cannot complete this request.";
}
