// A Space that answers every command from memory and records what it was asked.
// Tests import `space` to set up and read what happened, and hand
// `FakeCommands` to the client in place of its WebSocket.
import { vi } from "vitest";

import type { SubscriptionHandlers } from "../lib/ws";
import type {
  Approval,
  Bot,
  Card,
  CatalogStatus,
  CatalogVendor,
  Chat,
  ChatStatus,
  Computer,
  EventKind,
  FileSummary,
  InstalledPackage,
  McpServer,
  MemoryEntry,
  MemoryKind,
  MemoryList,
  Message,
  Model,
  Provider,
  RecordEvent,
  Rule,
  RunView,
  Skill,
  ThinkingLevel,
} from "../types";

const account = { id: "u1", display_name: "Ada", is_admin: true };

// The bytes each kind of memory one Bot may hold, as the Space counts them. A
// test may choose the entries, and the room a kind uses follows from them.
const MEMORY_LIMITS: Record<MemoryKind, number> = { rule: 4 * 1024, note: 16 * 1024 };
const bot = {
  id: "b1",
  owner: "u1",
  name: "Scout",
  computer: "p1",
  chat: "h1",
  full_access: false,
  grants: [] as string[],
  // The Space holds these as absent until a Bot names one, and a test may give
  // the Bot either.
  thinking: null as ThinkingLevel | null,
  model: null as string | null,
  // The MCP servers and skills the Bot uses, when the Space lists them.
  mcps: ["s1"],
  skills: ["k1"],
};
const otherBot = {
  id: "b2",
  owner: "u1",
  name: "Quill",
  computer: "p1",
  chat: "h2",
  full_access: false,
  grants: [] as string[],
  thinking: null,
  model: null,
  mcps: [] as string[],
  skills: [] as string[],
};
const thirdBot = {
  id: "b3",
  owner: "u1",
  name: "Sage",
  computer: "p1",
  chat: "h3",
  full_access: false,
  grants: [] as string[],
  thinking: null,
  model: null,
  mcps: [] as string[],
  skills: [] as string[],
};
const computer = {
  id: "p1",
  owner: "u1",
  kind: "local",
  online: true,
  workspace: "/w",
  // Its name for its owner, which is what the client shows beside its kind.
  name: "Workstation",
  sharing: "shared" as Computer["sharing"],
  space: false,
  supports_grants: true,
  supports_full_access: true,
  // The daemon fences this machine's commands, and it has said so. A test may
  // take the word away, or leave the Computer yet to give it.
  sandboxed: true as boolean | null,
};
// Another paired Local Computer, which is where a Bot may move.
const otherComputer = {
  id: "p9",
  owner: "u1",
  kind: "local",
  online: false,
  workspace: "/other",
  name: "Laptop",
  sharing: "shared" as Computer["sharing"],
  space: false,
  supports_grants: true,
  supports_full_access: true,
  sandboxed: true as boolean | null,
};
// The Computer the Space runs itself, which every User gets when they first
// create a Bot. A Hosted Computer's sharing may change.
const hostedComputer = {
  id: "p5",
  owner: "u1",
  kind: "hosted",
  online: true,
  workspace: "/hosted",
  name: "Space Computer",
  sharing: "shared" as Computer["sharing"],
  space: true,
  supports_grants: false,
  supports_full_access: false,
  sandboxed: true,
};
// A fresh Chat carries only what its creation events name: a test that wants
// the Space to project a state sets `space.statuses` and `space.activeRuns`,
// and one that wants the page to read the record leaves them away.
const chat = { id: "h1", owner: "u1", bot: "b1" } as unknown as Chat;
const otherChat = { id: "h2", owner: "u1", bot: "b2" } as unknown as Chat;
const thirdChat = { id: "h3", owner: "u1", bot: "b3" } as unknown as Chat;
const computers: Computer[] = [computer, otherComputer, hostedComputer];

// The MCP servers the fake Space lists: one the Admin connected for the Space,
// and one a package brought which nobody connected yet.
const searchServer: McpServer = {
  server: "s1",
  name: "Search",
  url: "https://mcp.example.com/search",
  scope: "space",
  user: null,
  by: account.id,
  by_name: account.display_name,
  connected: true,
  credential: "space",
};
const reportServer: McpServer = {
  server: "s2",
  name: "Reports",
  url: "https://mcp.example.com/reports",
  scope: "space",
  user: null,
  by: account.id,
  by_name: account.display_name,
  plugin: "g1",
  connected: false,
  credential: "none",
};
// The stdio server the same package brought: the Space launches its command on
// its own machine, so it has no address. It is connected when the sandbox,
// package, and command are launchable, not because a process is running.
export const stdioServer: McpServer = {
  server: "s3",
  name: "Local",
  transport: "stdio",
  url: null,
  command: "./bin/reporter",
  args: ["--data", "${PLUGIN_DATA}"],
  env: { REPORT_MODE: "weekly" },
  // The record keeps the working directory relative to its root; this one
  // hangs from the package's data directory.
  cwd: "work",
  data_cwd: true,
  scope: "space",
  user: null,
  by: account.id,
  by_name: account.display_name,
  plugin: "g1",
  connected: true,
  credential: "space",
};
// The package the Admin installed, and the Space skill it brought.
const installedPackage: InstalledPackage = {
  plugin: "g1",
  name: "reports",
  version: "1.2.0",
  commit: "0123456789abcdef",
  digest: "sha256:0000000000000000000000000000000000000000000000000000000000000000",
  source: "https://github.com/example/reports",
  enabled: true,
  servers: [
    {
      server: reportServer.server,
      name: reportServer.name,
      transport: "streamable-http",
      url: reportServer.url,
    },
    {
      server: stdioServer.server,
      name: stdioServer.name,
      transport: "stdio",
      command: stdioServer.command,
    },
  ],
  skills: [{ skill: "k1", name: "Weekly report" }],
};
const spaceSkill: Skill = {
  id: "k1",
  name: "Weekly report",
  description: "Writes the week's report from the record.",
  body: "Write the week's report.",
  scripts: [],
  library: "space",
  bot: null,
  plugin: installedPackage.plugin,
};

// The Bot that owns a Chat, like the Space answers bot_show.
function botFor(id: string): Bot {
  const created = made[id];
  if (created) return created;
  if (id === otherBot.id) return otherBot;
  if (id === thirdBot.id) return thirdBot;
  return bot;
}

// The Computer a Bot works on, like the Space answers bot_show.
function computerFor(id: string): Computer {
  return computers.find((known) => known.id === id) ?? computer;
}

