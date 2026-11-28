import {
  createMemo,
  createSignal,
  createStore,
  For,
  onCleanup,
  onSettled,
  Show,
  untrack,
} from "solid-js";

import AuthView from "./components/AuthView";
import BotPage from "./components/BotPage";
import ChatView from "./components/ChatView";
import Icon from "./components/Icon";
import IntegrationsView from "./components/IntegrationsView";
import Rail, { type RailView } from "./components/Rail";
import SettingsPage from "./components/SettingsPage";
import SetupView from "./components/SetupView";
import Sidebar, { type SettingsSection } from "./components/Sidebar";
import SkillsView from "./components/SkillsView";
import { type MessageKey, t } from "./i18n";
import { ApiError, api } from "./lib/api";
import { readCard } from "./lib/cards";
import { indexFiles, MAX_FILE_BYTES, MAX_FILE_LABEL, mediaTypeFor } from "./lib/files";
import { watchViewportHeight } from "./lib/viewport";
import { Commands, ProtocolError } from "./lib/ws";
import type {
  Account,
  Approval,
  AttentionEntry,
  Bot,
  BotCreateResult,
  BotResult,
  Card,
  CatalogRefreshResult,
  CatalogStatus,
  CatalogVendor,
  Chat,
  ChatStatus,
  Computer,
  Decision,
  FileSummary,
  InstalledPackage,
  McpServer,
  McpSignInState,
  MemoryList,
  Message,
  Model,
  Preview,
  Provider,
  RecordEvent,
  Rule,
  RunActivity,
  RunProgress,
  RunView,
  SendResult,
  Skill,
  ThinkingLevel,
  ToolAction,
} from "./types";

type AuthMode = "join" | "login" | "missing-invite";

// What one step of a phone's history says the screen showed. A phone shows one
// surface at a time, so its system Back walks the same steps the page's own Back
// does: out of a detail, out of a surface, out of a Chat. An entry names the
// screen rather than only that something is open, so Back and Forward both land
// on a screen instead of on a guess.
interface Screen {
  // The Chat on screen, or null when the list of Chats is.
  chat: string | null;
  // The surface the rail names.
  view: RailView;
  // The Settings section open inside its surface, or null for its list.
  section: SettingsSection | null;
  // The Bot whose page is open, or null for the list of Bots.
  bot: string | null;
}

// The answer a full-access switch carries: the version of the risk the owner
// agreed to. A client that names an older one cannot widen a Bot's access.
const FULL_ACCESS_CONSENT = 1;

// Where the followed Run is: in the queue, working, or waiting for an approval.
type RunStep = "queued" | "running" | "waiting";

// How far a Run has come. It only moves up, until a decision lets it work again.
function rank(step: RunStep): number {
  return step === "queued" ? 0 : step === "running" ? 1 : 2;
}

// How the followed Run ended: whether it failed, and why.
interface RunOutcome {
  failed: boolean;
  error: string | null;
}

// What the page knows about the Runs of one Chat: the Runs that still hold it
// (queued, running, or waiting for an approval) and how the latest Run to end
// ended. Every run and approval event merges into this one projection, so a
// sibling Run that ends never takes the Chat's state with it.
interface ChatRuns {
  active: Record<string, RunStep>;
  last: "succeeded" | "failed" | null;
}

function outcomeOf(status: RunView["status"], error: string | null): RunOutcome {
  return { failed: status === "failed", error: status === "failed" ? error : null };
}

// How the record says a Run ended, from the events that name it.
function outcomeFrom(data: Record<string, unknown>): RunOutcome {
  const error = typeof data.error === "string" ? data.error : null;
  return { failed: data.status === "failed", error: data.status === "failed" ? error : null };
}

// How the record says one action ended. An action that ran past its time limit
// failed; one whose result never returned is unknown.
function actionStatusOf(status: unknown): ToolAction["status"] {
  if (status === "succeeded") return "succeeded";
  if (status === "failed" || status === "timed_out") return "failed";
  if (status === "unknown") return "unknown";
  return "unknown";
}

// Whether an action's status says it is over, so a later word may replace it.
function ended(status: ToolAction["status"]): boolean {
  return status === "succeeded" || status === "failed";
}

// The step an event names, when it names one.
function stepOf(data: Record<string, unknown>): number {
  return typeof data.step === "number" ? data.step : 0;
}

// The action an event names, when it names one.
function actionOf(data: Record<string, unknown>): string {
  return typeof data.action === "string" ? data.action : "";
}

// The tool an event names, when it names one.
function toolOf(data: Record<string, unknown>): string {
  return typeof data.tool === "string" ? data.tool : "";
}

// The failed Run's activity: what it did, and why it failed.
function failActivity(current: RunActivity | null, run: string, error: string | null): RunActivity {
  const mine = current && current.run === run ? current : null;
  return {
    run,
    step: mine?.step ?? 0,
    actions: mine ? mine.actions.map((action) => ({ ...action })) : [],
    files: mine ? [...mine.files] : [],
    error,
  };
}

// A Bot's memory before a page reads one: no entry, and no room to show. A page
// reads the entries and the limits together, so it never renders this.
function emptyMemories(): MemoryList {
  return { entries: [], used: { rule: 0, note: 0 }, limits: { rule: 0, note: 0 } };
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
  // The followed Run waits; the Chat shows the place it holds.
  queue: RunProgress | null;
  // What each Chat is doing, from the Space's projection and the record's
  // live events. A Chat the record has not touched is idle.
  runs: Record<string, ChatRuns>;
  // The Chats that received a Bot Message while another Chat was on screen.
  unread: Record<string, boolean>;
  // The approvals that still wait for the User, by approval. Each one is its
  // own entry, so a Chat with two of them needs two answers.
  pending: Record<string, Approval>;
  // The approval whose answer is on its way to the Space.
  deciding: string | null;
  busy: boolean;
  error: unknown | null;
  // The Bot page: the Bot on screen, the Computer it works on, the Computers it
  // may move to, the rules that hold it, and the memory it keeps.
  pageBot: Bot | null;
  pageComputer: Computer | null;
  computers: Computer[];
  rules: Rule[];
  memories: MemoryList;
  // The Settings surface: every vendor the catalog knows, and the cache state.
  vendors: CatalogVendor[];
  catalog: CatalogStatus | null;
  // A management read or write is in flight.
  managing: boolean;
  // What the last failed management command said.
  manageError: unknown | null;
}

// The state a page starts from. Each page builds its own, so the containers one
// page writes never belong to another.
function emptyState(): State {
  return {
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
    queue: null,
    runs: {},
    unread: {},
    pending: {},
    deciding: null,
    busy: false,
    error: null,
    pageBot: null,
    pageComputer: null,
    computers: [],
    rules: [],
    memories: emptyMemories(),
    vendors: [],
    catalog: null,
    managing: false,
    manageError: null,
  };
}

// What a ChatView renders. Most of it is the page's own state, which moves on
// to the next Chat as soon as the reader switches; a Chat that leaves keeps the
// values it showed, so its view — and the apps' frames inside it — is frozen
// rather than remounted while they answer the host's teardown request.
interface FrozenChat {
  bot: Bot | null;
  computer: Computer | null;
  messages: Message[];
  files: Record<string, FileSummary>;
  approvals: Approval[];
  submitting: boolean;
  status: ChatStatus;
  queue: RunProgress | null;
  busy: boolean;
  activity: RunActivity | null;
  deciding: string | null;
  error: string | null;
}

// One ChatView the page renders. The entry keeps its identity for as long as its
// view is mounted: a Chat that leaves is the same entry, so <For> freezes its
// view instead of remounting it.
interface ChatPane {
  id: string;
}

// How often the page asks what became of a started browser sign-in, and how
// long it keeps asking: the Space forgets a sign-in after five minutes.
const SIGN_IN_POLL_MS = 1_000;
const SIGN_IN_WINDOW_MS = 5 * 60 * 1_000;
// Starting a sign-in discovers the provider's OAuth metadata, which may take
// longer than an ordinary command may wait.
const SIGN_IN_START_TIMEOUT_MS = 60_000;

// The SKILL.md document the Skills view's Add form describes: the two required
// frontmatter fields, then the instructions as the body.
function skillDocument(name: string, description: string, body: string): string {
  const instructions = body.trim();
  const suffix = instructions === "" ? "" : `\n${instructions}\n`;
  return `---\nname: ${JSON.stringify(name)}\ndescription: ${JSON.stringify(description)}\n---\n${suffix}`;
}

// Whether an event changed what a library view lists, or who may use it.
function isLibraryEvent(kind: string): boolean {
  return (
    kind.startsWith("neha.mcp.") ||
    kind.startsWith("neha.skill.") ||
    kind.startsWith("neha.plugin.") ||
    kind.startsWith("neha.bot.")
  );
}

// How many live Messages and Cards a page keeps for reads still on their way,
// and how many Chats it may keep them for. The newest events are the only ones
// a read in flight can miss; older ones would only grow a map no answer
// retires.
const LIVE_MESSAGE_LIMIT = 100;
const LIVE_CARD_LIMIT = 100;
const LIVE_CHAT_LIMIT = 16;

// How long a Chat the reader left may keep its view — and the apps' frames
// inside it — alive while its cards answer the host's teardown request. A
// document that never answers must not hold the view forever.
const CHAT_HANDOFF_MS = 600;

// What the newest page still keeps for a read on its way. A test hook: the test
// that proves a settled read retires what it merged reads this, because the
// view shows the same row whether or not the copy is still held.
export const liveRetained = { messages: 0, chats: 0 };

// Whether two record values are the same JSON. A Message is plain data from the
// Space, so comparing its shape is how a refresh tells an unchanged row from a
// changed one.
function sameJson(left: unknown, right: unknown): boolean {
  if (left === right) return true;
  if (typeof left !== "object" || typeof right !== "object") return false;
  if (left === null || right === null) return false;
  if (Array.isArray(left) !== Array.isArray(right)) return false;
  if (Array.isArray(left) && Array.isArray(right)) {
    return (
      left.length === right.length && left.every((value, index) => sameJson(value, right[index]))
    );
  }
  const leftRecord = left as Record<string, unknown>;
  const rightRecord = right as Record<string, unknown>;
  const keys = Object.keys(leftRecord);
  if (keys.length !== Object.keys(rightRecord).length) return false;
  return keys.every(
    (key) => Object.hasOwn(rightRecord, key) && sameJson(leftRecord[key], rightRecord[key]),
  );
}

// Keeps the row identity of every Message a read did not change. Solid's <For>
// keys rows by object identity, so a refresh that rebuilt equal Messages would
// remount each row — and an open app's frame would lose its state. Only a row
// whose content changed gets a new object.
function preserveMessages(previous: Message[], next: Message[]): Message[] {
  if (previous.length === 0) return next;
  const known = new Map(previous.map((message) => [message.id, message]));
  return next.map((message) => {
    const old = known.get(message.id);
    return old && sameJson(old, message) ? old : message;
  });
}

