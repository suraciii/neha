import { Show, createSignal, createStore, onCleanup, onSettled } from "solid-js";

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
  Approval,
  Bot,
  Chat,
  Computer,
  Decision,
  FileSummary,
  Message,
  Model,
  Provider,
  ProviderResult,
  BotResult,
  Preview,
  RecordEvent,
  RunProgress,
  RunView,
  SendResult,
} from "./types";

type AuthMode = "join" | "login" | "missing-invite";

// Where the followed Run is: in the queue, working, or waiting for an approval.
type RunStep = "queued" | "running" | "waiting";

// How far a Run has come. It only moves up, until a decision lets it work again.
function rank(step: RunStep): number {
  return step === "queued" ? 0 : step === "running" ? 1 : 2;
}

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
  // The active Chat's approval cards, oldest first: those still waiting and
  // the ones answered on this page.
  approvals: Approval[];
  activeBot: Bot | null;
  activeComputer: Computer | null;
  // A command (upload or send) is in flight; the composer waits for it.
  submitting: boolean;
  // The followed Run is running; the Chat shows the Bot working.
  working: boolean;
  // The followed Run waits for an approval.
  waiting: boolean;
  // The followed Run waits; the Chat shows the place it holds.
  queue: RunProgress | null;
  // The approval whose answer is on its way to the Space.
  deciding: string | null;
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
  approvals: [],
  activeBot: null,
  activeComputer: null,
  submitting: false,
  working: false,
  waiting: false,
  queue: null,
  deciding: null,
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
        draft.previews[chatId] = { text: next.text, file: next.file };
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
  let previewLoad = 0;

  // The Run this page follows: the newest one it submitted, or the one its Chat
  // was already running when it opened. It only moves forward, queued → running,
  // and back to running once an approval it waited for is granted.
  let followed: { run: string; step: RunStep } | null = null;
  // Runs the record already reported finished: a late command result or answer
  // must not revive them.
  const settledRuns = new Set<string>();

  // Whether the Chat on screen still belongs to this piece of work.
  function isCurrentChat(chat: Chat, generation: number): boolean {
    return generation === runGeneration && state.activeChat?.id === chat.id;
  }

  // The newest User request in a Chat. A Bot reply names its Run too, so only a
  // User Message may start following a Run.
  function newestRequestRun(messages: Message[]): string | null {
    return [...messages].reverse().find((message) => message.author.kind === "user" && message.run)?.run ?? null;
  }

  function isFollowing(run: string): boolean {
    return followed?.run === run;
  }

  // Follows one Run and shows the furthest state the page knows for it.
  function followRun(run: string, step: RunStep, position?: number) {
    if (settledRuns.has(run)) return;
    followed = { run, step };
    if (step === "queued") update({ working: false, waiting: false, queue: { status: "queued", position } });
    else if (step === "running") update({ working: true, waiting: false, queue: null });
    else update({ working: false, waiting: true, queue: null });
  }

  // Moves the followed Run forward: queued, then running, then waiting. An
  // answer older than the record cannot move it back, and a better place for
  // the same wait still shows.
  function advanceRun(run: string, step: RunStep, position?: number) {
    if (settledRuns.has(run)) return;
    const current = followed;
    if (!current) {
      followRun(run, step, position);
      return;
    }
    if (current.run !== run || rank(current.step) > rank(step)) return;
    if (current.step === step) {
      // The same wait, a better place.
      if (step === "queued") update({ queue: { status: "queued", position } });
      return;
    }
    followRun(run, step, position);
  }

  // Lets a waiting Run work again once its approval was granted. Only a
  // decision does this; a late answer about the record cannot.
  function resumeRun(run: string) {
    if (settledRuns.has(run) || followed?.run !== run || followed.step !== "waiting") return;
    followRun(run, "running");
  }

  function clearFollowed() {
    followed = null;
    update({ working: false, waiting: false, queue: null });
  }

  // The record says this Run ended. Only the Run still followed may settle the
  // Chat: a late answer about an earlier Run must not drop a newer one.
  function finishFollowing(run: string) {
    settledRuns.add(run);
    if (isFollowing(run)) clearFollowed();
  }

  function isUnfinished(status: RunView["status"]): status is RunStep {
    return status === "queued" || status === "running" || status === "waiting";
  }

  // Applies one record answer about the followed Run. The caller always asks
  // about that Run, so a finished answer settles the Chat.
  function applyRunView(view: RunView) {
    if (view.status === "queued") advanceRun(view.id, "queued", view.position);
    else if (view.status === "running") advanceRun(view.id, "running");
    else if (view.status === "waiting") advanceRun(view.id, "waiting");
    else finishFollowing(view.id);
  }

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

  onCleanup(() => {
    if (refreshTimer !== null) {
      window.clearTimeout(refreshTimer);
      refreshTimer = null;
    }
    void commands.close();
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
    const run = typeof data.run === "string" ? data.run : null;
    if (event.kind === "neha.run.finished") {
      // RunFinished carries no Chat. Only the followed Run may settle the Chat.
      if (!run) return;
      if (isFollowing(run)) {
        finishFollowing(run);
        queueRefresh();
        return;
      }
      settledRuns.add(run);
      // A Run of this Bot ahead of ours may have ended and left a better place.
      if (followed?.step === "queued") void refreshRunPlace(followed.run);
      else if (run === newestRequestRun(state.messages)) queueRefresh();
      return;
    }
    if (event.kind === "neha.run.queued") {
      // The command result may hold an older place than the record does.
      if (run && chat && state.activeChat?.id === chat && isFollowing(run)) void refreshRunPlace(run);
      return;
    }
    if (event.kind === "neha.run.started") {
      if (!run || !chat || state.activeChat?.id !== chat) return;
      if (isFollowing(run)) {
        advanceRun(run, "running");
      } else if (followed) {
        // Another Run of this Bot left the queue; read the place we hold now.
        if (followed.step === "queued") void refreshRunPlace(followed.run);
      } else {
        // Another client started this Run; follow it from the record.
        followRun(run, "running");
      }
      return;
    }
    if (event.kind === "neha.approval.requested") {
      const approval = approvalRequested(event);
      if (!approval || state.activeChat?.id !== approval.chat) return;
      showApproval(approval);
      // The Run stops here until the User answers, and no longer holds a place.
      if (isFollowing(approval.run)) advanceRun(approval.run, "waiting");
      else if (!followed) followRun(approval.run, "waiting");
      else if (followed.step === "queued") void refreshRunPlace(followed.run);
    }
    if (event.kind === "neha.approval.decided") {
      const approval = typeof data.approval === "string" ? data.approval : null;
      if (approval && run) applyDecision(approval, run, data.allow === true);
      return;
    }
    if (chat && state.activeChat?.id === chat) {
      queueRefresh();
    }
  }

  // Puts a new approval card in the Chat, in the order the approvals arrived.
  function showApproval(approval: Approval) {
    setState((draft) => {
      if (draft.approvals.some((known) => known.approval === approval.approval)) return;
      draft.approvals.push(approval);
      draft.approvals.sort(byApproval);
    });
  }

  // Records the User's answer on its card.
  function answerApproval(approval: string, allow: boolean) {
    setState((draft) => {
      const known = draft.approvals.find((card) => card.approval === approval);
      if (known) known.decided = allow;
    });
  }

  // Applies a decision the page heard about. A granted approval lets the Run
  // work again; a denied one is over for this page, and the record finishes it.
  function applyDecision(approval: string, run: string, allowed: boolean) {
    answerApproval(approval, allowed);
    if (!isFollowing(run)) return;
    if (allowed) resumeRun(run);
    else clearFollowed();
  }

  // Answers one approval. The card waits for the Space and keeps the verdict.
  async function decideApproval(approval: Approval, allow: boolean, lasting: boolean) {
    if (state.deciding) return;
    const generation = runGeneration;
    update({ deciding: approval.approval, error: null });
    try {
      const decision = await commands.command<Decision>("approval_decide", {
        approval: approval.approval,
        allow,
        lasting,
      });
      if (generation !== runGeneration) return;
      applyDecision(decision.approval, decision.run, decision.status === "allowed");
    } catch (reason) {
      if (generation === runGeneration) update({ error: reason });
    } finally {
      if (generation === runGeneration) update({ deciding: null });
    }
  }

  // Reads the place of the Run this page follows, once, from the record.
  async function refreshRunPlace(run: string) {
    if (!isFollowing(run) || settledRuns.has(run)) return;
    const generation = runGeneration;
    try {
      const view = await commands.command<RunView>("run_show", { run }, 10_000);
      if (generation !== runGeneration || !isFollowing(run)) return;
      applyRunView(view);
      if (view.status !== "queued" && view.status !== "running") queueRefresh();
    } catch {
      // A Run that is gone leaves the finish event to settle the Chat.
    }
  }

  // Coalesces the record reads a burst of events asks for.
  function queueRefresh() {
    if (refreshTimer !== null) {
      refreshAgain = true;
      return;
    }
    refreshTimer = window.setTimeout(() => {
      refreshTimer = null;
      const chat = state.activeChat;
      const refresh = chat ? refreshActive(chat, runGeneration) : Promise.resolve();
      void refresh.finally(() => {
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
    const generation = ++previewLoad;
    if (chats.length === 0) {
      if (generation === previewLoad) update({ previews: {} });
      return;
    }
    const before = new Map(chats.map((chat) => [chat.id, state.previews[chat.id]]));
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
    if (generation !== previewLoad) return;
    setState((draft) => {
      for (const chat of chats) {
        // A live event may have written a newer preview while this read ran.
        const next = previews[chat.id];
        if (next !== undefined && draft.previews[chat.id] === before.get(chat.id)) {
          draft.previews[chat.id] = next;
        }
      }
    });
  }

  async function selectChat(chat: Chat) {
    const generation = ++runGeneration;
    followed = null;
    update({ activeChat: chat, messages: [], files: {}, approvals: [], activeBot: null, activeComputer: null, submitting: false, working: false, waiting: false, queue: null, deciding: null, error: null });
    try {
      const [messages, files, approvals, bot] = await Promise.all([
        commands.command<Message[]>("chat_read", { chat: chat.id }),
        commands.command<FileSummary[]>("file_list", { chat: chat.id }),
        commands.command<Approval[]>("approval_list", { chat: chat.id }),
        commands.command<BotResult>("bot_show", { bot: chat.bot }),
      ]);
      if (generation !== runGeneration) return;
      update({
        messages,
        files: indexFiles(files),
        approvals: mergeApprovals(approvals, state.approvals),
        activeBot: bot.bot,
        activeComputer: bot.computer,
      });
      await restoreRun(chat, messages, generation);
    } catch (reason) {
      if (isCurrentChat(chat, generation)) update({ error: reason });
    }
  }

  async function restoreRun(chat: Chat, messages: Message[], generation: number) {
    const run = newestRequestRun(messages);
    if (!run || !isCurrentChat(chat, generation)) return;
    try {
      const view = await commands.command<RunView>("run_show", { run }, 10_000);
      if (!isCurrentChat(chat, generation)) return;
      if (isUnfinished(view.status)) {
        // A waiting Run is unfinished too: show its place and follow it.
        advanceRun(run, view.status, view.position);
      } else {
        finishFollowing(run);
        await refreshActive(chat, generation);
      }
    } catch {
      // A Run that is already gone leaves the recorded Messages in place.
    }
  }

  async function reconcileRun(chat: Chat, messages: Message[], generation: number) {
    if (!isCurrentChat(chat, generation)) return;
    // The followed Run owns the Chat; only a Chat that follows none discovers
    // the newest request in the record.
    const run = followed?.run ?? newestRequestRun(messages);
    if (!run || settledRuns.has(run)) return;
    try {
      const view = await commands.command<RunView>("run_show", { run }, 10_000);
      if (!isCurrentChat(chat, generation)) return;
      applyRunView(view);
    } catch {
      // A late answer for a Chat the reader left must not touch the new one.
      if (!isCurrentChat(chat, generation)) return;
      if (isFollowing(run)) clearFollowed();
    }
  }

  async function refreshActive(chat: Chat, generation: number) {
    if (!isCurrentChat(chat, generation)) return;
    try {
      const [messages, files, approvals] = await Promise.all([
        commands.command<Message[]>("chat_read", { chat: chat.id }),
        commands.command<FileSummary[]>("file_list", { chat: chat.id }),
        commands.command<Approval[]>("approval_list", { chat: chat.id }),
      ]);
      if (!isCurrentChat(chat, generation)) return;
      update({ messages, files: indexFiles(files), approvals: mergeApprovals(approvals, state.approvals) });
      const last = messages.at(-1);
      if (last) setPreview(chat.id, previewOf(last));
      await reconcileRun(chat, messages, generation);
    } catch (reason) {
      if (isCurrentChat(chat, generation)) update({ error: reason });
    }
  }

  // Resolves false when nothing left the composer, so it keeps the Message.
  async function sendMessage(text: string, file: File | null): Promise<boolean> {
    const chat = state.activeChat;
    const bot = state.activeBot;
    if (!chat || !bot || state.submitting) return false;
    if (!text && !file) return false;
    const generation = runGeneration;

    update({ submitting: true, error: null });
    let uploaded: FileSummary | null = null;
    if (file) {
      uploaded = await upload(chat, file, generation);
      // The reader moved on; the accepted File belongs to the old Chat.
      if (!isCurrentChat(chat, generation)) return true;
      if (!uploaded) {
        update({ submitting: false });
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
      );
      // The Space accepted the Message into its Bot's queue. The Chat stays
      // available: the reader may put another request behind this Run.
      if (!isCurrentChat(chat, generation)) return true;
      if (settledRuns.has(result.run)) {
        // The record already reported this Run finished before the answer came.
        queueRefresh();
      } else if (isFollowing(result.run)) {
        // An event already moved this Run; the result can only add its place.
        advanceRun(result.run, "queued", result.position);
      } else {
        followRun(result.run, "queued", result.position);
      }
      queueRefresh();
    } catch (reason) {
      if (!isCurrentChat(chat, generation)) return true;
      update({ error: reason });
      await refreshActive(chat, generation);
    } finally {
      if (isCurrentChat(chat, generation)) update({ submitting: false });
    }

    return true;
  }

  // Uploads one File, or shows why that failed and resolves null.
  async function upload(chat: Chat, file: File, generation: number): Promise<FileSummary | null> {
    const tooLarge = () => ({
      key: "error.fileTooLarge",
      params: { name: file.name, limit: MAX_FILE_LABEL },
    });
    if (file.size > MAX_FILE_BYTES) {
      update({ error: tooLarge() });
      return null;
    }
    try {
      return await api.uploadFile(chat.id, file, file.name, mediaTypeFor(file));
    } catch (reason) {
      if (!isCurrentChat(chat, generation)) return null;
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
    followed = null;
    update({ activeChat: null, messages: [], files: {}, approvals: [], activeBot: null, activeComputer: null, submitting: false, working: false, waiting: false, queue: null, deciding: null, error: null });
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
                  approvals={state.approvals}
                  submitting={state.submitting}
                  working={state.working}
                  waiting={state.waiting}
                  queue={state.queue}
                  deciding={state.deciding}
                  error={errorText()}
                  onSend={sendMessage}
                  onDecide={decideApproval}
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

// Whether the User answered this approval.
function isAnswered(approval: Approval): boolean {
  return approval.decided === true || approval.decided === false;
}

// Approvals arrive in order of their identifiers, which carry the time.
function byApproval(left: Approval, right: Approval): number {
  return left.approval.localeCompare(right.approval);
}

// The cards a refresh keeps: what the Space still lists, and the ones this
// page answered, which the Space no longer lists.
function mergeApprovals(listed: Approval[], shown: Approval[]): Approval[] {
  const merged = new Map<string, Approval>();
  for (const approval of shown) {
    if (isAnswered(approval)) merged.set(approval.approval, approval);
  }
  for (const approval of listed) {
    if (!merged.has(approval.approval)) merged.set(approval.approval, approval);
  }
  return [...merged.values()].sort(byApproval);
}

// The approval a `neha.approval.requested` event carries, when it is whole.
function approvalRequested(event: RecordEvent): Approval | null {
  const data = event.data;
  const text = (key: string): string | null => {
    const value = data[key];
    return typeof value === "string" ? value : null;
  };
  const approval = text("approval");
  const run = text("run");
  const bot = text("bot");
  const chat = text("chat");
  const action = text("action");
  const tool = text("tool");
  const summary = text("summary");
  const reason = data.reason;
  if (!approval || !run || !bot || !chat || !action || !tool || !summary) return null;
  if (reason !== "first_use_local" && reason !== "rule") return null;
  return {
    approval,
    run,
    bot,
    chat,
    action,
    step: typeof data.step === "number" ? data.step : 0,
    tool,
    summary,
    reason,
    rule: typeof data.rule === "string" ? data.rule : null,
  };
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
const MAX_CURSOR = "9223372036854775807";

function isCursor(value: string | null): value is string {
  if (!value || !/^(0|[1-9][0-9]*)$/.test(value)) return false;
  return value.length < MAX_CURSOR.length || (value.length === MAX_CURSOR.length && value <= MAX_CURSOR);
}

function readCursor(): string | null {
  try {
    const value = localStorage.getItem(CURSOR_KEY);
    return isCursor(value) ? value : null;
  } catch {
    return null;
  }
}

function saveCursor(cursor: string) {
  if (!isCursor(cursor)) return;
  try {
    localStorage.setItem(CURSOR_KEY, cursor);
  } catch {
    // A browser that blocks storage still follows events for this page.
  }
}