// The Bots a test created, which bot_list and bot_show answer like the Space's.
const made: Record<string, Bot> = {};
let madeBots = 0;

// One Chat as the Space projects it: its own state, and the Run it follows.
function project(known: Chat): Chat {
  const status = space.statuses[known.id];
  const active = space.activeRuns[known.id];
  return {
    ...known,
    ...(status === undefined ? {} : { status }),
    ...(active === undefined ? {} : { active_run: active }),
  };
}
// A send or a decision can be held open, so a test can act while the Space
// has not answered yet.
let sendGate: Promise<void> | null = null;
let openSend: (() => void) | null = null;
// A create can be held open, so a test can act while the Space has not answered
// it yet.
let createGate: Promise<void> | null = null;
let openCreate: (() => void) | null = null;
let decideGate: Promise<void> | null = null;
let openDecide: (() => void) | null = null;
let approvalGate: Promise<void> | null = null;
let openApproval: (() => void) | null = null;
// A rule read for one Bot can be held open, so a test can move to another page
// while the answer the first page asked for is still in flight.
let ruleGate: { bot: string; gate: Promise<void>; open: () => void } | null = null;
// Memory reads can be held open, so a test can answer two reads in the order it
// chooses. Each read answers with the entries it held when it was asked, so an
// answer that leaves later speaks about a newer record.
let holdingMemories = false;
let listGate: Promise<void> | null = null;
let openList: (() => void) | null = null;

// The URL a browser sign-in is opened at: the Space's own origin, never the
// provider's. The bind route sets the Space's cookie and redirects to the
// provider, so the page opens exactly this URL and adds nothing to it.
export function bindUrl(signIn: string): string {
  return `${window.location.origin}/api/mcp/signin/bind?sign_in=${signIn}`;
}

// The fixed explanation the Space's poll reports for a browser sign-in a newer
// one replaced: an account keeps one pending browser sign-in at a time, so a
// second start fails the first. Mirrors the Space's own constant.
export const signInReplacedReason = "a newer sign-in replaced this one";

// The fixed explanation the fake Space's poll reports for a sign-in it refused.
export const signInRefusedReason = "the sign-in was refused";

// A small app Card's HTML, as the Space stores it in a File: a Card is an HTML
// document, and the client reads it through the File download path.
export const appHtml = [
  "<!doctype html>",
  "<html>",
  "<head><title>Test card</title></head>",
  "<body><p>Hello from the card.</p></body>",
  "</html>",
].join("");

// The File the fake Space stores the app's HTML in.
export const appFile: FileSummary = {
  id: "f2",
  chat: "h1",
  name: "card.html",
  media_type: "text/html",
  size: appHtml.length,
  sha256: "cd".repeat(32),
  author: { kind: "bot", id: "b1" },
  at: "2026-01-01T00:00:00.000Z",
  seq: "3",
};

// The Card a tool's result carries: the app's File, the call it ran with, and
// the object it answered.
export const appCard: Card = {
  kind: "app",
  file: appFile.id,
  input: { query: "hello" },
  result: { answer: 42 },
  csp: null,
};

// Answers the client's File download path from the fake Space's Files, so an
// app Card's HTML arrives the way a real Space serves a File's bytes.
export function serveFiles() {
  return vi.spyOn(globalThis, "fetch").mockImplementation(async (input) => {
    const url = typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
    const path = new URL(url, "http://space.test").pathname;
    if (!path.startsWith("/api/files/")) return new Response("not found", { status: 404 });
    const id = decodeURIComponent(path.slice("/api/files/".length));
    const html = space.fileContents[id];
    if (html === undefined) return new Response("not found", { status: 404 });
    return new Response(html, { status: 200, headers: { "content-type": "text/html" } });
  });
}

// One browser sign-in the fake Space started.
interface SignInFlow {
  id: string;
  server: string | null;
  name: string | null;
  url: string | null;
  done: boolean;
}

// What a started sign-in's next poll answers. The newest flow's state is the
// test's `signInState` knob; an older flow keeps what the Space recorded when a
// newer start replaced it.
function signInStateOf(id: string): "pending" | "finished" | "failed" {
  if (space.signIns.at(-1)?.id === id) return space.signInState;
  return space.signInStates[id] ?? "pending";
}

// The identifier the fake Space gives the next server it records: the first
// free `s<n>`, so a fixture server cannot make two rows share an id.
function nextServerId(): string {
  const taken = new Set(space.servers.map((server) => server.server));
  let index = 1;
  while (taken.has(`s${index}`)) index += 1;
  return `s${index}`;
}