export default function App() {
  const [state, setState] = createStore<State>(emptyState());
  // Writes the fields the caller names, or lets the caller write the store.
  function update(patch: Partial<State> | ((draft: State) => void)) {
    setState((draft) => {
      if (typeof patch === "function") patch(draft);
      else Object.assign(draft, patch);
    });
  }
  function setPreview(chatId: string, next: Preview) {
    setState((draft) => {
      const current = draft.previews[chatId];
      if (current === next) return;
      if (current && next && "text" in current && "text" in next) {
        // A Card-only Message has no text to show, so the preview keeps its
        // card flag: the sidebar names the app instead of drawing a blank line.
        const card = next.card === true;
        if (
          current.text === next.text &&
          current.file === next.file &&
          (current.card === true) === card
        )
          return;
        draft.previews[chatId] = { text: next.text, file: next.file, card };
        return;
      }
      if (current && next && "error" in current && "error" in next) return;
      draft.previews[chatId] = next;
    });
  }
  const [ready, setReady] = createSignal(false);
  const [offline, setOffline] = createSignal(false);
  const [authMode, setAuthMode] = createSignal<AuthMode>("login");
  const [authSecret, setAuthSecret] = createSignal("");
  const [isMobile, setIsMobile] = createSignal(false);
  // What the rail selected: the Chats, the approvals that wait, one of the
  // management surfaces, or one of the libraries the Space holds.
  const [view, setView] = createSignal<RailView>("chats");
  // What the Settings surface shows.
  const [section, setSection] = createSignal<SettingsSection>("providers");
  // Whether the Settings surface has a section open. A narrow viewport shows
  // one surface at a time, so opening Settings shows the list of sections and
  // choosing one opens its detail, with Back returning to the list. A wide
  // viewport shows the list beside the detail, so this only says which of the
  // two a narrow one is looking at.
  const [sectionOpen, setSectionOpen] = createSignal(false);
  // The Bot whose page is open, so a read that lands late can tell.
  const [pageBotId, setPageBotId] = createSignal<string | null>(null);
  // Which opening of a Bot page the User is on. A write that lands after they
  // opened another Bot belongs to the page they left, so every answer checks
  // this before it writes: the User may have moved on while it was on its way.
  // It counts openings rather than naming a Bot, so opening the same Bot again
  // drops what the first opening asked for. Nothing renders from it, so it is a
  // plain count rather than a signal.
  let pageGen = 0;
  // The newest memory read a Bot page asked for, and the newest answer it has
  // shown. Reads overlap — the page-opening read beside an event's read, or a
  // write's read beside either — and the record answers them in any order, so a
  // read a later one replaced is dropped: it speaks about an older record.
  let memoryReads = 0;
  let memoryShown = 0;
  // A memory change may be newer than the answer the page holds. A read that
  // starts after the change clears this; a read already on its way sees it set
  // again and asks once more.
  let memoryStale = false;
  // A memory read is on its way, so a change waits for it and lets it read once
  // more when it lands rather than racing it with a second read.
  let memoryReading = false;
  // The management surfaces the User came through, oldest first, so Back
  // returns to the one before the surface they are leaving: a narrow viewport
  // shows one surface at a time.
  const [manageTrail, setManageTrail] = createSignal<RailView[]>([]);

  // Remembers a surface Back will return to. The same surface twice in a row is
  // one step, not two.
  function pushTrail(entry: RailView) {
    setManageTrail((trail) => (trail[trail.length - 1] === entry ? trail : [...trail, entry]));
  }
  // The approval the User asked to see, so its Chat scrolls to its card.
  const [focus, setFocus] = createSignal<string | null>(null);
  // The Message a notification named, in the Chat it opened.
  const [focusMessage, setFocusMessage] = createSignal<string | null>(null);
  // What the Integrations and Skills views list. Each list is read when its
  // view is first opened, and again when the record says it changed.
  const [integrations, setIntegrations] = createSignal<McpServer[]>([]);
  const [packages, setPackages] = createSignal<InstalledPackage[]>([]);
  const [skills, setSkills] = createSignal<Skill[]>([]);
  // One library command or read is in flight.
  const [libraryBusy, setLibraryBusy] = createSignal(false);
  // What the Space refused, in the User's words: the views show it as one line.
  const [libraryError, setLibraryError] = createSignal<string | null>(null);
  // The URL of a sign-in whose window the browser would not open, so the view
  // can offer it as a link while the page keeps polling.
  const [signInFallback, setSignInFallback] = createSignal<string | null>(null);
  const commands = new Commands();
  let runGeneration = 0;
  // How many library reads and commands are in flight, so one answer does not
  // reopen the controls another answer still holds.
  let libraryWork = 0;
  let libraryRefreshTimer: number | null = null;
  let libraryRefreshInFlight = false;
  let libraryRefreshAgain = false;
  // The lists this page has read once. A view opens its lists the first time,
  // and later opens show what the record's events kept current.
  let integrationsLoaded = false;
  let packagesLoaded = false;
  let skillsLoaded = false;
  // A read of a list is already on its way, so opening the view again adds no
  // second read.
  let integrationsLoading = false;
  let skillsLoading = false;
  // The browser sign-ins this page polls while the Space has not settled them,
  // with the URL each was opened at and the window it was opened in.
  interface SignInWatch {
    timer: number;
    url: string;
    popup: Window | null;
  }
  const signInWatches = new Map<string, SignInWatch>();
  const signInAsking = new Set<string>();
  let following = false;
  let refreshTimer: number | null = null;
  let refreshAgain = false;
  let calibrating = false;
  let calibrateAgain = false;
  let previewLoad = 0;
  let mediaQuery: MediaQueryList | null = null;
  let onMediaChange: ((event: MediaQueryListEvent) => void) | null = null;
  let onPopState: (() => void) | null = null;
  let stopViewport: (() => void) | null = null;

  // The Run this page follows: the newest one it submitted, or the one its Chat
  // was already running when it opened. It only moves forward, queued → running,
  // and back to running once an approval it waited for is granted.
  let followed: { run: string; step: RunStep } | null = null;
  // The Run the page expects to follow, from the last word it has about the
  // Chat: the Space's, or a live event's. It is set before a read is sent, so a
  // read that is still on its way can tell that the page moved on.
  let expectedRun: string | null = null;
  // How many times the Run the page expects to follow changed. A read that is
  // still on its way carries the version it was sent with, and an answer that
  // arrives after the page moved on is dropped.
  let followVersion = 0;
  // Runs the record already reported finished: a late command result or answer
  // must not revive them.
  const settledRuns = new Set<string>();
  // The Chat each Run this page heard about works in, from the events that name
  // both. A Run's end names no Chat, so the page remembers it here.
  const runChats = new Map<string, string>();
  // The Cards the record named as it added Messages, by Message id, with the
  // Chat each belongs to. A read of the Chat may answer with a record older
  // than an event already heard, so a Card the page was told about survives
  // that answer.
  const liveCards = new Map<string, { chat: string; card: Card }>();
  // The Messages the record added live, by Message id. A read sent before the
  // record wrote one may answer without it, so the page keeps the Message whole
  // rather than only annotating the rows a read happens to return. Only the
  // open Chat's Messages are kept: no read of another Chat is on its way, and a
  // Message the page can never show again must not hold memory.
  const liveMessages = new Map<string, Message>();
  // The Chats whose Runs the page learned from a snapshot rather than from an
  // event. When such a Run ends, the Space's word is read once more.
  const seeded = new Set<string>();
  // The events that landed while a calibration read was in flight, so they can
  // be replayed over the answer the read gives.
  let calibrateEvents: RecordEvent[] | null = null;
  // The followed Run's tool activity, while it has any.
  const [activity, setActivity] = createSignal<RunActivity | null>(null);

  // The ChatViews the page renders: the Chat on screen, and the one it just left
  // while its apps answer the host's teardown request. Entries are kept by
  // identity, so a view that stays mounted is never remounted — a remount would
  // replace every app's frame and lose the document that must answer.
  const [panes, setPanes] = createSignal<ChatPane[]>([]);
  // What the Chat the reader left showed, while its apps close. The page never
  // reads its own state through this: it is only what the leaving view renders.
  const [leavingChat, setLeavingChat] = createSignal<{ id: string; frozen: FrozenChat } | null>(
    null,
  );
  // The bounded wait for a leaving view whose apps never answer.
  let handoffTimer: number | null = null;

  // The values the Chat on screen shows now, kept for its view once the reader
  // leaves. The fields are replaced by the page, never mutated, so the view sees
  // exactly what it showed a moment ago.
  function leavingValues(): FrozenChat {
    return {
      bot: state.activeBot,
      computer: state.activeComputer,
      messages: [...state.messages],
      // The same File index object: replacing its identity would tell every
      // Card's own effect that its File changed, and a Card would reset the
      // state it is closing.
      files: state.files,
      approvals: [...state.approvals],
      submitting: state.submitting,
      status: chatStatus(),
      queue: state.queue,
      busy: busyChat(),
      activity: shownActivity(),
      deciding: state.deciding,
      error: errorText(),
    };
  }

  // Whether a Chat carries an app whose document must answer the host's teardown
  // request before the Chat's view may leave.
  function hasApp(messages: Message[]): boolean {
    return messages.some((message) => message.card?.kind === "app");
  }

  // Stops the bounded wait and forgets the view that was leaving.
  function endHandoff() {
    if (handoffTimer !== null) {
      window.clearTimeout(handoffTimer);
      handoffTimer = null;
    }
    setLeavingChat(null);
  }

  // The view of a Chat that left has nothing more to wait for: its apps all
  // answered, or the bounded wait ended. Only now may the view — and its frames
  // — leave.
  function dropChatPane(id: string) {
    endHandoff();
    setPanes((current) => current.filter((pane) => pane.id !== id));
  }

  // The page rendered for a leaving Chat stops waiting once its last app closed.
  // It runs from the view's effect phase, so it reads the current leaving view
  // rather than tracking it.
  function finishChatRetire(id: string) {
    const leaving = untrack(leavingChat);
    if (leaving === null || leaving.id !== id) return;
    dropChatPane(id);
  }

  // Starts the bounded wait: a document that never answers the host's teardown
  // request must not hold the view forever.
  function armHandoff(id: string) {
    if (handoffTimer !== null) window.clearTimeout(handoffTimer);
    handoffTimer = window.setTimeout(() => {
      handoffTimer = null;
      setLeavingChat(null);
      setPanes((current) => current.filter((pane) => pane.id !== id));
    }, CHAT_HANDOFF_MS);
  }

  // The reader moved to another Chat, or back to the list. The view of the Chat
  // they left stays mounted — hidden — while its apps answer the host's teardown
  // request: a frame removed before its document answers is gone for good. A
  // Chat with no app card has nothing to keep, so the switch is immediate.
  function handOffChat(next: string | null) {
    const known = panes();
    const current = state.activeChat;
    if (current !== null && current.id === next) {
      // Re-opening the Chat already on screen changes no view.
      if (!known.some((pane) => pane.id === next)) setPanes([...known, { id: next }]);
      return;
    }
    const frozen = current !== null && hasApp(state.messages) ? leavingValues() : null;
    const target = next === null ? undefined : known.find((pane) => pane.id === next);
    const left = current === null ? undefined : known.find((pane) => pane.id === current.id);
    const stale = leavingChat();
    const nextPanes: ChatPane[] = [];
    for (const pane of known) {
      if (pane === target) {
        // The reader came back to a Chat that had not finished leaving: it takes
        // the screen again, in the place it already held.
        nextPanes.push(pane);
        continue;
      }
      if (pane === left) {
        if (frozen !== null) nextPanes.push(pane);
        continue;
      }
      // Only the Chat the reader just left keeps a view: one already leaving is
      // dropped, so the page never stacks transitions.
      if (stale !== null && pane.id === stale.id) continue;
      nextPanes.push(pane);
    }
    if (next !== null && !nextPanes.some((pane) => pane.id === next)) {
      nextPanes.push({ id: next });
    }
    setPanes(nextPanes);
    if (frozen !== null && left !== undefined) {
      setLeavingChat({ id: left.id, frozen });
      armHandoff(left.id);
    } else {
      endHandoff();
    }
  }

  function noteRun(run: string, chat: string) {
    if (!runChats.has(run)) runChats.set(run, chat);
  }

  // Counts what the live maps still keep, for the test hook below.
  function noteLiveRetention() {
    liveRetained.messages = liveMessages.size;
    liveRetained.chats = new Set([...liveMessages.values()].map((message) => message.chat)).size;
  }

  // Keeps the live maps within a bound: a read that never answers must not grow
  // them forever. The oldest event goes first, and an old Chat's Messages go
  // with it, because only the newest events can still be missing from a read
  // that is on its way.
  function trimLive() {
    while (liveMessages.size > LIVE_MESSAGE_LIMIT) {
      const oldest = liveMessages.keys().next().value as string | undefined;
      if (oldest === undefined) break;
      liveMessages.delete(oldest);
      liveCards.delete(oldest);
    }
    while (liveCards.size > LIVE_CARD_LIMIT) {
      const oldest = liveCards.keys().next().value as string | undefined;
      if (oldest === undefined) break;
      liveCards.delete(oldest);
    }
    const chats = new Set([...liveMessages.values()].map((message) => message.chat));
    if (chats.size > LIVE_CHAT_LIMIT) {
      const [oldestChat] = chats;
      for (const [id, message] of liveMessages) {
        if (message.chat === oldestChat) liveMessages.delete(id);
      }
      for (const [id, entry] of liveCards) {
        if (entry.chat === oldestChat) liveCards.delete(id);
      }
    }
    noteLiveRetention();
  }

  // Keeps one Message a read may still be missing.
  function keepLiveMessage(message: Message) {
    liveMessages.set(message.id, message);
    trimLive();
  }

  // Keeps one Card a read may still be missing for a Message it already has.
  function keepLiveCard(id: string, chat: string, card: Card) {
    liveCards.set(id, { chat, card });
    trimLive();
  }

  // The reader left a Chat, so no read of it is on its way: what the page kept
  // live for it retires with it.
  function forgetLiveChat(chat: string) {
    for (const [id, message] of liveMessages) {
      if (message.chat === chat) liveMessages.delete(id);
    }
    for (const [id, entry] of liveCards) {
      if (entry.chat === chat) liveCards.delete(id);
    }
    noteLiveRetention();
  }

  // Takes the Messages a read answered with, attaches the Cards the record
  // named, and appends the live Messages the answer predates. A stale read
  // therefore hides neither a Card nor a Message the page was already told
  // about: both arrive through the same live merge. A Message the answer now
  // carries needs no live copy, so that copy retires with this read.
  function withCards(chat: string, messages: Message[]): Message[] {
    const rows = new Map(messages.map((message) => [message.id, message]));
    const live = [...liveMessages.values()].filter(
      (message) => message.chat === chat && !rows.has(message.id),
    );
    const shown = live.length === 0 ? messages : [...messages, ...live];
    for (const id of rows.keys()) {
      liveMessages.delete(id);
      // A read that already names the Card no longer needs the event's copy.
      if (rows.get(id)?.card) liveCards.delete(id);
    }
    noteLiveRetention();
    if (liveCards.size === 0) return shown;
    return shown.map((message) => {
      if (message.card) return message;
      const kept = liveCards.get(message.id);
      return kept ? { ...message, card: kept.card } : message;
    });
  }

  // Writes the File index a read answered with, touching only the Files whose
  // content changed. A whole new record would tell the reader of one File that
  // every File moved, and a Card that shows one File would load it again — its
  // frame would be replaced and its app state lost — though nothing changed.
  function mergeFiles(files: FileSummary[]) {
    const next = indexFiles(files);
    setState((draft) => {
      for (const id of Object.keys(draft.files)) {
        if (!(id in next)) delete draft.files[id];
      }
      for (const [id, file] of Object.entries(next)) {
        const known = draft.files[id];
        if (known && sameJson(known, file)) continue;
        draft.files[id] = file;
      }
    });
  }

  // The Run moved to a new step, so the card counts the newest one.
  function noteStep(run: string, step: number) {
    setActivity((current) => {
      if (!current || current.run !== run) return current;
      return { ...current, step: Math.max(current.step, step) };
    });
  }

  // The Run asked a tool to do something.
  function startAction(run: string, step: number, action: string, tool: string) {
    setActivity((current) => {
      if (!current || current.run !== run) return current;
      const known = current.actions.find((entry) => entry.action === action);
      if (known) return { ...current, step: Math.max(current.step, step) };
      return {
        ...current,
        step: Math.max(current.step, step),
        actions: [...current.actions, { action, step, tool, status: "running", error: null }],
      };
    });
  }

  // The tool's action ended, one way or another. An action the page only heard
  // the end of still belongs to the card.
  function finishAction(
    run: string,
    step: number,
    action: string,
    status: ToolAction["status"],
    error: string | null,
  ) {
    setActivity((current) => {
      if (!current || current.run !== run) return current;
      const known = current.actions.find((entry) => entry.action === action);
      const done = {
        ...(known ?? { action, step, tool: "", status: "running", error: null }),
        status,
        error: status === "failed" ? error : null,
      };
      const actions = known
        ? current.actions.map((entry) => (entry.action === action ? done : entry))
        : [...current.actions, done];
      return { ...current, step: Math.max(current.step, step), actions };
    });
  }

  // The Run produced a File, so the card can show it beside the actions.
  function noteFile(run: string, id: string) {
    setActivity((current) => {
      if (!current || current.run !== run || current.files.includes(id)) return current;
      return { ...current, files: [...current.files, id] };
    });
  }

  // A Run the page heard about holds its Chat from now on.
  function holdRun(chat: string, run: string, step: RunStep) {
    setState((draft) => {
      const runs = draft.runs[chat] ?? { active: {}, last: null };
      draft.runs[chat] = runs;
      runs.active[run] = step;
    });
  }

  // The Space's word about a Run the page never saw an event for. It fills the
  // projection in and never moves a Run the record already placed: a snapshot
  // answer can be older than the events that landed since it was sent.
  function seedRun(chat: string, run: string, step: RunStep) {
    setState((draft) => {
      const runs = draft.runs[chat] ?? { active: {}, last: null };
      draft.runs[chat] = runs;
      if (runs.active[run] === undefined) runs.active[run] = step;
    });
  }

  // A Run the page heard about ended. The Chat keeps whatever else holds it:
  // a Run still waiting or working outranks one that ended.
  function endRun(chat: string | undefined, run: string, outcome: RunOutcome) {
    if (!chat) return;
    setState((draft) => {
      const runs = draft.runs[chat] ?? { active: {}, last: null };
      draft.runs[chat] = runs;
      delete runs.active[run];
      runs.last = outcome.failed ? "failed" : "succeeded";
    });
    // A snapshot names one Run for a Chat. Now that it ended, the Space's word
    // is read once more before the page decides the Chat has nothing left. The
    // write above is staged, not committed, so the page reads the projection
    // with this Run already taken out of it.
    const held = Object.keys(state.runs[chat]?.active ?? {}).some((id) => id !== run);
    if (seeded.has(chat) && !held) {
      seeded.delete(chat);
      queueCalibrate();
    }
  }

  // The merged projection of the Runs the page heard about in one Chat, when it
  // has heard of any. A Run that waits for an approval outranks one that works,
  // and a Run that works outranks the latest one that ended.
  function projectRuns(chatId: string): ChatStatus | null {
    const runs = state.runs[chatId];
    if (!runs) return null;
    const active = Object.values(runs.active);
    if (active.includes("waiting")) return "waiting";
    if (active.length > 0) return "working";
    if (runs.last === "failed") return "failed";
    if (runs.last === "succeeded") return "idle";
    return null;
  }

  // What a Chat is doing: the merged projection of its Runs, else the Space's
  // snapshot, else idle.
  function statusOf(chat: Chat): ChatStatus {
    return projectRuns(chat.id) ?? chat.status ?? "idle";
  }

  // Whether the Chat on screen still belongs to this piece of work.
  function isCurrentChat(chat: Chat, generation: number): boolean {
    return generation === runGeneration && state.activeChat?.id === chat.id;
  }

  // The newest User request in a Chat. A Bot reply names its Run too, so only a
  // User Message may start following a Run.
  function newestRequestRun(messages: Message[]): string | null {
    return (
      [...messages].reverse().find((message) => message.author.kind === "user" && message.run)
        ?.run ?? null
    );
  }

  // The Run a Chat's record points at when the Space names none. A Chat that
  // failed shows the Run that failed: the last reply in the record carries its
  // run, and a reply is the end of a Run — the one the User turned down, not
  // the newest request that happens to sit above it. A Chat whose record holds
  // no reply yet falls back to the request it shows no answer for, which is the
  // one a Chat that follows no Run failed on. A Chat whose every request was
  // answered has only the newest one to show.
  function historyRun(messages: Message[], failed: boolean): string | null {
    if (failed) {
      const ended = [...messages]
        .reverse()
        .find((message) => message.author.kind === "bot" && message.run);
      if (ended?.run) return ended.run;
    }
    const answered = new Set(
      messages
        .filter((message) => message.author.kind === "bot" && message.run)
        .map((message) => message.run),
    );
    const waiting = [...messages]
      .reverse()
      .find(
        (message) => message.author.kind === "user" && message.run && !answered.has(message.run),
      );
    return waiting?.run ?? newestRequestRun(messages);
  }

  function isFollowing(run: string): boolean {
    return followed?.run === run;
  }

  // Follows one Run and shows the furthest state the page knows for it.
  function followRun(run: string, step: RunStep, position?: number) {
    if (settledRuns.has(run)) return;
    expectRun(run);
    followed = { run, step };
    update((draft) => {
      draft.queue = step === "queued" ? { status: "queued", position } : null;
    });
    // The Chat works on this Run now, whatever else it holds.
    const chat = state.activeChat;
    if (chat) holdRun(chat.id, run, step);
    setActivity((current) =>
      current?.run === run ? current : { run, step: 0, actions: [], files: [], error: null },
    );
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
    expectRun(null);
    setActivity(null);
    update({ queue: null });
  }

  // The Run the page expects to follow changed. Every change counts, so a read
  // that is still on its way can tell from its answer that the page moved on.
  function expectRun(run: string | null) {
    if (expectedRun === run) return;
    expectedRun = run;
    followVersion += 1;
  }

  // The Space names a Run other than the one the page follows: the page follows
  // the one it names. The old follow goes, and with it the card that showed its
  // work and the place the page held in the queue. The Run itself keeps its
  // place in the Chat's projection: it still works, and the Chat stays busy
  // until it ends or the Space says the Chat is idle. A Chat the Space names no
  // Run for keeps whatever it holds: the page reads its history, and its status
  // is the Space's word.
  function followNamed(run: string | null | undefined) {
    const named = run ?? null;
    if (!named || expectedRun === named) return;
    expectRun(named);
    followed = null;
    update({ queue: null });
    setActivity(null);
  }

  // The Space says a Chat holds no Run and is idle: the page lets go of the Runs
  // it still holds for it, so a Run that works while the page follows another
  // does not keep the Chat busy after the Space says the Chat is free.
  function releaseRuns(chat: string) {
    setState((draft) => {
      const runs = draft.runs[chat];
      if (runs) runs.active = {};
    });
  }

  // The record says this Run ended. Only the Run still followed may settle the
  // Chat: a late answer about an earlier Run must not drop a newer one.
  function finishFollowing(run: string, outcome: RunOutcome = { failed: false, error: null }) {
    settledRuns.add(run);
    const wasFollowing = isFollowing(run);
    if (wasFollowing) {
      followed = null;
      // The Run the page expected is over, so it expects none.
      expectRun(null);
    }
    const chat = state.activeChat;
    endRun(chat?.id, run, outcome);
    forgetRunAttention(run);
    if (!wasFollowing) return;
    update({ queue: null });
    // A failed Run keeps one line in the stream, with its reason behind it; a
    // Run that answered leaves its reply Message in place.
    setActivity((current) => {
      if (!outcome.failed) return null;
      const mine = current && current.run === run ? current : null;
      return {
        run,
        step: mine?.step ?? 0,
        actions: mine ? mine.actions.map((action) => ({ ...action })) : [],
        files: mine ? [...mine.files] : [],
        error: outcome.error,
      };
    });
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
    else finishFollowing(view.id, outcomeOf(view.status, view.error));
    // A reopened Chat shows what the Run already did: its recorded steps and
    // actions, which the events of a Run the page never saw cannot tell. A Run
    // that answered leaves its reply in the stream instead.
    if (view.status !== "succeeded")
      restoreActivity(view, view.status === "failed" ? (view.error ?? null) : null);
  }

  // Puts the recorded steps and actions of one Run into its activity card.
  function restoreActivity(view: RunView, error: string | null = null) {
    const steps = view.steps ?? [];
    const actions = (view.actions ?? []).map((action) => ({
      action: action.id,
      step: action.step,
      tool: action.tool,
      status: actionStatusOf(action.status),
      error: action.error ?? null,
    }));
    // A Run that failed before its first step has no steps and no actions to
    // restore, but it still has its reason to show.
    if (steps.length === 0 && actions.length === 0 && !error) return;
    setActivity((current) => {
      if (current && current.run !== view.id) return current;
      const mine = current?.run === view.id ? current : null;
      // Each action keeps its own place in the card, so a newer word about one
      // replaces it there: the card names an action once.
      const merged = mine ? mine.actions.map((entry) => ({ ...entry })) : [];
      for (const action of actions) {
        const known = merged.find((entry) => entry.action === action.action);
        // A newer word about an action wins, but only a finished one: a Run the
        // Space compacted can name an action unknown, and an answer that reads
        // older than the record must not put a finished action back to work.
        if (!known) merged.push(action);
        else if (ended(action.status)) merged[merged.indexOf(known)] = action;
      }
      return {
        run: view.id,
        step: steps.reduce((highest, step) => Math.max(highest, step.index), mine?.step ?? 0),
        actions: merged,
        files: mine ? mine.files : [],
        error: error ?? mine?.error ?? null,
      };
    });
  }

  onSettled(() => {
    mediaQuery = window.matchMedia("(max-width: 900px)");
    setIsMobile(mediaQuery.matches);
    onMediaChange = (event) => {
      setIsMobile(event.matches);
      // A Chat chosen while the window was wide still needs its Back entry.
      if (event.matches && state.activeChat) remember(screen(), true);
    };
    mediaQuery.addEventListener("change", onMediaChange);
    stopViewport = watchViewportHeight();

    onPopState = () => {
      if (!isMobile()) return;
      // The entry the User landed on names the screen it showed, so a Back or a
      // Forward restores that screen: out of a detail, out of a surface, out of
      // a Chat, in that order.
      void showScreen(screenOf(history.state));
    };
    window.addEventListener("popstate", onPopState);

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
    if (mediaQuery && onMediaChange) mediaQuery.removeEventListener("change", onMediaChange);
    if (onPopState) window.removeEventListener("popstate", onPopState);
    if (stopViewport) stopViewport();
    if (handoffTimer !== null) {
      window.clearTimeout(handoffTimer);
      handoffTimer = null;
    }
    if (libraryRefreshTimer !== null) {
      window.clearTimeout(libraryRefreshTimer);
      libraryRefreshTimer = null;
    }
    for (const watch of signInWatches.values()) window.clearInterval(watch.timer);
    signInWatches.clear();
    signInAsking.clear();
    setSignInFallback(null);
    void commands.close();
  });

  async function startFollowing() {
    if (following) return;
    following = true;
    try {
      await commands.subscribe(
        {
          // A page that asked to continue reads the events it missed: the gap
          // is bounded, and the page was gone for it. A page that asked for no
          // cursor has nothing to catch up on, so it takes the Space's word
          // instead of reading the whole record.
          onSnapshot: (events, cursor, resumed) => {
            saveCursor(cursor);
            if (resumed) {
              for (const event of events ?? []) {
                saveCursor(event.seq);
                handleRecordEvent(event);
              }
            }
            queueCalibrate();
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

  // Reads the Space's word again after a snapshot: what every Chat is doing,
  // and every approval that still waits. This calibrates the whole attention
  // list, not only the Chat on screen, because the events a page missed can
  // leave an approval waiting in any Chat.
  async function calibrate() {
    // The events that land while these reads run are replayed over the answer,
    // in order: the answer describes the record as it was when the reads were
    // sent, and the events since are the newer word about it.
    const during: RecordEvent[] = [];
    calibrateEvents = during;
    try {
      let chats: Chat[];
      try {
        chats = await commands.command<Chat[]>("chat_list");
      } catch {
        // A read that failed leaves the replayed events as the answer.
        return;
      }
      update({ chats: [...chats].sort((left, right) => left.id.localeCompare(right.id)) });
      let approvals: Approval[];
      try {
        // One read names every approval that waits for this User, in every
        // Chat, so a late answer cannot bring an answered one back.
        approvals = await commands.command<Approval[]>("approval_list", {});
      } catch {
        // Without the Space's word the page keeps what its events tell it.
        approvals = [];
        if (state.activeChat) await refreshActive(state.activeChat, runGeneration);
        return;
      }
      // The Space's word about each Chat becomes part of the page's own
      // projection of Runs, so a Run the page never saw an event for still
      // counts: its Chat waits while it waits, and its end lands on the Chat it
      // belongs to. A Run the page already heard end is not put back to work:
      // an answer about the record as it was does not unmake the User's news.
      for (const chat of chats) {
        const run = chat.active_run;
        if (!run || settledRuns.has(run)) continue;
        if (chat.status !== "working" && chat.status !== "waiting") continue;
        noteRun(run, chat.id);
        seedRun(chat.id, run, chat.status === "waiting" ? "waiting" : "running");
        seeded.add(chat.id);
      }
      for (const approval of approvals) {
        if (settledRuns.has(approval.run)) continue;
        noteRun(approval.run, approval.chat);
        seedRun(approval.chat, approval.run, "waiting");
        seeded.add(approval.chat);
      }
      setState((draft) => {
        // The Space's answer is the authority: an approval it no longer lists
        // is answered, wherever the page last heard about it.
        for (const [id] of Object.entries(draft.pending)) {
          if (!approvals.some((known) => known.approval === id)) delete draft.pending[id];
        }
        for (const approval of approvals) draft.pending[approval.approval] = approval;
      });
      // The Chat on screen takes the Space's word about itself over the copy it
      // holds: the header, the badge and the composer read the same projection,
      // and the page follows the Run the Space names for it — the one that
      // works, not the one that queued behind it. The write is staged while the
      // answer is read back, so the answer itself is what answers.
      const answered = state.activeChat
        ? { ...state.activeChat, ...chats.find((chat) => chat.id === state.activeChat?.id) }
        : null;
      if (answered) {
        // The Space's word is the authority: when it names a Run other than the
        // one the page follows, the page follows the one it names — the Run the
        // User let work while the page was away, not the one it last heard
        // about.
        followNamed(answered.active_run);
        // A Chat the Space says holds no Run and is idle keeps nothing: the page
        // lets go of the follow, the place it held in the queue and the Runs it
        // still holds, so the composer opens again — and a read that is still on
        // its way about a Run the Space no longer names is dropped when it
        // lands, because the version it was sent with is no longer the page's.
        if (answered.active_run === null && answered.status === "idle") {
          clearFollowed();
          releaseRuns(answered.id);
        }
        update({ activeChat: answered });
        await refreshActive(answered, runGeneration);
      }
    } finally {
      const landed = [...during];
      if (calibrateEvents === during) calibrateEvents = null;
      // What landed while the reads ran is replayed over the answer, so an
      // answer the User has already given stays given.
      for (const event of landed) handleRecordEvent(event);
    }
  }

  // Coalesces the calibrations a burst of snapshot frames asks for: the page
  // takes the Space's word now, and once more after the last frame has landed.
  function queueCalibrate() {
    if (calibrating) {
      calibrateAgain = true;
      return;
    }
    calibrating = true;
    void calibrate().finally(() => {
      calibrating = false;
      if (calibrateAgain) {
        calibrateAgain = false;
        queueCalibrate();
      }
    });
  }

  function handleRecordEvent(event: RecordEvent) {
    // A calibration read in flight answers about the record as it was when the
    // read was sent, so what lands now is replayed over that answer later.
    calibrateEvents?.push(event);
    const data = event.data ?? {};
    const chat = typeof data.chat === "string" ? data.chat : null;
    if (
      event.kind === "neha.bot.created" ||
      event.kind === "neha.chat.created" ||
      event.kind === "neha.computer.created" ||
      event.kind === "neha.provider.added"
    ) {
      void loadWorkspace();
      // A Computer another client paired or created is one the Settings
      // surface lists.
      if (event.kind === "neha.computer.created") void refreshComputers();
      return;
    }
    // Another client changed who may use a Computer: the Settings list and an
    // open Bot's page both show its sharing.
    if (event.kind === "neha.computer.updated") {
      void refreshComputers();
      const opened = pageBotId();
      if (opened) void reloadPageBot(opened);
      return;
    }
    // A Bot's page reads its own Bot, its Computers, and its rules again, so a
    // change another client made shows without a reload.
    if (
      event.kind === "neha.bot.updated" ||
      event.kind === "neha.bot.moved" ||
      event.kind === "neha.bot.renamed"
    ) {
      void refreshBots();
      const opened = pageBotId();
      if (opened) void reloadPageBot(opened);
      // A rename renames the Chat on screen too: its header names the Bot, so
      // it shows the new name without waiting for a read.
      const name = typeof data.name === "string" ? data.name : null;
      const bot = typeof data.bot === "string" ? data.bot : null;
      if (bot !== null && name !== null && state.activeBot?.id === bot) {
        update({ activeBot: { ...state.activeBot, name } });
      }
      return;
    }
    if (event.kind === "neha.rule.added" || event.kind === "neha.rule.removed") {
      void refreshRules();
      return;
    }
    if (event.kind === "neha.memory.added" || event.kind === "neha.memory.removed") {
      // A memory event names its Bot only when it adds an entry; a removal
      // belongs to the Bot whose page is open, so any open page reads again. A
      // page that is still opening reads once more after its first answer,
      // because that answer may predate the event.
      const changed = typeof data.bot === "string" ? data.bot : null;
      const opened = pageBotId();
      if (opened === null || changed === null || changed === opened) {
        memoryStale = true;
        if (opened !== null) void refreshMemories();
      }
      return;
    }
    if (event.kind === "neha.provider.models_updated") {
      void refreshProviders();
      return;
    }
    // An event that changed what a library holds or who may use it: the views
    // read the lists it touched again. Every other event leaves them alone.
    if (isLibraryEvent(event.kind)) queueLibraryRefresh();
    const run = typeof data.run === "string" ? data.run : null;
    if (event.kind === "neha.message.added" && chat) {
      const text = typeof data.text === "string" ? data.text : "";
      const carried = typeof data.file === "string" ? data.file : null;
      // A tool's Card arrives with the Message that carries it; keep it for the
      // read that follows, which may answer with the record as it was before.
      const card = readCard(data.card);
      const id = typeof data.message === "string" ? data.message : null;
      // Only the open Chat may keep a live copy: the page reads no other Chat,
      // so no read of it is on its way that could answer without this Message,
      // and a copy no read retires would hold memory forever. A Message or Card
      // the view already holds needs no copy either.
      const shown = chat === state.activeChat?.id;
      const known = shown ? state.messages.find((message) => message.id === id) : undefined;
      if (shown && card && id && !known?.card) keepLiveCard(id, chat, card);
      // The read that follows may predate the record's write of this Message,
      // so the live event keeps the whole Message, not only its Card.
      if (shown && id && !known) {
        keepLiveMessage({
          id,
          chat,
          author: event.author,
          acting: event.acting ?? null,
          text,
          at: event.at,
          seq: event.seq,
          run,
          file: carried,
          ...(card ? { card } : {}),
        });
      }
      // A Card-only Message has no text, so the preview names the app instead.
      setPreview(chat, { text, file: carried !== null, card: card !== null });
      // A Bot reply names its Run, which is how the page learns which Chat a
      // Run belongs to: the events that end a Run name no Chat.
      if (run) noteRun(run, chat);
      // A reply that carries a File names the Run that made it, so the File
      // belongs to that Run's work: only a Bot reply does. A File the User
      // uploaded is theirs, and a File another Run of the same Bot made is
      // that Run's.
      if (event.author.kind === "bot" && carried && run && readsChat(chat) && isFollowing(run)) {
        noteFile(run, carried);
      }
      if (event.author.kind === "bot" && !readsChat(chat)) markUnread(chat);
    }
    if (event.kind === "neha.run.queued") {
      if (run && chat) {
        noteRun(run, chat);
        holdRun(chat, run, "queued");
        // The command result may hold an older place than the record does.
        if (state.activeChat?.id === chat && isFollowing(run)) void refreshRunPlace(run);
      }
      return;
    }
    if (event.kind === "neha.run.started") {
      if (!run || !chat) return;
      noteRun(run, chat);
      holdRun(chat, run, "running");
      if (state.activeChat?.id !== chat) return;
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
    if (event.kind === "neha.run.step" && run && isFollowing(run)) {
      noteStep(run, typeof data.step === "number" ? data.step : 0);
    }
    if (event.kind === "neha.action.started" && run && isFollowing(run)) {
      startAction(run, stepOf(data), actionOf(data), toolOf(data));
    }
    if (event.kind === "neha.action.finished" && run && isFollowing(run)) {
      const error = typeof data.error === "string" ? data.error : null;
      finishAction(run, stepOf(data), actionOf(data), actionStatusOf(data.status), error);
    }
    if (event.kind === "neha.run.finished") {
      // RunFinished carries no Chat, so the page looks up the Chat it remembered
      // for the Run. Only the followed Run may settle the Chat on screen.
      if (!run) return;
      const outcome = outcomeFrom(data);
      if (isFollowing(run)) {
        finishFollowing(run, outcome);
        queueRefresh();
        return;
      }
      settledRuns.add(run);
      settleChat(runChats.get(run), run, outcome);
      // A Run of this Bot ahead of ours may have ended and left a better place.
      if (followed?.step === "queued") void refreshRunPlace(followed.run);
      else if (run === newestRequestRun(state.messages)) queueRefresh();
      return;
    }
    if (event.kind === "neha.approval.requested") {
      const approval = approvalRequested(event);
      if (!approval) return;
      noteRun(approval.run, approval.chat);
      holdRun(approval.chat, approval.run, "waiting");
      // Every approval that waits is its own entry, wherever it runs.
      rememberAttention(approval);
      if (approval.chat !== state.activeChat?.id) return;
      showApproval(approval);
      // The Run stops here until the User answers, and no longer holds a place.
      if (isFollowing(approval.run)) advanceRun(approval.run, "waiting");
      else if (!followed) followRun(approval.run, "waiting");
      else if (followed.step === "queued") void refreshRunPlace(followed.run);
    }
    if (event.kind === "neha.approval.decided") {
      const approval = typeof data.approval === "string" ? data.approval : null;
      if (approval && run) applyDecision(approval, run, data.allow === true);
      // A granted approval lets its Run work again, wherever it runs; a denied
      // one ends it. Either way the approval no longer waits for the User.
      if (run) {
        const owner = runChats.get(run);
        if (owner) {
          if (data.allow === true) holdRun(owner, run, "running");
          else endRun(owner, run, { failed: true, error: null });
        }
      }
      forgetAttention(approval);
      return;
    }
    if (chat && state.activeChat?.id === chat) {
      queueRefresh();
    }
  }

  // A Chat the User is reading. A management surface leaves the Chat open, but
  // the User is not reading it there, so its new activity is still new.
  const readsChat = (chat: string) => view() === "chats" && state.activeChat?.id === chat;

  // A Bot Message that arrived while another Chat was on screen is new activity
  // for that Chat. Opening it answers the mark.
  function markUnread(chat: string) {
    setState((draft) => {
      draft.unread[chat] = true;
    });
  }

  // An approval that waits for the User, so the attention list can name it.
  // Opening its Chat does not clear it: only the User's answer does.
  function rememberAttention(approval: Approval) {
    setState((draft) => {
      draft.pending[approval.approval] = approval;
    });
  }

  // An approval that was answered no longer needs attention.
  function forgetAttention(approval: string | null) {
    if (!approval) return;
    setState((draft) => {
      delete draft.pending[approval];
    });
  }

  // Every approval one Run waited for stops needing attention with it.
  function forgetRunAttention(run: string) {
    setState((draft) => {
      for (const [id, waiting] of Object.entries(draft.pending)) {
        if (waiting.run === run) delete draft.pending[id];
      }
    });
  }

  // A Run the page heard about ended: its Chat keeps whatever else holds it,
  // and the approvals it waited for no longer need attention.
  function settleChat(chat: string | undefined, run: string, outcome: RunOutcome) {
    if (!chat) return;
    endRun(chat, run, outcome);
    forgetRunAttention(run);
    // A Chat on screen that follows no Run of its own keeps the failure in its
    // header and one line in its stream, but only once nothing else holds the
    // Chat: a sibling Run that still works outranks the one that ended.
    if (
      outcome.failed &&
      state.activeChat?.id === chat &&
      !followed &&
      projectRuns(chat) === "failed"
    ) {
      setActivity((current) => failActivity(current, run, outcome.error));
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

  // Puts one view on screen. A library view reads what it lists the first time
  // it opens; later opens show what the record kept current.
  function showView(next: RailView) {
    setView(next);
    if (next !== "integrations" && next !== "skills") return;
    // A refusal from an earlier visit does not belong to the list being opened.
    setLibraryError(null);
    if (next === "integrations") void loadIntegrations();
    else void loadSkills();
  }

  // Opens a view from the rail or the sidebar, or the Chats a library's own
  // Back selects. On a phone a surface the rail selects is a step of its own,
  // so the history gets an entry for it: the system Back then leaves the
  // library for the surface before it rather than skipping it.
  function selectView(next: RailView) {
    remember({ chat: state.activeChat?.id ?? null, view: next, section: null, bot: null });
    showView(next);
  }

  function isAdmin(): boolean {
    return state.account?.is_admin === true;
  }

  function beginLibraryWork() {
    libraryWork += 1;
    setLibraryBusy(true);
  }

  function endLibraryWork() {
    libraryWork = Math.max(0, libraryWork - 1);
    if (libraryWork === 0) setLibraryBusy(false);
  }

  // The Space's MCP servers and, for the Admin, the packages it installed. A
  // non-Admin never asks for the packages: the Space refuses that read. The
  // view still groups the servers a package brought, under a neutral name.
  async function loadIntegrations(): Promise<void> {
    if (integrationsLoaded && (packagesLoaded || !isAdmin())) return;
    if (integrationsLoading) return;
    integrationsLoading = true;
    beginLibraryWork();
    setLibraryError(null);
    try {
      if (!integrationsLoaded) {
        const servers = await commands.command<McpServer[]>("mcp_list");
        setIntegrations(servers);
        integrationsLoaded = true;
      }
      if (isAdmin() && !packagesLoaded) {
        const installed = await commands.command<InstalledPackage[]>("plugin_list");
        setPackages(installed);
        packagesLoaded = true;
      }
    } catch (reason) {
      setLibraryError(messageFor(reason));
    } finally {
      integrationsLoading = false;
      endLibraryWork();
    }
  }

  // The skills the Space and the reader's Bots hold. The packages an Admin may
  // read come with them, so a skill a package brought can name it.
  async function loadSkills(): Promise<void> {
    if (skillsLoaded && (packagesLoaded || !isAdmin())) return;
    if (skillsLoading) return;
    skillsLoading = true;
    beginLibraryWork();
    setLibraryError(null);
    try {
      if (!skillsLoaded) {
        const listed = await commands.command<Skill[]>("skill_list", {});
        setSkills(listed);
        skillsLoaded = true;
      }
      if (isAdmin() && !packagesLoaded) {
        const installed = await commands.command<InstalledPackage[]>("plugin_list");
        setPackages(installed);
        packagesLoaded = true;
      }
    } catch (reason) {
      setLibraryError(messageFor(reason));
    } finally {
      skillsLoading = false;
      endLibraryWork();
    }
  }

  // Reads the lists the views already hold: a list a User never opened is left
  // alone, so an event for it changes nothing until its view opens.
  async function refreshLibraries(): Promise<void> {
    if (!integrationsLoaded && !skillsLoaded) return;
    beginLibraryWork();
    try {
      const reads: Promise<void>[] = [];
      if (integrationsLoaded) {
        reads.push(
          commands.command<McpServer[]>("mcp_list").then((servers) => {
            setIntegrations(servers);
          }),
        );
        if (isAdmin()) {
          reads.push(
            commands.command<InstalledPackage[]>("plugin_list").then((installed) => {
              setPackages(installed);
              packagesLoaded = true;
            }),
          );
        }
      }
      if (skillsLoaded) {
        reads.push(
          commands.command<Skill[]>("skill_list", {}).then((listed) => {
            setSkills(listed);
          }),
        );
      }
      // Both views name Bots and toggle their servers and skills, so the Bots
      // are read with them. Each one is copied: a row keyed by its object must
      // see the server and skill lists the read replaced.
      reads.push(
        commands.command<Bot[]>("bot_list").then((listed) => {
          update({
            bots: listed
              .map((bot) => ({ ...bot }))
              .sort((left, right) => left.id.localeCompare(right.id)),
          });
        }),
      );
      await Promise.all(reads);
    } catch (reason) {
      setLibraryError(messageFor(reason));
    } finally {
      endLibraryWork();
    }
  }

  // Coalesces the reads a burst of library events and command answers asks
  // for, so an event about a command the page just ran is one read, not two.
  // A request that lands while a read runs asks for one more read: its change
  // arrived after the answer the read was sent with.
  function queueLibraryRefresh(): void {
    if (libraryRefreshInFlight) {
      libraryRefreshAgain = true;
      return;
    }
    if (libraryRefreshTimer !== null) window.clearTimeout(libraryRefreshTimer);
    libraryRefreshTimer = window.setTimeout(() => {
      libraryRefreshTimer = null;
      libraryRefreshInFlight = true;
      void refreshLibraries().finally(() => {
        libraryRefreshInFlight = false;
        if (libraryRefreshAgain) {
          libraryRefreshAgain = false;
          queueLibraryRefresh();
        }
      });
    }, 50);
  }

  // Runs one library command. The views wait while it is in flight, a refusal
  // shows above the list it would have changed, and an accepted change is read
  // back from the Space after the record's own event has landed.
  async function libraryCommand(run: () => Promise<unknown>): Promise<void> {
    if (libraryBusy()) return;
    beginLibraryWork();
    setLibraryError(null);
    let accepted = false;
    try {
      await run();
      accepted = true;
    } catch (reason) {
      setLibraryError(messageFor(reason));
    } finally {
      endLibraryWork();
    }
    if (accepted) queueLibraryRefresh();
  }

  // Connects an MCP server. The token lives only in this call and the frame it
  // sends: it never enters a signal, a log, or the page after the answer.
  function addIntegration(input: {
    name: string;
    url: string;
    scope: "space" | "user";
    token: string;
  }): Promise<void> {
    const args: Record<string, unknown> = { name: input.name, url: input.url, scope: input.scope };
    if (input.token !== "") args.credential = input.token;
    return libraryCommand(() => commands.command("mcp_add", args));
  }

  function connectIntegration(server: string, token: string): Promise<void> {
    return libraryCommand(() => commands.command("mcp_connect", { server, credential: token }));
  }

  function removeIntegration(server: string): Promise<void> {
    return libraryCommand(() => commands.command("mcp_remove", { server }));
  }

  function setBotIntegration(bot: string, server: string, enabled: boolean): Promise<void> {
    return libraryCommand(() =>
      commands.command(enabled ? "bot_mcp_enable" : "bot_mcp_disable", { bot, server }),
    );
  }

  function setPackageEnabled(plugin: string, enabled: boolean): Promise<void> {
    return libraryCommand(() =>
      commands.command(enabled ? "plugin_enable" : "plugin_disable", { plugin }),
    );
  }

  // Adds a skill to the library the Add form names. The Space reads one
  // SKILL.md document, so the form's fields become its frontmatter and body.
  function addSkill(input: {
    name: string;
    description: string;
    body: string;
    bot: string | null;
    scripts: { name: string; content: string }[];
  }): Promise<void> {
    const document = skillDocument(input.name, input.description, input.body);
    const args =
      input.bot === null
        ? { library: "space", document, scripts: input.scripts }
        : { library: "bot", bot: input.bot, document, scripts: input.scripts };
    return libraryCommand(() => commands.command("skill_add", args));
  }

  function removeSkill(skill: string): Promise<void> {
    return libraryCommand(() => commands.command("skill_remove", { skill }));
  }

  function setBotSkill(bot: string, skill: string, enabled: boolean): Promise<void> {
    return libraryCommand(() =>
      commands.command(enabled ? "bot_skill_enable" : "bot_skill_disable", { bot, skill }),
    );
  }

  // Opens the blank window a browser sign-in will use. The click that starts
  // the sign-in is the browser's permission to open a window; that permission
  // is gone by the time the command's answer arrives, so the window is opened
  // before the command is sent.
  function openSignInWindow(): Window | null {
    try {
      const opened = window.open("about:blank", "_blank");
      if (opened) {
        // The page this window reaches later has no way back into this one.
        try {
          opened.opener = null;
        } catch {
          // A window that already navigated keeps its opener; nothing here can.
        }
      }
      return opened ?? null;
    } catch {
      return null;
    }
  }

  function closeSignInWindow(popup: Window | null): void {
    try {
      if (popup && !popup.closed) popup.close();
    } catch {
      // A window the browser already closed needs no second close.
    }
  }

  // Sends the sign-in's window where the Space said it goes, or keeps the URL
  // for the view when the window is blocked or already closed.
  function deliverSignInUrl(url: string, popup: Window | null): void {
    if (popup && !popup.closed) {
      try {
        popup.location.href = url;
        return;
      } catch {
        // A window this page cannot steer is one the view has to offer.
      }
    }
    setSignInFallback(url);
  }

  // Starts a browser sign-in, opens the URL the Space answers with, and polls
  // until the Space says what became of it. The answer's URL is the Space's own
  // bind address, which sets the Space's cookie and redirects to the provider:
  // the browser opens it as it is.
  function startSignIn(input: {
    name: string;
    url: string;
    scope: "space" | "user";
  }): Promise<void> {
    if (libraryBusy()) return Promise.resolve();
    // Opened before the answer, while the click still counts as permission.
    const popup = openSignInWindow();
    setSignInFallback(null);
    // An existing server keeps the name and address the Space connected; the
    // sign-in attaches a credential to it. A new server enters the record when
    // the sign-in finishes. Either way the scope is the one the form named,
    // and the Space refuses a scope this caller may not use.
    const existing = integrations().find(
      (server) => server.name === input.name && server.url === input.url,
    );
    const args = existing
      ? { server: existing.server, scope: input.scope }
      : { name: input.name, url: input.url, scope: input.scope };
    beginLibraryWork();
    setLibraryError(null);
    return (async () => {
      try {
        const started = await commands.command<{ sign_in: string; url: string }>(
          "mcp_sign_in_start",
          args,
          SIGN_IN_START_TIMEOUT_MS,
        );
        deliverSignInUrl(started.url, popup);
        watchSignIn(started.sign_in, started.url, popup);
      } catch (reason) {
        closeSignInWindow(popup);
        setLibraryError(messageFor(reason));
      } finally {
        endLibraryWork();
      }
    })();
  }

  // Polls one sign-in while the Space holds it pending. The Space forgets a
  // sign-in after five minutes, so the page stops asking then.
  function watchSignIn(signIn: string, url: string, popup: Window | null): void {
    stopSignIn(signIn);
    const deadline = Date.now() + SIGN_IN_WINDOW_MS;
    const timer = window.setInterval(() => {
      // A window the reader closed before the Space settled the sign-in leaves
      // the URL reachable from the list, and polling goes on.
      if (popup?.closed && signInFallback() !== url) setSignInFallback(url);
      if (signInAsking.has(signIn)) return;
      signInAsking.add(signIn);
      void (async () => {
        try {
          const status = await commands.command<McpSignInState>(
            "mcp_sign_in_status",
            { sign_in: signIn },
            10_000,
          );
          if (status.state === "pending") {
            if (Date.now() >= deadline) {
              settleSignIn(signIn, url);
              setLibraryError(t("error.generic"));
            }
            return;
          }
          settleSignIn(signIn, url);
          // The Space's reason is one of its own fixed sentences, never the
          // provider's text.
          if (status.state === "failed") setLibraryError(status.reason ?? t("error.generic"));
          queueLibraryRefresh();
        } catch (reason) {
          settleSignIn(signIn, url);
          setLibraryError(messageFor(reason));
        } finally {
          signInAsking.delete(signIn);
        }
      })();
    }, SIGN_IN_POLL_MS);
    signInWatches.set(signIn, { timer, url, popup });
  }

  // Stops polling one sign-in, and drops its fallback link when it is the one
  // on the page.
  function settleSignIn(signIn: string, url: string): void {
    stopSignIn(signIn);
    if (signInFallback() === url) setSignInFallback(null);
  }

  function stopSignIn(signIn: string) {
    const watch = signInWatches.get(signIn);
    if (!watch) return;
    window.clearInterval(watch.timer);
    signInWatches.delete(signIn);
  }

  async function boot() {
    setOffline(false);
    setReady(false);
    try {
      const account = await api.me();
      await enter(account);
    } catch (reason) {
      if (reason instanceof ApiError && reason.status === 401) {
        // The Space is there and this browser is signed out.
        setReady(true);
        return;
      }
      // The installed client keeps its shell and lets the User retry; an
      // unreachable Space is not the same as a signed-out browser.
      setOffline(true);
      setReady(true);
    }
  }

  async function enter(account: Account) {
    update({ account });
    setReady(true);
    void startFollowing();
    await loadWorkspace();
    if (isMobile()) await restoreScreen();
  }

  // Puts the screen the phone's history entry names on the screen, which is how
  // a reload lands where the User was: on the Chat, the surface, and what was
  // open inside it rather than on the list behind them. The restore takes no
  // step of its own — the entry the browser kept is the one the User was on —
  // so the stack keeps its length and its Back returns to the screen before.
  async function restoreScreen() {
    const shown = screenOf(history.state);
    // An entry that names a Chat the Space no longer has keeps the rest of its
    // screen, so a Back never lands on a Chat that cannot open.
    if (shown.chat !== null && !state.chats.some((chat) => chat.id === shown.chat)) {
      history.replaceState({ ...shown, chat: null }, "");
    }
    await showScreen(shown);
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
      const query = new URLSearchParams(location.search);
      const requested = query.get("chat");
      const requestedApproval = query.get("approval");
      const requestedMessage = query.get("message");
      if (requested || requestedApproval || requestedMessage) {
        history.replaceState(history.state, "", `${location.pathname}${location.hash}`);
      }
      const active = state.activeChat
        ? chats.find((chat) => chat.id === state.activeChat?.id)
        : undefined;
      // A refresh reads the Chat that is open again, and a wide window opens the
      // newest Chat. What a phone reloads into is its history entry's screen,
      // which enter() restores once the workspace is read, and a link that names
      // a Chat is the screen the User asked for.
      const target =
        active ??
        (requested ? chats.find((chat) => chat.id === requested) : undefined) ??
        (!isMobile() ? chats[0] : undefined);
      // The Chat a link names is a step of its own the way choosing one is, so
      // the entry it landed on records it before it opens: the restore that
      // follows reads it there rather than closing the Chat the link opened.
      if (requested && target) {
        remember({ chat: target.id, view: "chats", section: null, bot: null });
      }
      if (target) await selectChat(target, requestedApproval, requestedMessage);
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

  async function selectChat(chat: Chat, focus?: string | null, message?: string | null) {
    // The Chat on screen keeps its view (and its apps' frames) until they close.
    handOffChat(chat.id);
    const generation = ++runGeneration;
    followed = null;
    setActivity(null);
    // The reader left the Chat that was open: no read of it is on its way, so
    // what the page kept live for it retires before the new Chat's read lands.
    if (state.activeChat) forgetLiveChat(state.activeChat.id);
    update({
      activeChat: chat,
      messages: [],
      files: {},
      approvals: [],
      activeBot: null,
      activeComputer: null,
      submitting: false,
      queue: null,
      deciding: null,
      error: null,
    });
    setFocus(focus ?? null);
    setFocusMessage(message ?? null);
    // Opening a Chat answers its new activity.
    setState((draft) => {
      draft.unread[chat.id] = false;
    });
    try {
      const [messages, files, approvals, bot] = await Promise.all([
        commands.command<Message[]>("chat_read", { chat: chat.id }),
        commands.command<FileSummary[]>("file_list", { chat: chat.id }),
        commands.command<Approval[]>("approval_list", { chat: chat.id }),
        commands.command<BotResult>("bot_show", { bot: chat.bot }),
      ]);
      if (generation !== runGeneration) return;
      // The answer about the Bot names its Chat too, and it is the newest word
      // about what that Chat does: the page takes it over the list's copy, so
      // the header, the badge and the composer all read the same projection.
      const answered: Chat = bot.chat ? { ...chat, ...bot.chat } : chat;
      const shown = withCards(chat.id, messages);
      update({
        activeChat: answered,
        messages: shown,
        approvals: mergeApprovals(approvals, state.approvals),
        activeBot: bot.bot,
        activeComputer: bot.computer,
      });
      mergeFiles(files);
      await restoreRun(answered, shown, generation);
    } catch (reason) {
      if (isCurrentChat(chat, generation)) update({ error: reason });
    }
  }

  async function restoreRun(chat: Chat, messages: Message[], generation: number) {
    // The Run the Chat works on, in the Space's words. The page never guesses
    // which Run is the newest: a Chat that waits follows the Run that waits,
    // even when a later Run already works.
    const named = chat.active_run;
    // A Chat the Space names no Run for keeps the state it was given: a failed
    // Chat stays failed. The Run the page reads there is history, and it only
    // fills that Chat's card. A Space that names no Run at all, an older one,
    // leaves the page to the newest request in the record.
    const run =
      named ??
      (named === null
        ? historyRun(messages, chat.status === "failed")
        : newestRequestRun(messages));
    if (!run || !isCurrentChat(chat, generation)) return;
    // Only a Run the Space named may write the Chat's state back.
    const holds = named !== null;
    // The page expects the Run it is about to read, so an answer about a Run
    // the Space no longer names is dropped: a Run the page only reads for its
    // history still fills its card, and a late answer about a Run the page
    // moved on from does not.
    expectRun(run);
    const sent = followVersion;
    try {
      const view = await commands.command<RunView>("run_show", { run }, 10_000);
      if (!isCurrentChat(chat, generation)) return;
      // The page moved on to another Run while the answer was on its way.
      if (followVersion !== sent) return;
      if (expectedRun !== run) return;
      if (followed && followed.run !== run) return;
      if (holds) applyRunView(view);
      else restoreActivity(view, view.status === "failed" ? (view.error ?? null) : null);
      // A Run that answered is over: the Chat reads the record again for the
      // reply and the Files it produced.
      if (!isUnfinished(view.status)) await refreshActive(chat, generation);
    } catch {
      // A Run that is already gone leaves the recorded Messages in place.
    }
  }

  async function reconcileRun(chat: Chat, messages: Message[], generation: number) {
    if (!isCurrentChat(chat, generation)) return;
    // The followed Run owns the Chat. A Chat that follows none takes the Run
    // the Space names for it, and a Chat the Space names no Run for keeps the
    // state it was given, so the page discovers nothing new in it: an older
    // Run's answer only fills its card.
    const named = chat.active_run;
    const run = followed?.run ?? (named === null ? null : (named ?? newestRequestRun(messages)));
    if (!run || settledRuns.has(run)) return;
    // The page expects the Run it is about to read.
    expectRun(run);
    const sent = followVersion;
    try {
      const view = await commands.command<RunView>("run_show", { run }, 10_000);
      if (!isCurrentChat(chat, generation)) return;
      // The page moved on to another Run while the answer was on its way.
      if (followVersion !== sent) return;
      if (expectedRun !== run) return;
      if (followed && followed.run !== run) return;
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
      const shown = withCards(chat.id, messages);
      // A refresh that answers with the same record must not remount its rows:
      // the rows keep their identity, and an open app's frame keeps its state.
      update({
        messages: preserveMessages(state.messages, shown),
        approvals: mergeApprovals(approvals, state.approvals),
      });
      mergeFiles(files);
      const last = shown.at(-1);
      if (last) setPreview(chat.id, previewOf(last));
      await reconcileRun(chat, shown, generation);
    } catch (reason) {
      if (isCurrentChat(chat, generation)) update({ error: reason });
    }
  }

  // Resolves false when nothing left the composer, so it keeps the Message.
  async function sendMessage(text: string, file: File | null): Promise<boolean> {
    const chat = state.activeChat;
    const bot = state.activeBot;
    if (!chat || !bot || state.submitting) return false;
    // The Chat already works or waits for an answer, so its composer is closed.
    // The card sends through this same boundary, so it is refused the same way:
    // a Card must not be able to enqueue a second Run while the Bot is busy.
    if (busyChat()) return false;
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
      setState((draft) => {
        draft.files[saved.id] = saved;
      });
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
      // The Space accepted the Message into its Bot's queue. The Chat now works
      // on it and takes no second Message until the Run is done.
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
      update({
        error:
          reason instanceof ApiError && reason.status === 413 ? tooLarge() : uploadFailure(reason),
      });
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
      await commands.command<unknown>("provider_add", {
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

  // Creates a Bot, which answers with the Bot, its Computer, and the Chat the
  // Space opened between them, so the new Bot's Chat is the one that opens. The
  // create is the one step that cannot be taken twice: once the Space has
  // answered it the Bot is there, so everything after only reads, and a read
  // that fails is never a reason to make another one. It answers what the Space
  // said when it made no Bot, so the form that asked can say it in its own
  // words, and nothing when the Bot is there.
  async function createBot(name: string, computer?: string): Promise<string | null> {
    // A Space with no Provider has no Model for a Bot to work with, so the
    // client makes none.
    if (state.providers.length === 0) return t("bots.new.noProvider");
    update({ busy: true, error: null });
    let created: BotCreateResult;
    try {
      created = await commands.command<BotCreateResult>("bot_create", {
        name,
        ...(computer === undefined ? {} : { computer }),
      });
    } catch (reason) {
      // The Space never confirmed the Bot, so the form may ask again.
      update({ busy: false, error: reason });
      return messageFor(reason);
    }
    // The answer names the Chat by identity: the Space had not projected it
    // yet, so all the page holds of it is its name, and the Bot's own answer —
    // read when the Chat opens — and the events the create appended are what
    // project it.
    const opened = created.chat as Chat;
    // The Bot is there from here on, and the create's own answer carries it and
    // its Chat, so the page reads them from the answer rather than from the
    // lists: a list that cannot be read leaves the Bot on the screen.
    update({
      bots: [...state.bots, created.bot].sort((left, right) => left.id.localeCompare(right.id)),
      chats: [...state.chats, opened].sort((left, right) => left.id.localeCompare(right.id)),
      busy: false,
    });
    await loadPreviews([opened]);
    // The Chat the Space opened with the Bot is the one that opens, the way the
    // first run's own create leaves the User in it, from wherever the create was
    // asked for.
    setView("chats");
    // Opening that Chat is a step of its own the way choosing it is, so the
    // phone's history records the screen it lands on: a reload after a create
    // stays in the new Bot's Chat, and Back returns to the list it was made
    // from rather than to an entry that still names that list.
    remember({ chat: opened.id, view: "chats", section: null, bot: null });
    await selectChat(opened);
    return null;
  }

  function backToList() {
    // Leaving to the list is a switch too: the Chat's apps close before its view
    // leaves, exactly as when another Chat takes the screen.
    handOffChat(null);
    runGeneration += 1;
    followed = null;
    setActivity(null);
    if (state.activeChat) forgetLiveChat(state.activeChat.id);
    update({
      activeChat: null,
      messages: [],
      files: {},
      approvals: [],
      activeBot: null,
      activeComputer: null,
      submitting: false,
      queue: null,
      deciding: null,
      error: null,
    });
  }

  // The screen the User is on, as one history entry.
  function screen(): Screen {
    return {
      chat: state.activeChat?.id ?? null,
      view: view(),
      section: sectionOpen() ? section() : null,
      bot: pageBotId(),
    };
  }

  // The screen one history entry names. An entry the client before this one
  // wrote names only its Chat, so it is read the same way: that Chat, on the
  // list of Chats. An entry this client did not write at all reads as the list
  // of Chats rather than as nothing.
  function screenOf(written: unknown): Screen {
    const entry = (written ?? {}) as Partial<Screen> & { nehaChat?: unknown };
    const named = typeof entry.chat === "string" ? entry.chat : entry.nehaChat;
    return {
      chat: typeof named === "string" ? named : null,
      view:
        entry.view === "bots" ||
        entry.view === "settings" ||
        entry.view === "integrations" ||
        entry.view === "skills"
          ? entry.view
          : "chats",
      section: typeof entry.section === "string" ? (entry.section as SettingsSection) : null,
      bot: typeof entry.bot === "string" ? entry.bot : null,
    };
  }

  // Whether two entries name the same screen.
  function sameScreen(left: Screen, right: Screen): boolean {
    return (
      left.chat === right.chat &&
      left.view === right.view &&
      left.section === right.section &&
      left.bot === right.bot
    );
  }

  // Whether the phone's history holds the step the screen is on, so its system
  // Back can take the step the page's own Back would. A window narrowed after a
  // surface opened leaves no step for it, and the trail does the work instead.
  function onPhoneStep(): boolean {
    const entry = screenOf(history.state);
    return (entry.view === "bots" || entry.view === "settings") && sameScreen(entry, screen());
  }

  // Gives the phone's history one entry for the screen. A step the User takes
  // writes one; restoring a screen from an entry writes none, because the entry
  // the browser kept is already the one on the stack. Moving between Chats
  // replaces the entry rather than stacking one per Chat, because the Chats are
  // one surface. A screen the entry already names writes nothing either.
  function remember(next: Screen, force = false) {
    if (!force && !isMobile()) return;
    const entry = screenOf(history.state);
    if (sameScreen(entry, next)) return;
    const swapping = entry.view === "chats" && entry.chat !== null && next.view === "chats";
    if (swapping) history.replaceState(next, "");
    else history.pushState(next, "");
  }

  // Puts the screen a history entry names on the screen. It takes the steps the
  // User's own Back and Forward would, so a phone's system Back lands on the
  // screen it left rather than on an empty shell.
  async function showScreen(shown: Screen) {
    if (shown.bot === null) {
      if (pageBotId() !== null) {
        pageGen += 1;
        setPageBotId(null);
        update({
          pageBot: null,
          pageComputer: null,
          rules: [],
          memories: emptyMemories(),
          manageError: null,
        });
      }
    } else if (shown.bot !== pageBotId()) {
      const bot = state.bots.find((known) => known.id === shown.bot);
      if (bot) await selectPageBot(bot);
    }
    // A library view reads the list it shows, the way selecting it in the rail
    // does; every other surface is just the view it names.
    if (shown.view === "integrations" || shown.view === "skills") showView(shown.view);
    else setView(shown.view);
    // The Settings surface reads what it connects from, so a screen that lands on
    // it reads it again, the way opening the surface does.
    if (shown.view === "settings") void loadSettings();
    // The Bots surface is where a Bot is made, so a screen that lands on it
    // reads the Computers a new one may work on, the way opening it does.
    if (shown.view === "bots") void refreshComputers();
    setSectionOpen(shown.section !== null);
    if (shown.section !== null) setSection(shown.section);
    if (shown.chat === null) {
      if (state.activeChat !== null) backToList();
    } else if (state.activeChat?.id !== shown.chat) {
      const chat = state.chats.find((known) => known.id === shown.chat);
      if (chat) await selectChat(chat);
    }
  }

  function closeChat() {
    if (isMobile() && screenOf(history.state).chat !== null) {
      history.back();
      return;
    }
    backToList();
  }

  // Reads the Bot list again, so a rename or a move shows in the sidebar.
  async function refreshBots() {
    try {
      const bots = await commands.command<Bot[]>("bot_list");
      update({ bots: [...bots].sort((left, right) => left.id.localeCompare(right.id)) });
    } catch {
      // A failed read keeps the list the page already shows.
    }
  }

  // Opens one Bot's page: the Bot, its Computer, the Computers it may move to,
  // its Models, and its rules.
  async function selectPageBot(bot: Bot) {
    // A Bot's page is a step deeper than the surface it was opened from, so Back
    // returns to that surface.
    pushTrail(view());
    pageGen += 1;
    setPageBotId(bot.id);
    update({
      pageBot: null,
      pageComputer: null,
      rules: [],
      memories: emptyMemories(),
      manageError: null,
      managing: true,
    });
    await reloadPageBot(bot.id);
  }

  // Applies one memory answer when it is newer than the one the page shows. A
  // read that lost the race answers about an older record, so its answer is
  // dropped and the newer one stays.
  function showMemories(read: number, memories: MemoryList) {
    if (read <= memoryShown) return;
    memoryShown = read;
    update({ memories });
  }

  // Everything one Bot's page shows, read together so the Bot, its Computer,
  // the Computers it may move to, its Models, its rules, and its memory are one
  // answer rather than reads that can disagree with each other. The memory
  // answer takes its place in the order memory answers land in, so an event's
  // read that leaves later can land first without being overwritten.
  async function readBotPage(id: string) {
    const memoryRead = ++memoryReads;
    const [shown, computers, models, rules, memories] = await Promise.all([
      commands.command<BotResult>("bot_show", { bot: id }),
      commands.command<Computer[]>("computer_list"),
      commands.command<Model[]>("model_list"),
      commands.command<Rule[]>("rule_list", { bot: id }),
      commands.command<MemoryList>("memory_list", { bot: id }),
    ]);
    return {
      page: {
        pageBot: shown.bot,
        pageComputer: shown.computer,
        computers,
        models,
        rules,
      },
      memoryRead,
      memories,
    };
  }

  // Reads the Bot page again from the Space. A read that lands after the User
  // opened another Bot is dropped.
  async function reloadPageBot(id: string) {
    const gen = pageGen;
    update({ managing: true });
    try {
      const { page, memoryRead, memories } = await readBotPage(id);
      if (gen !== pageGen) return;
      update({ ...page, managing: false });
      showMemories(memoryRead, memories);
      // A memory change that landed while this read was on its way is newer
      // than its answer, so the page reads memory once more.
      if (memoryStale) void refreshMemories();
    } catch (reason) {
      if (gen !== pageGen) return;
      update({ manageError: reason, managing: false });
    }
  }

  // Sends one Bot change and reads the Bot again, so the page shows what the
  // Space holds. It answers whether the Space accepted the change.
  async function updateBot(patch: Record<string, unknown>): Promise<boolean> {
    const id = state.pageBot?.id;
    if (!id) return false;
    const gen = pageGen;
    update({ managing: true, manageError: null });
    try {
      await commands.command("bot_update", { bot: id, ...patch });
      // A write that lands after the User opened another Bot is dropped: the
      // page it would write belongs to a Bot the User is no longer reading. The
      // answer is checked again before the page is written, because the User
      // may have moved on while the second read was on its way.
      if (gen !== pageGen) return false;
      const { page, memoryRead, memories } = await readBotPage(id);
      if (gen !== pageGen) return false;
      update({ ...page, managing: false });
      showMemories(memoryRead, memories);
      if (memoryStale) void refreshMemories();
      await refreshBots();
      return true;
    } catch (reason) {
      if (gen !== pageGen) return false;
      update({ manageError: reason, managing: false });
      return false;
    }
  }

  async function renameBot(name: string) {
    await updateBot({ name });
  }

  // Sends only what changed: a Bot without its own Model or level follows the
  // Space's default, and an absent field leaves the Bot's own choice alone.
  // Going back to the default is a field of its own, because an absent Model or
  // level leaves the choice alone.
  async function saveBotModel(model: string | null, thinking: ThinkingLevel | null) {
    const patch: Record<string, unknown> = {};
    if (model !== (state.pageBot?.model ?? null)) {
      if (model === null) patch.reset_model = true;
      else patch.model = model;
    }
    if (thinking !== (state.pageBot?.thinking ?? null)) {
      if (thinking === null) patch.reset_thinking = true;
      else patch.thinking = thinking;
    }
    if (Object.keys(patch).length === 0) return;
    await updateBot(patch);
  }

  async function grantFolder(path: string) {
    await updateBot({ grant_add: path });
  }

  async function dropGrant(path: string) {
    await updateBot({ grant_remove: path });
  }

  // Widening a Bot's access carries the owner's answer at the current consent
  // version; an older client cannot turn full access on without it.
  async function setFullAccess(on: boolean) {
    const consent = on
      ? { full_access: true, full_access_consent: FULL_ACCESS_CONSENT }
      : { full_access: false };
    await updateBot(consent);
  }

  async function moveBotTo(computer: string) {
    const id = state.pageBot?.id;
    if (!id) return;
    const gen = pageGen;
    update({ managing: true, manageError: null });
    try {
      await commands.command("bot_move", { bot: id, computer });
      if (gen !== pageGen) return;
      const { page, memoryRead, memories } = await readBotPage(id);
      if (gen !== pageGen) return;
      update({ ...page, managing: false });
      showMemories(memoryRead, memories);
      if (memoryStale) void refreshMemories();
      await refreshBots();
    } catch (reason) {
      if (gen !== pageGen) return;
      update({ manageError: reason, managing: false });
    }
  }

  async function resetWorkspace() {
    // The Bot on screen names its Computer, which is the one the reset clears.
    // Reading it from the page's own Bot keeps the reset on the Computer the
    // User asked about, whichever Bot they have opened since.
    const computer = state.pageBot?.computer;
    if (!computer) return;
    update({ managing: true, manageError: null });
    try {
      await commands.command("computer_reset", { computer });
      await refreshComputers();
      update({ managing: false });
    } catch (reason) {
      update({ manageError: reason, managing: false });
    }
  }

  // Reads the Computers this Space can use, which the Settings surface lists and
  // a Bot page moves between.
  async function refreshComputers() {
    try {
      const computers = await commands.command<Computer[]>("computer_list");
      update({ computers });
    } catch {
      // A failed read keeps the Computers the page already shows.
    }
  }

  // Clears a Hosted Computer's Workspace from the Computers list, which is
  // where an Admin finds one before they know which Bot works on it.
  async function resetComputer(computer: string): Promise<boolean> {
    update({ managing: true, manageError: null });
    try {
      await commands.command("computer_reset", { computer });
      await refreshComputers();
      update({ managing: false });
      return true;
    } catch (reason) {
      update({ manageError: reason, managing: false });
      return false;
    }
  }

  // Creates a Hosted Computer from the Computers list. Its owner names it and
  // chooses whether it takes any of their Bots or a single one.
  async function createComputer(input: {
    name: string;
    sharing: Computer["sharing"];
  }): Promise<boolean> {
    update({ managing: true, manageError: null });
    try {
      await commands.command<Computer>("computer_create", input);
      await refreshComputers();
      update({ managing: false });
      return true;
    } catch (reason) {
      update({ manageError: reason, managing: false });
      return false;
    }
  }

  // Makes an exclusive Hosted Computer shared, so another of its owner's Bots
  // may use it.
  async function shareComputer(computer: string): Promise<boolean> {
    update({ managing: true, manageError: null });
    try {
      await commands.command<Computer>("computer_update", {
        computer,
        sharing: "shared",
      });
      await refreshComputers();
      update({ managing: false });
      return true;
    } catch (reason) {
      update({ manageError: reason, managing: false });
      return false;
    }
  }

  // Opens one Bot's page from a Computer in the Settings surface, leaving the
  // Settings where Back returns to.
  function openBotPage(bot: Bot) {
    // The page is a step deeper than the Settings it was opened from, so the
    // phone's history gets an entry for it.
    remember({ chat: state.activeChat?.id ?? null, view: "bots", section: null, bot: bot.id });
    void selectPageBot(bot);
    setView("bots");
  }

  // Reads one Bot's rules again, so a change shows without a page reload. A
  // read that lands after the User opened another Bot is dropped.
  async function refreshRules() {
    const id = state.pageBot?.id;
    if (!id) return;
    const gen = pageGen;
    try {
      const rules = await commands.command<Rule[]>("rule_list", { bot: id });
      if (gen !== pageGen) return;
      update({ rules });
    } catch {
      // A failed read keeps the rules the page already shows.
    }
  }

  // Sends one rule change and reads the Bot's rules again, so a change shows
  // without a page reload. A write that lands after the User opened another Bot
  // is dropped: the rules it would write belong to a Bot the User is not
  // reading, and they may have moved on while the second read was on its way.
  async function writeRule(
    id: string,
    name: "rule_add" | "rule_remove",
    args: Record<string, unknown>,
  ) {
    const gen = pageGen;
    update({ managing: true, manageError: null });
    try {
      await commands.command(name, args);
      if (gen !== pageGen) return;
      const rules = await commands.command<Rule[]>("rule_list", { bot: id });
      if (gen !== pageGen) return;
      update({ rules, managing: false });
    } catch (reason) {
      if (gen !== pageGen) return;
      update({ manageError: reason, managing: false });
    }
  }

  async function addRule(rule: {
    effect: Rule["effect"];
    tool: string;
    command_prefix: string | null;
    path_prefix: string | null;
  }) {
    const id = state.pageBot?.id;
    if (!id) return;
    await writeRule(id, "rule_add", { bot: id, ...rule });
  }

  async function removeRule(rule: string) {
    const id = state.pageBot?.id;
    if (!id) return;
    await writeRule(id, "rule_remove", { rule });
  }

  // Reads one Bot's memory again, so a change shows without a page reload. It
  // targets the Bot the page names even before the page has its first answer,
  // and keeps reading while changes land: the page applies only the newest
  // answer, whichever read lands last. A read that lands after the User opened
  // another Bot is dropped.
  async function refreshMemories() {
    const id = pageBotId();
    if (!id) return;
    if (memoryReading) {
      // The read on its way may predate this change, so it reads once more when
      // it lands rather than letting a second read race it.
      memoryStale = true;
      return;
    }
    memoryReading = true;
    const gen = pageGen;
    try {
      do {
        // This read starts after every change seen so far, so it answers about
        // all of them.
        memoryStale = false;
        const read = ++memoryReads;
        try {
          const memories = await commands.command<MemoryList>("memory_list", { bot: id });
          if (gen !== pageGen) return;
          showMemories(read, memories);
        } catch {
          // A failed read keeps the memory the page already shows.
        }
      } while (memoryStale && gen === pageGen);
    } finally {
      memoryReading = false;
      // A change waited for this read while the page moved on, so the page the
      // change belongs to reads now rather than waiting for another event.
      if (memoryStale) void refreshMemories();
    }
  }

  // Clears one memory entry and reads the Bot's memory again, so the list and
  // the room each kind has left follow. A write that lands after the User
  // opened another Bot is dropped: the entry it would clear belongs to a Bot
  // the User is no longer reading.
  async function removeMemory(memory: string) {
    const id = state.pageBot?.id;
    if (!id) return;
    const gen = pageGen;
    update({ managing: true, manageError: null });
    try {
      await commands.command("memory_remove", { bot: id, memory });
      if (gen !== pageGen) return;
      // The removal's read takes its place behind any read already on its way,
      // so an older answer cannot bring the entry back.
      const read = ++memoryReads;
      const memories = await commands.command<MemoryList>("memory_list", { bot: id });
      if (gen !== pageGen) return;
      showMemories(read, memories);
      update({ managing: false });
    } catch (reason) {
      if (gen !== pageGen) return;
      update({ manageError: reason, managing: false });
    }
  }

  // Reads the catalog's vendors and its cache state, which is what the Settings
  // surface connects from and shows.
  async function loadSettings() {
    update({ managing: true, manageError: null });
    try {
      const [vendors, catalog, computers] = await Promise.all([
        commands.command<CatalogVendor[]>("catalog_list"),
        commands.command<CatalogStatus>("catalog_status"),
        commands.command<Computer[]>("computer_list"),
      ]);
      update({ vendors, catalog, computers, managing: false });
    } catch (reason) {
      update({ manageError: reason, managing: false });
    }
  }

  // Reads the Providers and their Models again after a change to either.
  async function refreshProviders() {
    try {
      const [providers, models] = await Promise.all([
        commands.command<Provider[]>("provider_list"),
        commands.command<Model[]>("model_list"),
      ]);
      update({ providers, models });
    } catch {
      // A failed read keeps the lists the page already shows.
    }
  }

  async function connectVendor(input: {
    vendor: string;
    apiKey: string;
    baseUrl: string | null;
    protocol: string | null;
    thinking: ThinkingLevel | null;
  }): Promise<boolean> {
    update({ managing: true, manageError: null });
    try {
      const args: Record<string, unknown> = { vendor: input.vendor, api_key: input.apiKey };
      if (input.baseUrl) args.base_url = input.baseUrl;
      if (input.protocol) args.protocol = input.protocol;
      if (input.thinking) args.thinking = input.thinking;
      await commands.command<unknown>("provider_connect", args);
      await refreshProviders();
      update({ managing: false });
      return true;
    } catch (reason) {
      update({ manageError: reason, managing: false });
      return false;
    }
  }

  async function refreshProviderModels(provider: string): Promise<boolean> {
    update({ managing: true, manageError: null });
    try {
      await commands.command("provider_refresh", { provider });
      await refreshProviders();
      update({ managing: false });
      return true;
    } catch (reason) {
      update({ manageError: reason, managing: false });
      return false;
    }
  }

  async function makeDefaultModel(model: string): Promise<boolean> {
    update({ managing: true, manageError: null });
    try {
      await commands.command("model_default", { model });
      await refreshProviders();
      update({ managing: false });
      return true;
    } catch (reason) {
      update({ manageError: reason, managing: false });
      return false;
    }
  }

  async function refreshCatalog(): Promise<boolean> {
    update({ managing: true, manageError: null });
    try {
      const answer = await commands.command<CatalogRefreshResult>("catalog_refresh");
      update({ catalog: answer.status, managing: false });
      return true;
    } catch (reason) {
      update({ manageError: reason, managing: false });
      return false;
    }
  }

  // Opens a management surface, reading what it shows before it appears. The
  // surface the User is leaving goes on the trail, so Back returns to it: a
  // narrow viewport shows one surface at a time.
  function openManage(next: RailView) {
    // The surface the User is leaving is the one Back returns to.
    if (view() === "bots" || view() === "settings") pushTrail(view());
    // Opening the Settings surface again starts from its list of sections, so
    // every section stays reachable on a viewport that shows one surface.
    if (next === "settings") setSectionOpen(false);
    setView(next);
    update({ manageError: null });
    // The phone's history gets the surface, so its system Back leaves it the way
    // the page's own Back does.
    remember({ chat: state.activeChat?.id ?? null, view: next, section: null, bot: null });
    if (next === "settings") void loadSettings();
    // The Bots surface is where a Bot is made, so it reads the Computers a new
    // one may work on.
    if (next === "bots") void refreshComputers();
  }

  // Leaves a management surface for the one before it, with no Bot selected.
  // The Chats are where the trail runs out.
  function closeManage() {
    // A phone's system Back leaves a management surface the same way the page's
    // own Back does, so the page's Back asks the history for the step back rather
    // than taking one of its own: the two must land on the same screen.
    if (isMobile() && onPhoneStep()) {
      history.back();
      return;
    }
    // A Settings section is a step inside its surface, not a surface of its own:
    // Back leaves the section for the list of sections first, so a viewport that
    // shows one surface at a time still reaches every section.
    if (view() === "settings" && sectionOpen()) {
      setSectionOpen(false);
      return;
    }
    // The page is closing, so a read it asked for belongs to a page that is gone:
    // the count moves on, and the answer is dropped when it lands.
    pageGen += 1;
    const trail = manageTrail();
    setManageTrail(trail.slice(0, -1));
    const back = trail[trail.length - 1] ?? "chats";
    setView(back);
    // Coming back to the Settings surface keeps the section that was open, so a
    // Bot opened from a section returns to that section. Leaving it forgets the
    // section, so the surface opens on its list next time.
    if (back !== "settings") setSectionOpen(false);
    setPageBotId(null);
    update({ pageBot: null, pageComputer: null, manageError: null });
  }

  const errorText = () => (state.error ? messageFor(state.error) : null);
  // What a failed management command says, in the User's words.
  const manageErrorText = () => (state.manageError ? messageFor(state.manageError) : null);
  const needsSetup = () => state.providers.length === 0 || state.chats.length === 0;
  // A Space with no Provider has no Model to work with, and the client creates
  // no Bot before one exists.
  const canCreateBot = () => state.providers.length > 0;
  const showChat = () => state.activeChat !== null && !needsSetup() && state.bots.length > 0;
  // A management surface fills the main area once it has something to show, so
  // a narrow viewport shows one surface at a time with a Back action.
  const showManage = () => view() === "bots" || view() === "settings";
  const showPage = () =>
    (view() === "settings" && sectionOpen()) || (view() === "bots" && state.pageBot !== null);
  // The main area holds a detail rather than a list: on a narrow viewport that is
  // the one surface on screen and the sidebar is the one hidden. Which of the two
  // is on screen follows the rail view, not what is open behind it: a Chat left
  // open must not hide the list of the surface the User just opened.
  const showDetail = () => (showManage() ? showPage() : showChat());
  // The setup view fills the main area too, so the sidebar is the one hidden —
  // unless a management surface is open, where its list is how a Space with no
  // Provider yet gets one.
  const showSetup = () => needsSetup() && !showManage();
  // The two library views the rail opens, and the account they render with.
  const libraryView = () => view() === "integrations" || view() === "skills";
  const viewAccount = () => ({
    id: state.account?.id ?? "",
    displayName: state.account?.display_name ?? "",
    isAdmin: state.account?.is_admin === true,
  });
  // The shell's classes: the detail and setup surfaces main's layout gives the
  // main area, plus the library shell a narrow viewport fills with the view.
  const shellClass = () => {
    const library = libraryView();
    const detail = showDetail() ? " app--detail" : "";
    const setup = showSetup() && !library ? " app--setup" : "";
    return `app${detail}${setup}${library ? " app--library" : ""}`;
  };

  // What each Chat is doing, for the list and the attention section.
  const chatStates = createMemo<Record<string, ChatStatus>>(() => {
    const states: Record<string, ChatStatus> = {};
    for (const chat of state.chats) {
      // The Chat on screen carries the Bot's own word about it, which is newer
      // than the list's, so its mark reads the same as its header.
      const shown = state.activeChat?.id === chat.id ? state.activeChat : chat;
      states[chat.id] = statusOf(shown);
    }
    return states;
  });

  // Every approval that waits for the User, one entry each. Opening its Chat
  // does not answer it, so a Chat the User is reading stays listed until its
  // answer is sent.
  const attentionEntries = createMemo<AttentionEntry[]>(() =>
    Object.values(state.pending)
      .sort((left, right) => left.approval.localeCompare(right.approval))
      .flatMap((approval) => {
        const chat = state.chats.find((known) => known.id === approval.chat);
        if (!chat) return [];
        return [
          {
            approval,
            chat,
            botName: state.bots.find((bot) => bot.id === chat.bot)?.name ?? "Bot",
            detail: `${approval.tool} · ${approval.summary}`,
          },
        ];
      }),
  );

  // Opens the Chat an approval waits in and shows its card.
  function openAttention(entry: AttentionEntry) {
    setView("chats");
    if (state.activeChat?.id === entry.chat.id) {
      setFocus(entry.approval.approval);
      return;
    }
    // Opening a Chat from an approval is a step of its own, the way choosing one
    // from the list is.
    remember({ chat: entry.chat.id, view: "chats", section: null, bot: null });
    void selectChat(entry.chat, entry.approval.approval);
  }

  // What the Chat on screen is doing, from the merged projection of its Runs.
  // Every mark the Chat shows, and its composer, read this one word: a Run the
  // page does not follow still shows in the Chat.
  const chatStatus = (): ChatStatus => (state.activeChat ? statusOf(state.activeChat) : "idle");
  const holdsChat = (status: ChatStatus) => status === "working" || status === "waiting";

  // What the activity card shows: the followed Run's activity while it works,
  // and one line with the reason once it failed. A Run waiting in the queue has
  // done nothing yet, and a Run waiting for an approval shows its card instead.
  // The step the followed Run holds, as the projection of its Chat's Runs has
  // it: the record gave it that word, so it is what the page renders from.
  const followedStep = () => {
    const run = followed?.run;
    if (!run) return null;
    const chat = state.activeChat?.id ?? runChats.get(run);
    return (chat !== undefined ? state.runs[chat]?.active[run] : undefined) ?? null;
  };
  const shownActivity = () => {
    // Read first, so the render follows the Run's activity whichever way the
    // Run it follows is holding.
    const shown = activity();
    if (!followed) return chatStatus() === "failed" ? shown : null;
    return followedStep() === "running" ? shown : null;
  };

  // The Chat on screen works or waits for an approval, so its composer takes no
  // second Message until the Run is done.
  const busyChat = () => holdsChat(chatStatus()) || state.queue !== null;

  return (
    <Show when={ready()} fallback={<Boot />}>
      <Show when={!offline()} fallback={<Offline onRetry={() => void boot()} />}>
        <Show
          when={state.account}
          fallback={
            <AuthView
              mode={authMode()}
              secret={authSecret()}
              onAuthed={(account) => void enter(account)}
            />
          }
        >
          {(account) => (
            <div class={shellClass()}>
              <Rail
                account={account()}
                view={view()}
                attentionCount={attentionEntries().length}
                onSelect={(next) => {
                  if (next === "integrations" || next === "skills") selectView(next);
                  else if (next === "bots" || next === "settings") openManage(next);
                  else setView(next);
                }}
              />
              <Sidebar
                account={account()}
                chats={state.chats}
                bots={state.bots}
                activeChat={state.activeChat}
                previews={state.previews}
                statuses={chatStates()}
                unread={state.unread}
                attention={attentionEntries()}
                attentionOpen={view() === "attention"}
                view={view()}
                activeBot={pageBotId()}
                section={section()}
                onSelect={(chat) => {
                  setView("chats");
                  // Choosing a Chat is a step of its own: the phone's history gets
                  // an entry, so its system Back closes the Chat the way the
                  // page's own Back does.
                  remember({ chat: chat.id, view: "chats", section: null, bot: null });
                  void selectChat(chat);
                }}
                onAttention={(entry) => openAttention(entry)}
                onView={(next) => selectView(next)}
                onBot={(bot) => {
                  // A Bot's page is a step deeper than the list it was opened from,
                  // so the phone's history gets an entry for it.
                  remember({
                    chat: state.activeChat?.id ?? null,
                    view: "bots",
                    section: null,
                    bot: bot.id,
                  });
                  void selectPageBot(bot);
                }}
                onSection={(next) => {
                  setSection(next);
                  setSectionOpen(true);
                  // A section is a step inside its surface, so the phone's history
                  // gets one for it: its system Back leaves the section for the
                  // list of sections, the way the page's own Back does.
                  remember({
                    chat: state.activeChat?.id ?? null,
                    view: "settings",
                    section: next,
                    bot: null,
                  });
                }}
                onCreateBot={(name, computer) => createBot(name, computer)}
                computers={state.computers}
                canCreateBot={canCreateBot()}
              />
              <main class="main">
                {/* A narrow viewport gives the library view the whole screen and
                    the sidebar gives way, so the view carries its own way back. */}
                <Show when={libraryView() && isMobile()}>
                  <div class="library__back">
                    <button
                      class="btn btn--quiet"
                      type="button"
                      aria-label={t("chat.back")}
                      onClick={() => selectView("chats")}
                    >
                      <Icon name="chevron-left" size="sm" />
                      {t("chat.back")}
                    </button>
                  </div>
                </Show>
                <Show when={view() === "integrations"}>
                  <IntegrationsView
                    account={viewAccount()}
                    bots={state.bots}
                    servers={integrations()}
                    packages={packages()}
                    busy={libraryBusy()}
                    error={libraryError()}
                    signInUrl={signInFallback()}
                    onScopeChange={() => setLibraryError(null)}
                    onAdd={addIntegration}
                    onConnect={connectIntegration}
                    onSignIn={startSignIn}
                    onRemove={removeIntegration}
                    onBot={setBotIntegration}
                    onPackageEnabled={setPackageEnabled}
                  />
                </Show>
                <Show when={view() === "skills"}>
                  <SkillsView
                    account={viewAccount()}
                    bots={state.bots}
                    skills={skills()}
                    packages={packages()}
                    busy={libraryBusy()}
                    error={libraryError()}
                    onAdd={addSkill}
                    onRemove={removeSkill}
                    onBot={setBotSkill}
                  />
                </Show>
                <Show when={showManage()}>
                  <Show when={view() === "bots"}>
                    <Show
                      when={state.pageBot}
                      fallback={
                        <div class="select-chat">
                          <h2>{t("bots.select.title")}</h2>
                        </div>
                      }
                    >
                      {(bot) => (
                        <BotPage
                          bot={bot()}
                          computer={state.pageComputer}
                          computers={state.computers}
                          models={state.models}
                          rules={state.rules}
                          memories={state.memories}
                          busy={state.managing}
                          error={manageErrorText()}
                          onBack={closeManage}
                          onRename={(name) => renameBot(name)}
                          onModel={(model, thinking) => saveBotModel(model, thinking)}
                          onGrantAdd={(path) => grantFolder(path)}
                          onGrantRemove={(path) => dropGrant(path)}
                          onFullAccess={(on) => setFullAccess(on)}
                          onMove={(computer) => moveBotTo(computer)}
                          onReset={() => resetWorkspace()}
                          onRuleAdd={(rule) => addRule(rule)}
                          onRuleRemove={(rule) => removeRule(rule)}
                          onMemoryRemove={(memory) => removeMemory(memory)}
                        />
                      )}
                    </Show>
                  </Show>
                  <Show when={view() === "settings"}>
                    <SettingsPage
                      account={account()}
                      providers={state.providers}
                      models={state.models}
                      computers={state.computers}
                      bots={state.bots}
                      vendors={state.vendors}
                      catalog={state.catalog}
                      section={section()}
                      busy={state.managing}
                      error={manageErrorText()}
                      onBack={closeManage}
                      onCreate={(input) => createComputer(input)}
                      onShare={(computer) => shareComputer(computer)}
                      onReset={(computer) => resetComputer(computer)}
                      onOpenBot={(bot) => openBotPage(bot)}
                      onConnect={(input) => connectVendor(input)}
                      onRefresh={(provider) => refreshProviderModels(provider)}
                      onDefault={(model) => makeDefaultModel(model)}
                      onCatalog={() => refreshCatalog()}
                    />
                  </Show>
                </Show>
                <Show when={!showManage() && !libraryView()}>
                  <For each={panes()}>
                    {(pane) => {
                      // The values a view renders while it is leaving. The page's
                      // own fields have already moved to the Chat on screen, so
                      // the leaving view must read only what it showed before.
                      const frozen = () => {
                        const leaving = leavingChat();
                        return leaving !== null && leaving.id === pane.id ? leaving.frozen : null;
                      };
                      const retiring = () => frozen() !== null;
                      // One value the view renders: the page's live state while
                      // the Chat is on screen, the frozen copy once it leaves.
                      const shown = <T,>(
                        live: () => T,
                        fromFrozen: (values: FrozenChat) => T,
                      ): (() => T) => {
                        return () => {
                          const values = frozen();
                          return values === null ? live() : fromFrozen(values);
                        };
                      };
                      const bot = shown(
                        () => state.activeBot,
                        (values) => values.bot,
                      );
                      const computer = shown(
                        () => state.activeComputer,
                        (values) => values.computer,
                      );
                      const messages = shown(
                        () => state.messages,
                        (values) => values.messages,
                      );
                      const files = shown(
                        () => state.files,
                        (values) => values.files,
                      );
                      const approvals = shown(
                        () => state.approvals,
                        (values) => values.approvals,
                      );
                      const submitting = shown(
                        () => state.submitting,
                        (values) => values.submitting,
                      );
                      const status = shown(
                        () => chatStatus(),
                        (values) => values.status,
                      );
                      const queue = shown(
                        () => state.queue,
                        (values) => values.queue,
                      );
                      const busy = shown(
                        () => busyChat(),
                        (values) => values.busy,
                      );
                      const activity = shown(
                        () => shownActivity(),
                        (values) => values.activity,
                      );
                      const deciding = shown(
                        () => state.deciding,
                        (values) => values.deciding,
                      );
                      const error = shown(
                        () => errorText(),
                        (values) => values.error,
                      );
                      const paneFocus = shown(
                        () => focus(),
                        () => null,
                      );
                      const paneMessage = shown(
                        () => focusMessage(),
                        () => null,
                      );
                      return (
                        <div
                          class={retiring() ? "chat-pane chat-pane--retiring" : "chat-pane"}
                          aria-hidden={retiring() ? "true" : undefined}
                        >
                          <ChatView
                            account={account()}
                            bot={bot()}
                            computer={computer()}
                            messages={messages()}
                            files={files()}
                            approvals={approvals()}
                            submitting={submitting()}
                            status={status()}
                            queue={queue()}
                            busy={busy()}
                            activity={activity()}
                            focus={paneFocus()}
                            focusMessage={paneMessage()}
                            onFocused={() => {
                              if (retiring()) return;
                              setFocus(null);
                              setFocusMessage(null);
                            }}
                            deciding={deciding()}
                            error={error()}
                            onSend={(text, file) =>
                              retiring() ? Promise.resolve(false) : sendMessage(text, file)
                            }
                            onDecide={(approval, allow, lasting) => {
                              if (!retiring()) decideApproval(approval, allow, lasting);
                            }}
                            onBack={() => {
                              if (!retiring()) closeChat();
                            }}
                            retiring={retiring()}
                            onRetired={() => finishChatRetire(pane.id)}
                          />
                        </div>
                      );
                    }}
                  </For>
                  <Show when={!showChat()}>
                    <Show
                      when={needsSetup()}
                      fallback={
                        <div class="select-chat">
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
                  </Show>
                </Show>
              </main>
            </div>
          )}
        </Show>
      </Show>
    </Show>
  );
}

function Offline(props: { onRetry: () => void }) {
  return (
    <div class="offline" role="alert">
      <span class="boot__mark">
        <svg viewBox="0 0 32 32" aria-hidden="true">
          <path d="M16 4.5 27 16 16 27.5 5 16Z" />
          <path d="M16 11.4 20.6 16 16 20.6 11.4 16Z" />
        </svg>
      </span>
      <h1 class="offline__title">{t("boot.offline.title")}</h1>
      <p class="offline__text">{t("boot.offline.text")}</p>
      <button class="btn btn--quiet" type="button" onClick={props.onRetry}>
        {t("boot.retry")}
      </button>
    </div>
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
  return {
    text: message.text,
    file: message.file !== null,
    card: message.card != null,
  };
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
// The event's optional fields keep their three states: a string, null, or absent.
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
  if (
    reason !== "first_use_local" &&
    reason !== "first_script_run" &&
    reason !== "first_mcp_call" &&
    reason !== "rule" &&
    reason !== "grant"
  )
    return null;
  const card: Approval = {
    approval,
    run,
    bot,
    chat,
    action,
    step: typeof data.step === "number" ? data.step : 0,
    tool,
    summary,
    reason,
    ambiguous: data.ambiguous === true,
    rule: text("rule"),
  };
  const arguments_ = data.arguments;
  if (arguments_ !== null && typeof arguments_ === "object" && !Array.isArray(arguments_)) {
    card.arguments = arguments_ as Record<string, unknown>;
  }
  const undoable = data.undoable;
  if (undoable === "no" || undoable === "unknown") card.undoable = undoable;
  // A null here is the Space saying that no lasting rule can come from the
  // action. It must not read as silence, or the page would offer what the
  // Space refuses.
  if ("lasting_scope" in data) card.lasting_scope = text("lasting_scope");
  card.computer = text("computer");
  card.acting = text("acting");
  card.call = text("call");
  card.index = typeof data.index === "number" ? data.index : null;

  return card;
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
    if (reason.code === "forbidden") return t("error.permission");
    if (reason.code === "not_found") return t("error.record");
    if (reason.code === "invalid_argument") return t("error.invalid");
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
    if (reason.code === "forbidden") return t("error.permission");
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
  return (
    value.length < MAX_CURSOR.length || (value.length === MAX_CURSOR.length && value <= MAX_CURSOR)
  );
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