export const space = {
  account,
  bot,
  otherBot,
  thirdBot,
  computer,
  otherComputer,
  hostedComputer,
  chat,
  otherChat,
  thirdChat,
  asked: [] as string[],
  // Every command the client sent, with its arguments as they arrived: a test
  // reads the boundary rather than what a case chose to keep, so an argument
  // the client should not have sent shows up.
  frames: [] as { name: string; args?: unknown }[],
  sent: [] as unknown[],
  // The timeout each command reached the fake Space with, so a test can see a
  // command that names its own wait.
  timeouts: {} as Record<string, number | undefined>,
  // The MCP servers the Space lists, the packages the Admin installed, and the
  // Space's skills. The commands that change them answer from these.
  servers: [searchServer, reportServer, stdioServer] as McpServer[],
  packages: [installedPackage] as InstalledPackage[],
  skills: [spaceSkill] as Skill[],
  // What the newest started sign-in's next poll answers with. A finished
  // sign-in connects the server the start named, or registers the new one; a
  // newer start fails an older pending one with `signInReplacedReason`.
  signInState: "pending" as "pending" | "finished" | "failed",
  // How many browser sign-ins the Space started, and each one in order.
  signInSeq: 0,
  signIns: [] as SignInFlow[],
  signInStates: {} as Record<string, "pending" | "finished" | "failed">,
  files: [] as FileSummary[],
  // The HTML each File holds, so the File download path can answer it.
  fileContents: {} as Record<string, string>,
  messages: [] as Message[],
  otherMessages: [] as Message[],
  chats: [chat] as Chat[],
  // What the Space projects each Chat to be doing. A Chat it omits is idle.
  statuses: {} as Record<string, ChatStatus>,
  // The Run each Chat works on, as the Space projects it: the waiting Run while
  // it waits, else the oldest queued or running Run, else none.
  activeRuns: {} as Record<string, string | null>,
  chatDelay: null as null | ((chatId: string) => Promise<void>),
  handlers: null as SubscriptionHandlers | null,
  // What the record says about each Run. A Run this map omits has succeeded.
  // A Run can also carry the steps and actions the record holds for it, which
  // is how a reopened Chat shows what the Run already did.
  runs: {} as Record<
    string,
    {
      status: RunView["status"];
      position?: number;
      approval?: string;
      steps?: { index: number; tool_calls: number }[];
      actions?: { id: string; step: number; tool: string; status: string; error?: string | null }[];
      // Why the Run failed, when it did before any step or action.
      error?: string | null;
    }
  >,
  runShows: [] as string[],
  // A run_show answer can be held open, so a test can act while it is in flight.
  runHolds: {} as Record<
    string,
    { gate: Promise<void>; release: () => void; fail: () => void } | undefined
  >,
  // Sends that settled in the fake Space.
  sendsDone: 0,
  // Runs the Space accepted, in order.
  sends: 0,
  // The approvals the Space lists for the Chat: the ones still waiting.
  approvals: [] as Approval[],
  // The decisions the client sent, in order.
  decisions: [] as { approval: string; allow: boolean; lasting: boolean }[],
  // Decisions that settled in the fake Space.
  decidesDone: 0,
  // The Providers the Space has connected, like provider_list answers.
  providers: [] as Provider[],
  // The Models the Space offers, like model_list answers.
  models: [] as Model[],
  // The Computers the Space knows, like computer_list answers. A Bot may move
  // between the paired Local ones.
  computers: computers,
  // The rules each Bot holds, like rule_list answers.
  rules: [] as Rule[],
  // The memory each Bot holds, by Bot, like memory_list answers. The command's
  // rows name no Bot, so the fake keeps the entries under the Bot they belong
  // to. A test sets them directly; a removal takes one away.
  memories: {} as Record<string, MemoryEntry[]>,
  // The room each kind of memory one Bot has, like memory_list answers.
  memoryLimits: { ...MEMORY_LIMITS } as Record<MemoryKind, number>,
  // Every vendor the catalog knows, like catalog_list answers.
  vendors: [] as CatalogVendor[],
  // What the catalog's cache holds, like catalog_status answers.
  catalog: null as CatalogStatus | null,
  // The Bot updates, in order, with everything the client sent.
  updates: [] as Record<string, unknown>[],
  // The moves and the resets the Space was asked for.
  moves: [] as { bot: string; computer: string }[],
  // The Bots a test created, with what each was asked for.
  creates: [] as { name: string; computer?: string }[],
  resets: [] as string[],
  revoked: [] as string[],
  deviceApprovals: [] as string[],
  // The Hosted Computers the Space was asked to create, and the sharing
  // changes it was asked to make.
  computerCreates: [] as { name: string; sharing: Computer["sharing"] }[],
  computerUpdates: [] as { computer: string; sharing: Computer["sharing"] }[],
  // The rules the Space was asked to add and to remove.
  ruleAdds: [] as unknown[],
  ruleRemoves: [] as string[],
  // The memory removals the Space was asked for, in order: the Bot the entry
  // must belong to, and the entry.
  memoryRemoves: [] as { bot: string; memory: string }[],
  // The memory reads the Space is holding, oldest first. A test lets one answer
  // with `releaseMemoryRead`, so two reads can land out of the order they were
  // asked for.
  memoryHolds: [] as { release: () => void }[],
  // The Providers the Space connected and refreshed, and the Models made default.
  connects: [] as unknown[],
  refreshes: [] as string[],
  defaults: [] as string[],
  // The catalog reads the Space was asked for.
  catalogRefreshes: 0,
  // A command the Space refuses, with the error it answers. A test sets one to
  // see what the page does when the Space says no.
  refuse: {} as Record<string, Error | undefined>,
  // The seq of the last event the client read.
  seq: 100,
  // Puts the Space back where a test expects to find it.
  reset() {
    space.releaseSends();
    space.releaseCreates();
    space.releaseDecide();
    space.releaseApprovals();
    space.releaseLists();
    space.releaseMemories();
    space.computer.kind = "local";
    space.computer.online = true;
    space.computer.name = "Workstation";
    space.computer.sharing = "shared";
    space.computer.space = false;
    space.computer.workspace = "/w";
    space.computer.id = "p1";
    space.otherComputer.kind = "local";
    space.otherComputer.online = false;
    space.otherComputer.name = "Laptop";
    space.otherComputer.sharing = "shared";
    space.otherComputer.space = false;
    space.otherComputer.workspace = "/other";
    space.otherComputer.id = "p9";
    space.computer.supports_grants = true;
    space.computer.supports_full_access = true;
    space.otherComputer.supports_grants = true;
    space.otherComputer.supports_full_access = true;
    space.computer.sandboxed = true;
    space.otherComputer.sandboxed = true;
    space.hostedComputer.sandboxed = true;
    space.hostedComputer.kind = "hosted";
    space.hostedComputer.name = "Space Computer";
    space.hostedComputer.sharing = "shared";
    space.hostedComputer.space = true;
    // A test that created a Computer leaves the first three in place.
    space.computers = computers;
    space.bot.name = "Scout";
    space.bot.computer = "p1";
    space.bot.full_access = false;
    space.bot.grants = [];
    space.bot.thinking = null;
    space.bot.model = null;
    space.asked = [];
    space.frames = [];
    space.sent = [];
    space.timeouts = {};
    space.files = [];
    space.fileContents = {};
    space.messages = [];
    space.otherMessages = [];
    space.chats = [chat];
    space.servers = [searchServer, reportServer, stdioServer];
    space.packages = [installedPackage];
    space.skills = [spaceSkill];
    space.signInState = "pending";
    space.signInSeq = 0;
    space.signIns = [];
    space.signInStates = {};
    space.bot.mcps = [searchServer.server];
    space.bot.skills = [spaceSkill.id];
    space.otherBot.mcps = [];
    space.otherBot.skills = [];
    space.thirdBot.mcps = [];
    space.thirdBot.skills = [];
    space.statuses = {};
    space.activeRuns = {};
    space.chatDelay = null;
    space.handlers = null;
    space.runs = {};
    space.runShows = [];
    space.runHolds = {};
    space.sendsDone = 0;
    space.sends = 0;
    space.approvals = [];
    space.decisions = [];
    space.decidesDone = 0;
    space.providers = [
      { id: "v1", protocol: "chat_completions", base_url: "http://provider", vendor: null },
    ];
    space.models = [];
    space.rules = [];
    space.memories = {};
    space.memoryLimits = { ...MEMORY_LIMITS };
    space.vendors = [];
    space.catalog = null;
    space.updates = [];
    space.moves = [];
    space.creates = [];
    for (const key of Object.keys(made)) delete made[key];
    madeBots = 0;
    space.resets = [];
    space.revoked = [];
    space.deviceApprovals = [];
    space.computerCreates = [];
    space.computerUpdates = [];
    space.ruleAdds = [];
    space.ruleRemoves = [];
    space.memoryRemoves = [];
    space.connects = [];
    space.refreshes = [];
    space.defaults = [];
    space.catalogRefreshes = 0;
    space.refuse = {};
    space.seq = 100;
    ruleGate = null;
  },
  holdSends() {
    sendGate = new Promise<void>((resolve) => {
      openSend = resolve;
    });
  },
  releaseSends() {
    openSend?.();
    openSend = null;
    sendGate = null;
  },
  // Holds a create open, so a test can act while the Space has not answered it.
  holdCreates() {
    createGate = new Promise<void>((resolve) => {
      openCreate = resolve;
    });
  },
  releaseCreates() {
    openCreate?.();
    openCreate = null;
    createGate = null;
  },
  // Holds one Bot's rule reads open, so a test can act while that Bot's page
  // is still waiting on the rules its own write asked to read again.
  holdRules(bot: string) {
    let open!: () => void;
    const gate = new Promise<void>((resolve) => {
      open = resolve;
    });
    ruleGate = { bot, gate, open };
  },
  // Lets the held rule reads answer.
  releaseRules() {
    ruleGate?.open();
    ruleGate = null;
  },
  // Holds every memory read open, so a test can answer the reads in the order it
  // chooses rather than the order the page asked for them.
  holdMemories() {
    holdingMemories = true;
    space.memoryHolds = [];
  },
  // Lets the held memory read at `which` answer, counted from the oldest.
  releaseMemoryRead(which = 0) {
    const [held] = space.memoryHolds.splice(which, 1);
    held?.release();
  },
  // Lets every held memory read answer, oldest first.
  releaseMemories() {
    holdingMemories = false;
    const held = space.memoryHolds.splice(0);
    for (const hold of held) hold.release();
  },
  // An approval read can be held open, so a test can answer while the Space's
  // word about the record is still in flight.
  holdApprovals() {
    approvalGate = new Promise<void>((resolve) => {
      openApproval = resolve;
    });
  },
  releaseApprovals() {
    openApproval?.();
    openApproval = null;
    approvalGate = null;
  },
  holdDecide() {
    decideGate = new Promise<void>((resolve) => {
      openDecide = resolve;
    });
  },
  releaseDecide() {
    openDecide?.();
    openDecide = null;
    decideGate = null;
  },
  // A library read can be held open, so a test can see the view while its
  // first list is still in flight.
  holdLists() {
    listGate = new Promise<void>((resolve) => {
      openList = resolve;
    });
  },
  releaseLists() {
    openList?.();
    openList = null;
    listGate = null;
  },
  holdRunShow(run: string) {
    let release!: () => void;
    let fail!: () => void;
    const gate = new Promise<void>((resolve, reject) => {
      release = resolve;
      fail = () => reject(new Error("the run_show answer was lost"));
    });
    space.runHolds[run] = { gate, release, fail };
  },
  // Lets every read held about one Run answer, after the page moved on.
  releaseRunShow(run: string) {
    space.runHolds[run]?.release();
    space.runHolds[run] = undefined;
  },
  // A page that reconnects reads a snapshot of the events it missed, and the
  // Space says the cursor it now holds. This is what a real reconnect delivers.
  resume(events: RecordEvent[]) {
    space.seq += 1;
    space.handlers?.onSnapshot?.(events, `s${space.seq}`, true);
  },
  // The Space removed one installed package: its servers and its skills leave
  // the lists, and the record tells every signed-in account with only the
  // package's id. A member sees the rows leave the lists it holds, without
  // ever reading the package list the Admin alone may see.
  removePackage(plugin: string) {
    space.packages = space.packages.filter((pkg) => pkg.plugin !== plugin);
    space.servers = space.servers.filter((server) => server.plugin !== plugin);
    space.skills = space.skills.filter((skill) => skill.plugin !== plugin);
    emit("neha.plugin.removed", { plugin });
  },
  // A Bot Message that carries an app Card, as the Space records a tool's
  // result. The Card's HTML becomes the File's bytes, and the Message joins the
  // Chat's record, so chat_read answers with the Card.
  showAppCard(card: Card = appCard, html = appHtml): Message {
    const file = card.file ?? null;
    if (file !== null) {
      space.files = [
        ...space.files.filter((known) => known.id !== file),
        { ...appFile, id: file, size: html.length },
      ];
      space.fileContents = { ...space.fileContents, [file]: html };
    }
    const message: Message = {
      id: `card${space.messages.length + 1}`,
      chat: space.chat.id,
      author: { kind: "bot", id: bot.id },
      acting: "u1",
      text: "Here is the result.",
      at: "2026-01-01T00:00:01.000Z",
      seq: String(space.messages.length + 2),
      run: null,
      file: null,
      card,
    };
    space.messages = [...space.messages, message];
    return message;
  },
  // The record compacts a Chat: the Message whose cursor is `upToSeq` carries
  // the boundary mark a read answers with, and the summary stays the Model's,
  // never the stream's. The record keeps every Message.
  compact(upToSeq: string, summary = "Earlier Messages were summarized.") {
    space.messages = space.messages.map((message) =>
      message.seq === upToSeq ? { ...message, compacted: true } : { ...message },
    );
    emit("neha.chat.compacted", {
      chat: space.chat.id,
      bot: bot.id,
      up_to_seq: upToSeq,
      summary,
    });
  },
  // Tells the page that the record added a Message, with the Card it carries.
  announceCard(message: Message) {
    emit("neha.message.added", {
      chat: message.chat,
      message: message.id,
      text: message.text,
      file: message.file,
      run: message.run,
      ...(message.card ? { card: message.card } : {}),
    });
  },
};

// One event the Space writes to the record, delivered live to the page that
// follows it. The commands that change the record write their event, so a page
// hears about its own answer the same way it hears about everyone else's.
function emit(kind: EventKind, data: Record<string, unknown>) {
  space.seq += 1;
  space.handlers?.onEvent?.({
    seq: String(space.seq),
    kind,
    version: 1,
    at: "2026-01-01T00:00:00.000Z",
    author: { kind: "user", id: "u1" },
    acting: "u1",
    data,
  });
}

// The client's own decoder rule shapes every frame, and the fake checks with it
// so a wrong-shaped call site fails here instead of passing only against the
// fake. The rule is imported through a promise started here rather than by a
// static import awaited at module load: the UI tests replace the client's
// `Commands` with the fake, and waiting for that mock while it is being created
// would close a module cycle between the fake and the mock.
type CommandShape = (name: string, args?: unknown) => unknown;

let commandShape: CommandShape | null = null;
const commandShapeReady: Promise<CommandShape> = import("../lib/ws").then((ws) => {
  commandShape = ws.commandPayload;
  return ws.commandPayload;
});

// Nothing is returned once the rule has loaded, so a command checks its shape
// without yielding, the way the Space's decoder does.
function checkCommandShape(name: string, args: unknown): void | Promise<void> {
  if (commandShape) {
    commandShape(name, args);
    return;
  }
  return commandShapeReady.then((shape) => {
    shape(name, args);
  });
}

export class FakeCommands {
  async subscribe(handlers: SubscriptionHandlers): Promise<void> {
    space.handlers = handlers;
    // A real Space answers a subscribe with one snapshot frame, which a page
    // that asked for no cursor reads as its first page of the record.
    await Promise.resolve();
    handlers.onSnapshot?.([], "", false);
  }

  async close(): Promise<void> {
    space.handlers = null;
  }

  async command<T>(name: string, args?: unknown, timeoutMs?: number): Promise<T> {
    // The real decoder refuses a unit command that names `args` and an argument
    // command that names none. Checking with the client's own rule makes a
    // wrong-shaped call site fail here instead of passing only against the fake.
    const loading = checkCommandShape(name, args);
    if (loading) await loading;
    space.asked.push(name);
    space.frames.push({ name, args });
    const refused = space.refuse[name];
    if (refused) throw refused;
    space.timeouts[name] = timeoutMs;
    switch (name) {
      case "provider_list":
        return space.providers.map((known) => ({ ...known })) as T;
      case "model_list":
        return space.models.map((known) => ({ ...known })) as T;
      case "bot_list":
        // Each Chat's Bot answers, like the Space lists them.
        return space.chats.map((known) => ({ ...botFor(known.bot) })) as T;
      case "chat_list":
        // Like the Space's JSON, each answer carries its own copies, with what
        // the Space projects the Chat to be doing and the Run it follows.
        return space.chats.map((known) => project(known)) as T;
      case "chat_read": {
        const asked = args as { chat: string };
        // The answer describes the record as it was when the read was sent, so
        // a held answer can be overtaken by a live event a test fires while it
        // waits. Like the Space's JSON, each answer carries its own copies: a
        // page that keys rows by object identity must keep its own state
        // across a refresh that answers with the same record.
        const snapshot =
          asked.chat === space.chat.id ? [...space.messages] : [...space.otherMessages];
        await space.chatDelay?.(asked.chat);
        return JSON.parse(JSON.stringify(snapshot)) as T;
      }
      case "file_list":
        return [...space.files] as T;
      case "approval_list": {
        // Like the Space's JSON, each answer carries its own copies, and names
        // only the approvals the User may still answer: the asked Chat's, or
        // every Chat's when the call names none.
        const asked = args as { chat?: string };
        if (approvalGate) await approvalGate;
        return space.approvals
          .filter((approval) => asked.chat === undefined || approval.chat === asked.chat)
          .map((approval) => ({ ...approval })) as T;
      }
      case "computer_list":
        return space.computers.map((known) => ({ ...known })) as T;
      case "computer_create": {
        // A Space names the Hosted Computer it now holds and writes it to the
        // record, so the page hears about its own write.
        const asked = args as { name: string; sharing: Computer["sharing"] };
        space.computerCreates.push(asked);
        const created: Computer = {
          // An id the paired Computers do not carry, so a created Computer is
          // never mistaken for one the machine paired.
          id: `p${space.computers.length + 10}`,
          owner: "u1",
          kind: "hosted",
          online: true,
          workspace: `/hosted/${asked.name}`,
          name: asked.name,
          sharing: asked.sharing,
          space: false,
          supports_grants: false,
          supports_full_access: false,
          sandboxed: true,
        };
        space.computers = [...space.computers, created];
        emit("neha.computer.created", {
          computer: created.id,
          owner: created.owner,
          kind: created.kind,
          workspace: created.workspace,
          name: created.name,
          sharing: created.sharing,
          space: created.space,
        });
        return { ...created } as T;
      }
      case "computer_update": {
        const asked = args as { computer: string; sharing: Computer["sharing"] };
        const known = space.computers.find((entry) => entry.id === asked.computer);
        if (!known) throw new Error("the fake Space has no such Computer");
        space.computerUpdates.push(asked);
        known.sharing = asked.sharing;
        emit("neha.computer.updated", { computer: known.id, sharing: known.sharing });
        return { ...known } as T;
      }
      case "bot_show": {
        const asked = args as { bot: string };
        const known = botFor(asked.bot);
        const own = space.chats.find((entry) => entry.bot === known.id) ?? space.chat;
        return {
          bot: { ...known },
          computer: { ...computerFor(known.computer) },
          chat: project(own),
        } as T;
      }
      case "bot_create": {
        // A Bot answers with itself, its Computer, and its Chat, and the Space
        // records the create as its own two events, so the client hears about
        // its own write and reads the workspace again.
        const asked = args as { name?: string; computer?: string };
        const name = (asked.name ?? "").trim();
        if (name === "") throw new Error("a Bot needs a name");
        if (createGate) await createGate;
        space.creates.push({
          name,
          ...(asked.computer === undefined ? {} : { computer: asked.computer }),
        });
        madeBots += 1;
        const id = `b-made-${madeBots}`;
        const chatId = `h-made-${madeBots}`;
        // A Bot that names no Computer joins the owner's Space Computer, which
        // every User receives; a named one is a Computer the User owns.
        const target =
          asked.computer === undefined
            ? hostedComputer
            : (computers.find((known) => known.id === asked.computer) ?? hostedComputer);
        const created: Bot = {
          id,
          owner: "u1",
          name,
          computer: target.id,
          chat: chatId,
          full_access: false,
          grants: [],
          thinking: null,
          model: null,
          skills: [],
          mcps: [],
        };
        made[id] = created;
        // The Chat the events name, held as the list holds it: what the Space
        // projects it to be doing, which a test says and the record leaves
        // alone until a run event does.
        const opened = { id: chatId, owner: "u1", bot: id } as unknown as Chat;
        space.chats = [...space.chats, opened];
        emit("neha.bot.created", {
          bot: id,
          owner: "u1",
          name,
          computer: target.id,
          chat: chatId,
        });
        emit("neha.chat.created", { chat: chatId, owner: "u1", bot: id });
        return {
          bot: { ...created },
          computer: { ...target },
          // The answer names the Chat by identity, the way the Space does: it
          // had not projected the Chat when it wrote the answer, so the Bot's
          // own answer is what projects it.
          chat: { id: chatId, owner: "u1", bot: id },
        } as T;
      }
      case "bot_update": {
        // A Space answers with the Bot it now holds, and writes the change to
        // the record, so the page hears about its own write.
        const asked = args as Record<string, unknown> & { bot: string };
        space.updates.push(asked);
        const known = botFor(asked.bot);
        const changed: Record<string, unknown> = {};
        if (typeof asked.name === "string") {
          known.name = asked.name;
          // A rename is its own event, and it names the Bot and its new name.
          // What else changed travels as the Bot's own update.
          emit("neha.bot.renamed", { bot: known.id, name: asked.name });
        }
        if (Array.isArray(asked.grants)) {
          known.grants = [...(asked.grants as string[])];
          changed.grants = known.grants;
        }
        if (typeof asked.grant_add === "string" && !known.grants.includes(asked.grant_add)) {
          known.grants = [...known.grants, asked.grant_add].sort();
          changed.grants = known.grants;
        }
        if (typeof asked.grant_remove === "string") {
          known.grants = known.grants.filter((known2) => known2 !== asked.grant_remove);
          changed.grants = known.grants;
        }
        if (typeof asked.full_access === "boolean") {
          known.full_access = asked.full_access;
          changed.full_access = asked.full_access;
        }
        if ("model" in asked) {
          known.model = (asked.model as string | null) ?? null;
          changed.model = known.model;
        }
        // Back to the Space's default is a field of its own, so an absent
        // Model or level never means it.
        if (asked.reset_model === true) {
          known.model = null;
          changed.model = null;
        }
        if ("thinking" in asked) {
          known.thinking = (asked.thinking as Bot["thinking"]) ?? null;
          changed.thinking = known.thinking;
        }
        if (asked.reset_thinking === true) {
          known.thinking = null;
          changed.thinking = null;
        }
        if (Object.keys(changed).length > 0) {
          emit("neha.bot.updated", { bot: known.id, ...changed });
        }
        return { ...known, user: "u1" } as T;
      }
      case "bot_move": {
        const asked = args as { bot: string; computer: string };
        space.moves.push(asked);
        const known = botFor(asked.bot);
        const target = space.computers.find((entry) => entry.id === asked.computer);
        if (!target) throw new Error("the fake Space has no such Computer");
        const from = known.computer;
        known.computer = target.id;
        emit("neha.bot.moved", { bot: known.id, from, to: target.id });
        return { bot: { ...known }, from: { ...computerFor(from) }, to: { ...target } } as T;
      }
      case "computer_reset": {
        const asked = args as { computer: string };
        space.resets.push(asked.computer);
        return { computer: asked.computer } as T;
      }
      case "device_approve": {
        const asked = args as { code: string };
        space.deviceApprovals.push(asked.code);
        return { account: { id: "a1", display_name: "Ada", is_admin: true } } as T;
      }
      case "computer_revoke": {
        const asked = args as { computer: string };
        space.revoked.push(asked.computer);
        return { id: asked.computer, kind: "local", online: false } as T;
      }
      case "rule_list": {
        const asked = args as { bot?: string };
        if (ruleGate && asked.bot === ruleGate.bot) await ruleGate.gate;
        // Like the Space's read, each row names the skill and the server the
        // record keeps by identifier, so a reader shows a name it can check
        // against its library. A rule whose skill or server is gone answers
        // null, which the client reads as the identifier.
        return space.rules
          .filter((known) => asked.bot === undefined || known.bot === asked.bot)
          .map((known) => ({
            ...known,
            ...(known.skill
              ? {
                  skill_name: space.skills.find((skill) => skill.id === known.skill)?.name ?? null,
                }
              : {}),
            ...(known.server
              ? {
                  server_name:
                    space.servers.find((server) => server.server === known.server)?.name ?? null,
                }
              : {}),
          })) as T;
      }
      case "rule_add": {
        const asked = args as Record<string, unknown> & {
          bot: string;
          effect: Rule["effect"];
          tool: string;
        };
        space.ruleAdds.push(asked);
        const rule: Rule = {
          rule: `rule${space.rules.length + 1}`,
          bot: asked.bot,
          user: "u1",
          // The rule's cursor is the event's cursor, which `emit` writes next.
          seq: String(space.seq + 1),
          effect: asked.effect,
          tool: asked.tool,
          command_prefix: (asked.command_prefix as string | null) ?? null,
          path_prefix: (asked.path_prefix as string | null) ?? null,
        };
        space.rules = [...space.rules, rule];
        emit("neha.rule.added", { rule: rule.rule, bot: rule.bot });
        return rule as T;
      }
      case "rule_remove": {
        const asked = args as { rule: string };
        space.ruleRemoves.push(asked.rule);
        space.rules = space.rules.filter((known) => known.rule !== asked.rule);
        emit("neha.rule.removed", { rule: asked.rule });
        return { rule: asked.rule } as T;
      }
      case "memory_list": {
        const asked = args as { bot: string };
        const entries = (space.memories[asked.bot] ?? []).map((entry) => ({ ...entry }));
        // The room a kind uses is the size of its entries, as the Space counts
        // it, so removing one entry moves both the list and the capacity.
        const used: Record<MemoryKind, number> = { rule: 0, note: 0 };
        for (const entry of entries) used[entry.kind] += entry.size;
        const answer: MemoryList = { entries, used, limits: { ...space.memoryLimits } };
        // A held read keeps the answer it had when it was asked, so a test can
        // let a later read answer before an earlier one.
        if (holdingMemories) {
          let release!: () => void;
          const gate = new Promise<void>((resolve) => {
            release = resolve;
          });
          space.memoryHolds.push({ release });
          await gate;
        }
        return answer as T;
      }
      case "memory_remove": {
        const asked = args as { bot: string; memory: string };
        // The Space refuses an entry that belongs to another Bot, and names the
        // Bot the removal must fall under.
        const entries = space.memories[asked.bot] ?? [];
        if (!entries.some((entry) => entry.memory === asked.memory)) {
          throw new Error("no such memory entry");
        }
        space.memoryRemoves.push(asked);
        space.memories[asked.bot] = entries.filter((entry) => entry.memory !== asked.memory);
        // The Space writes the removal to the record, so the page hears about
        // its own answer the way it hears about another client's.
        emit("neha.memory.removed", { memory: asked.memory });
        return { memory: asked.memory, user: "u1" } as T;
      }
      case "catalog_list":
        return space.vendors.map((known) => ({ ...known })) as T;
      case "catalog_status":
        return space.catalog ? ({ ...space.catalog } as T) : (null as T);
      case "catalog_refresh": {
        // A refresh reads the catalog again, which replaces the cache it holds.
        space.catalogRefreshes += 1;
        space.catalog = {
          ...(space.catalog ?? {
            source: "https://models.dev/api.json",
            builtin: false,
            etag: null,
            fetched_at: null,
            checked_at: null,
            age_seconds: null,
            stale: false,
            provider_count: 0,
            cache_path: "/home/catalog.json",
            model_count: 0,
          }),
          source: "https://models.dev/api.json",
          builtin: false,
          fetched_at: 1_764_000_000,
          checked_at: 1_764_000_000,
          age_seconds: 1,
          stale: false,
        };
        return { status: { ...space.catalog }, changed: true } as T;
      }
      case "provider_connect": {
        const asked = args as Record<string, unknown> & { vendor: string; api_key: string };
        space.connects.push(asked);
        const vendor = space.vendors.find((known) => known.id === asked.vendor);
        const provider: Provider = {
          id: `v${space.providers.length + 1}`,
          protocol:
            vendor?.protocol ?? (asked.protocol as Provider["protocol"]) ?? "chat_completions",
          base_url: (asked.base_url as string) ?? vendor?.api ?? "http://vendor",
          vendor: asked.vendor,
        };
        space.providers = [...space.providers, provider];
        emit("neha.provider.added", { provider: provider.id });
        return { provider, models: [] } as T;
      }
      case "provider_refresh": {
        const asked = args as { provider: string };
        space.refreshes.push(asked.provider);
        emit("neha.provider.models_updated", { provider: asked.provider });
        return { provider: asked.provider, models: [] } as T;
      }
      case "model_default": {
        const asked = args as { model: string };
        space.defaults.push(asked.model);
        space.models = space.models.map((known) => ({
          ...known,
          is_default: known.id === asked.model,
        }));
        const chosen = space.models.find((known) => known.id === asked.model);
        const provider = chosen?.provider ?? "";
        // The Provider's Models changed, so the page hears about it.
        emit("neha.provider.models_updated", { provider });
        return {
          provider,
          models: space.models
            .filter((known) => known.provider === provider)
            .map((known) => ({ ...known })),
          default_model: asked.model,
        } as T;
      }
      case "message_send": {
        if (sendGate) await sendGate;
        try {
          const sent = args as { chat: string; text: string; file?: string };
          space.sent.push(sent);
          space.sends += 1;
          const run = `r${space.sends}`;
          // The place sits behind every Run its Bot has not finished.
          const active = Object.values(space.runs).filter(
            (known) => known.status === "queued" || known.status === "running",
          ).length;
          const position = active + 1;
          space.messages.push({
            id: `m${space.messages.length + 1}`,
            chat: sent.chat,
            author: { kind: "user", id: "u1" },
            acting: null,
            text: sent.text,
            at: "2026-01-01T00:00:00.000Z",
            seq: "3",
            run,
            file: sent.file ?? null,
          });
          space.runs[run] = { status: "queued", position };
          return { run, status: "queued", position } as T;
        } finally {
          space.sendsDone += 1;
        }
      }
      case "approval_decide": {
        const decided = args as { approval: string; allow: boolean; lasting: boolean };
        space.decisions.push(decided);
        try {
          if (decideGate) await decideGate;
          const waiting = space.approvals.find((known) => known.approval === decided.approval);
          if (waiting) space.approvals = space.approvals.filter((known) => known !== waiting);
          // The Space writes the decision to the record, so the page hears
          // about its own answer the same way it hears about any other.
          emit("neha.approval.decided", {
            approval: decided.approval,
            run: waiting?.run ?? "r0",
            allow: decided.allow,
            lasting: decided.lasting,
            user: "u1",
            rule: null,
          });
          return {
            approval: decided.approval,
            run: waiting?.run ?? "r0",
            status: decided.allow ? "allowed" : "denied",
          } as T;
        } finally {
          space.decidesDone += 1;
        }
      }
      case "run_show": {
        const requested = (args as { run: string }).run;
        space.runShows.push(requested);
        const hold = space.runHolds[requested];
        if (hold) await hold.gate;
        const known = space.runs[requested] ?? { status: "succeeded" };
        return {
          id: requested,
          chat: space.chat.id,
          bot: space.bot.id,
          acting: "u1",
          command: "c1",
          message: "m1",
          status: known.status,
          seq: "4",
          position: known.position,
          approval: known.status === "waiting" ? (known.approval ?? null) : null,
          reply: null,
          error: known.error ?? null,
          usage: null,
          steps: known.steps ?? [],
          actions: known.actions ?? [],
        } as T;
      }
      case "mcp_list": {
        if (listGate) await listGate;
        return space.servers.map((server) => ({ ...server })) as T;
      }
      case "mcp_add": {
        const asked = args as {
          name: string;
          url: string;
          scope: "space" | "user";
          credential?: string;
        };
        space.sent.push(asked);
        const connected = asked.credential !== undefined;
        const added: McpServer = {
          server: nextServerId(),
          name: asked.name,
          transport: "streamable-http",
          url: asked.url,
          scope: asked.scope,
          user: asked.scope === "user" ? account.id : null,
          by: account.id,
          by_name: account.display_name,
          connected,
          credential:
            asked.credential === undefined ? "none" : asked.scope === "user" ? "user" : "space",
        };
        space.servers = [...space.servers, added];
        return {
          server: added.server,
          name: added.name,
          url: added.url,
          scope: added.scope,
          user: account.id,
          by: account.id,
          connected,
        } as T;
      }
      case "mcp_connect": {
        const asked = args as { server: string; credential?: string };
        space.sent.push(asked);
        // The caller's credential is personal: a Space server keeps its own
        // credential too, while a User's server has only this one.
        space.servers = space.servers.map((server) =>
          server.server === asked.server
            ? {
                ...server,
                connected: true,
                credential: server.scope === "space" ? "space-and-user" : "user",
              }
            : server,
        );
        return { server: asked.server, connected: true } as T;
      }
      case "mcp_remove": {
        const asked = args as { server: string };
        space.sent.push(asked);
        const removed = space.servers.find((server) => server.server === asked.server);
        space.servers = space.servers.filter((server) => server.server !== asked.server);
        return { server: asked.server, name: removed?.name ?? "server", user: account.id } as T;
      }
      case "mcp_sign_in_start": {
        const asked = args as {
          server?: string;
          name?: string;
          url?: string;
          scope?: "space" | "user";
        };
        space.sent.push(asked);
        // One browser sign-in is active per account: a newer start fails an
        // older one that still waits, and that older flow's poll then reads
        // the fixed reason. The newer flow stays pending.
        for (const earlier of space.signIns) {
          if (signInStateOf(earlier.id) === "pending") {
            space.signInStates[earlier.id] = "failed";
          }
        }
        space.signInSeq += 1;
        const started: SignInFlow = {
          id: `i${space.signInSeq}`,
          server: asked.server ?? null,
          name: asked.name ?? null,
          url: asked.url ?? null,
          done: false,
        };
        space.signInState = "pending";
        space.signIns = [...space.signIns, started];
        space.signInStates[started.id] = "pending";
        return { sign_in: started.id, url: bindUrl(started.id) } as T;
      }
      case "mcp_sign_in_status": {
        const asked = args as { sign_in: string };
        space.sent.push(asked);
        const signingIn = space.signIns.find((flow) => flow.id === asked.sign_in);
        if (!signingIn) throw new Error(`the fake Space has no sign-in ${asked.sign_in}`);
        const state = signInStateOf(signingIn.id);
        space.signInStates[signingIn.id] = state;
        if (state === "finished" && !signingIn.done) {
          signingIn.done = true;
          if (signingIn.server !== null) {
            space.servers = space.servers.map((server) =>
              server.server === signingIn.server
                ? {
                    ...server,
                    connected: true,
                    credential: server.scope === "space" ? "space-and-user" : "user",
                  }
                : server,
            );
          } else {
            space.servers = [
              ...space.servers,
              {
                server: nextServerId(),
                name: signingIn.name ?? "server",
                transport: "streamable-http",
                url: signingIn.url ?? "https://mcp.example.com",
                scope: "space",
                user: null,
                by: account.id,
                by_name: account.display_name,
                connected: true,
                credential: "space",
              },
            ];
          }
        }
        // An older flow answers with the reason the Space recorded when a
        // newer start replaced it; the newest one carries the fixed sentence a
        // refusal or an unreachable exchange reads.
        const replaced = space.signIns.at(-1)?.id !== signingIn.id;
        return {
          sign_in: asked.sign_in,
          state,
          server: signingIn.server,
          connected: state === "finished",
          reason:
            state === "failed" ? (replaced ? signInReplacedReason : signInRefusedReason) : null,
        } as T;
      }
      case "bot_mcp_enable":
      case "bot_mcp_disable": {
        const asked = args as { bot: string; server: string };
        space.sent.push(asked);
        const known = [space.bot, space.otherBot, space.thirdBot].find(
          (candidate) => candidate.id === asked.bot,
        );
        if (known) {
          known.mcps =
            name === "bot_mcp_enable"
              ? [...new Set([...(known.mcps ?? []), asked.server])]
              : (known.mcps ?? []).filter((id) => id !== asked.server);
        }
        const serverName = space.servers.find((server) => server.server === asked.server)?.name;
        return { bot: asked.bot, server: asked.server, name: serverName ?? "server" } as T;
      }
      case "skill_list": {
        if (listGate) await listGate;
        return space.skills.map((skill) => ({ ...skill })) as T;
      }
      case "skill_add": {
        const asked = args as {
          library: "space" | "bot";
          bot?: string;
          document: string;
          scripts?: { name: string; content: string }[];
        };
        space.sent.push(asked);
        const added: Skill = {
          id: `k${space.skills.length + 1}`,
          name: "Added skill",
          description: "Added by a test.",
          body: asked.document,
          scripts: (asked.scripts ?? []).map((script) => ({
            name: script.name,
            sha256: "sha256:0",
          })),
          library: asked.library,
          bot: asked.bot ?? null,
        };
        space.skills = [...space.skills, added];
        return {
          skill: added.id,
          library: added.library,
          bot: added.bot,
          name: added.name,
          description: added.description,
          scripts: added.scripts.map((script) => script.name),
        } as T;
      }
      case "skill_remove": {
        const asked = args as { skill: string };
        space.sent.push(asked);
        const removed = space.skills.find((skill) => skill.id === asked.skill);
        space.skills = space.skills.filter((skill) => skill.id !== asked.skill);
        return { skill: asked.skill, name: removed?.name ?? "skill" } as T;
      }
      case "bot_skill_enable":
      case "bot_skill_disable": {
        const asked = args as { bot: string; skill: string };
        space.sent.push(asked);
        const known = [space.bot, space.otherBot, space.thirdBot].find(
          (candidate) => candidate.id === asked.bot,
        );
        if (known) {
          known.skills =
            name === "bot_skill_enable"
              ? [...new Set([...(known.skills ?? []), asked.skill])]
              : (known.skills ?? []).filter((id) => id !== asked.skill);
        }
        const skillName = space.skills.find((skill) => skill.id === asked.skill)?.name;
        return {
          bot: asked.bot,
          skill: asked.skill,
          name: skillName ?? "skill",
          enabled: name === "bot_skill_enable",
        } as T;
      }
      case "plugin_list": {
        if (listGate) await listGate;
        return space.packages.map((plugin) => ({ ...plugin })) as T;
      }
      case "plugin_install": {
        const asked = args as { source: string };
        space.sent.push(asked);
        const added: InstalledPackage = {
          plugin: `g${space.packages.length + 1}`,
          name: "installed",
          version: null,
          commit: null,
          digest: "sha256:0",
          source: asked.source,
          enabled: true,
          servers: [],
          skills: [],
        };
        space.packages = [...space.packages, added];
        return {
          plugin: added.plugin,
          name: added.name,
          version: added.version,
          digest: added.digest,
          commit: added.commit,
          source: added.source,
          servers: [],
          skills: [],
          skipped: [],
          user: account.id,
        } as T;
      }
      case "plugin_enable":
      case "plugin_disable": {
        const asked = args as { plugin: string };
        space.sent.push(asked);
        const enabled = name === "plugin_enable";
        space.packages = space.packages.map((plugin) =>
          plugin.plugin === asked.plugin ? { ...plugin, enabled } : plugin,
        );
        return { plugin: asked.plugin, enabled, user: account.id } as T;
      }
      case "plugin_remove": {
        const asked = args as { plugin: string };
        space.sent.push(asked);
        const removed = space.packages.find((plugin) => plugin.plugin === asked.plugin);
        space.packages = space.packages.filter((plugin) => plugin.plugin !== asked.plugin);
        return { plugin: asked.plugin, name: removed?.name ?? "package", user: account.id } as T;
      }
      default:
        throw new Error(`the fake Space cannot answer ${name}`);
    }
  }
}
