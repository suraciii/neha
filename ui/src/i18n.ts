import { createSignal } from "solid-js";

export type Locale = "en-US" | "zh-CN";

const en = {
  "auth.headline": "Put work in the hands of a team you can see.",
  "auth.lede":
    "A Bot is a teammate. It has its own Computer, memory, and identity. Work lands in the Chat, and authority always stays with you.",
  "auth.point.space": "One Space, on this machine",
  "auth.point.provider": "Your Provider, your Models",
  "auth.point.computer": "A Computer for each Bot",
  "auth.missing.title": "Open the whole invitation link",
  "auth.missing.lead": "The link printed by",
  "auth.missing.mid": "carries a secret after the",
  "auth.missing.tail": ". Paste the complete link into this browser.",
  "auth.join.title": "Join this Space",
  "auth.join.hint":
    "Become the Admin of this Space. The invitation works once, while the Space runs.",
  "auth.login.title": "Sign in to Neha",
  "auth.login.hint": "Use the account that already belongs to this Space.",
  "auth.name": "Display name",
  "auth.password": "Password",
  "auth.password.hint": "At least 8 characters. A short phrase is fine.",
  "auth.join.action": "Join Space",
  "auth.login.action": "Sign in",
  "auth.working": "Working…",
  "auth.fine":
    "The Space stays on this machine. Your session stays in this browser.",

  "error.invitation": "This invitation is not usable. Start a new Space or ask the Admin.",
  "error.credentials": "The display name or password is not correct.",
  "error.rate": "Too many sign-in attempts. Wait a few minutes.",
  "error.conflict": "This Space already has an Admin. Sign in instead.",
  "error.unreachable": "The browser cannot reach this Space. Is neha serve still running?",
  "error.generic": "The Space cannot complete this request.",
  "error.session": "The session ended. Reload the page and sign in again.",
  "error.record": "That record is no longer available.",
  "error.botAnswer": "The Bot could not answer.",
  "error.password": "The password is too short. Use at least 8 characters.",
  "error.invalid": "There is a problem with that value.",
  "error.upload": "The file could not be uploaded. Try again.",
  "error.fileTooLarge": "{name} is larger than {limit}.",

  "sidebar.chats": "Chats",
  "sidebar.chats.one": "1 Chat",
  "sidebar.chats.many": "{count} Chats",
  "sidebar.empty.title": "No Chats yet",
  "sidebar.empty.text": "Create a Bot and its Chat will appear here.",
  "sidebar.startChat": "Start a Chat with {name}",
  "sidebar.newChat": "New Chat",
  "sidebar.openChat": "Open Chat",
  "sidebar.file": "Sent a File",
  "sidebar.online": "Space online",

  "rail.chats": "Chats",
  "rail.admin": "Admin",
  "rail.user": "User",
  "rail.primary": "Primary navigation",

  "setup.step.one": "Step 1 of 2",
  "setup.step.two": "Step 2 of 2",
  "setup.provider.title": "Connect a Provider",
  "setup.provider.lede":
    "Every Bot in this Space thinks through a Provider. The key stays inside the Space.",
  "setup.bot.title": "Create your first Bot",
  "setup.bot.lede":
    "A Bot has its own identity, memory, and Computer. Give it a name and hand it work.",
  "setup.bot.panelTitle": "Name the teammate",
  "setup.provider.connected.one": "Provider connected",
  "setup.provider.connected.many": "{count} Providers connected",
  "setup.ready": "Ready",
  "setup.models": "Models",
  "setup.none": "None",
  "setup.bot.name": "Name",
  "setup.bot.nameHint": "The name appears in Chats.",
  "setup.bot.accessTitle": "Owner only",
  "setup.bot.accessHint": "Only you can use this Bot.",
  "setup.bot.create": "Create Bot",
  "setup.bot.creating": "Creating…",
  "setup.provider.panelTitle": "OpenAI-compatible Provider",
  "setup.adminOnly": "Admin only",
  "setup.baseUrl": "Base URL",
  "setup.baseUrl.hint": "The address that serves OpenAI Chat Completions.",
  "setup.apiKey": "API key",
  "setup.apiKey.hint": "Stored encrypted in this Space. It can't be read back.",
  "setup.model": "Model name",
  "setup.model.hint": "The exact name the Provider expects.",
  "setup.advanced": "Advanced",
  "setup.contextLimit": "Context limit",
  "setup.maxOutput": "Max output",
  "setup.connect": "Connect Provider",
  "setup.connecting": "Connecting…",
  "setup.provider.note": "A Provider belongs to this Space, shared by every Chat.",
  "setup.noAdmin": "Ask the Space Admin to connect a Provider before a Bot can work.",

  "chat.title": "Chat",
  "chat.aria": "Chat with {name}",
  "chat.loading": "Loading…",
  "chat.localComputer": "Local Computer",
  "chat.hostedComputer": "Hosted Computer",
  "chat.online": "Online",
  "chat.offline": "Offline",
  "chat.back": "Back to Chats",
  "chat.new.title": "Say hello to {name}.",
  "chat.opening": "Opening the Chat…",
  "chat.new.text":
    "Give the Bot work in your own words. It answers here, and the Chat keeps the conversation.",
  "chat.suggestion.hello": "Say hello",
  "chat.suggestion.work": "What are you working on?",
  "chat.suggestion.plan": "Help me plan the next step.",
  "chat.acting": "On your behalf",
  "chat.working": "Working",
  "chat.queue.queued": "Waiting in the queue",
  "chat.queue.waiting.one": "Waiting behind 1 request",
  "chat.queue.waiting.many": "Waiting behind {count} requests",
  "chat.placeholder": "Message {name}…",
  "chat.placeholder.empty": "Message the Bot…",
  "chat.hint": "Enter to send · Shift+Enter for a new line",
  "chat.send": "Send Message",
  "chat.attach": "Attach a file",
  "chat.attachment.remove": "Remove {name}",
  "chat.file.download": "Download {name}",
  "chat.waiting": "Waiting for your approval",
  "chat.waiting.short": "Waiting",
  "chat.approval.heading": "needs your approval",
  "chat.approval.reason.firstUse": "This is the first time {name} uses your Computer.",
  "chat.approval.reason.rule": "A rule you set asks before this action.",
  "chat.approval.reason.ambiguous": "This action has a form no rule can judge.",
  "chat.approval.reason.grant": "This action writes outside the folders you granted.",
  "chat.approval.undo.none": "This action cannot be undone.",
  "chat.approval.undo.unknown": "It is not known whether this action can be undone.",
  "chat.approval.lastingScope": "Always allow covers: {scope}",
  "chat.approval.scope.shell": "shell commands starting with \"{command}\"",
  "chat.approval.scope.files": "files under \"{path}\"",
  "chat.approval.scope.root": "files in the Workspace root",
  "chat.approval.showFull": "Show full action",
  "chat.approval.showLess": "Show less",
  "chat.approval.allowOnce": "Allow once",
  "chat.approval.alwaysAllow": "Always allow",
  "chat.approval.deny": "Deny",
  "chat.approval.allowed": "Allowed",
  "chat.approval.denied": "Denied",

  "select.title": "Select a Chat to open it.",
  "boot.opening": "Opening the Space…",

  "format.today": "Today",
  "locale.switch.to.zh": "Switch to Chinese",
  "locale.switch.to.en": "Switch to English",
  "locale.name.zh": "中文",
  "locale.name.en": "English",
} as const;

type MessageKey = keyof typeof en;

const zh: Record<MessageKey, string> = {
  "auth.headline": "把工作交给一支你看得见的团队。",
  "auth.lede":
    "Bot 是队友。它有自己的 Computer、记忆和身份。工作在 Chat 中沉淀，权限始终由你掌握。",
  "auth.point.space": "Space 只在这台机器上",
  "auth.point.provider": "你的 Provider，你的 Model",
  "auth.point.computer": "每个 Bot 都有一个 Computer",
  "auth.missing.title": "打开完整邀请链接",
  "auth.missing.lead": "链接由",
  "auth.missing.mid": "打印，密钥位于",
  "auth.missing.tail": " 之后。请把完整链接粘贴到这个浏览器。",
  "auth.join.title": "加入这个 Space",
  "auth.join.hint": "成为这个 Space 的管理员。邀请只在 Space 运行期间有效，并且只能使用一次。",
  "auth.login.title": "登录 Neha",
  "auth.login.hint": "使用已经属于这个 Space 的账号。",
  "auth.name": "显示名称",
  "auth.password": "密码",
  "auth.password.hint": "至少 8 个字符。一个短语就够了。",
  "auth.join.action": "加入 Space",
  "auth.login.action": "登录",
  "auth.working": "处理中…",
  "auth.fine": "Space 就在这台机器上。你的会话保存在这个浏览器里。",

  "error.invitation": "这个邀请不可用。请启动新的 Space 或联系管理员。",
  "error.credentials": "显示名称或密码不正确。",
  "error.rate": "登录尝试过多。请等待几分钟。",
  "error.conflict": "这个 Space 已经有管理员。请改用登录。",
  "error.unreachable": "浏览器无法连接 Space。neha serve 还在运行吗？",
  "error.generic": "Space 无法完成这个请求。",
  "error.session": "会话已结束。请刷新页面并重新登录。",
  "error.record": "这条记录已不存在。",
  "error.botAnswer": "Bot 无法回答。",
  "error.password": "密码过短。请使用至少 8 个字符。",
  "error.invalid": "这个值不符合要求。",
  "error.upload": "文件上传失败，请重试。",
  "error.fileTooLarge": "{name} 超过 {limit}。",

  "sidebar.chats": "Chats",
  "sidebar.chats.one": "1 个 Chat",
  "sidebar.chats.many": "{count} 个 Chat",
  "sidebar.empty.title": "还没有 Chat",
  "sidebar.empty.text": "创建一个 Bot，它的 Chat 会出现在这里。",
  "sidebar.startChat": "和 {name} 打个招呼",
  "sidebar.newChat": "新 Chat",
  "sidebar.openChat": "打开 Chat",
  "sidebar.file": "已发送文件",
  "sidebar.online": "Space 在线",

  "rail.chats": "Chats",
  "rail.admin": "管理员",
  "rail.user": "成员",
  "rail.primary": "主导航",

  "setup.step.one": "第 1 步，共 2 步",
  "setup.step.two": "第 2 步，共 2 步",
  "setup.provider.title": "连接一个 Provider",
  "setup.provider.lede": "这个 Space 里的每个 Bot 都通过 Provider 思考。密钥留在 Space 内。",
  "setup.bot.title": "创建你的第一个 Bot",
  "setup.bot.lede": "Bot 有自己的身份、记忆和 Computer。给它一个名字，然后把工作交给它。",
  "setup.bot.panelTitle": "给队友起个名字",
  "setup.provider.connected.one": "Provider 已连接",
  "setup.provider.connected.many": "已连接 {count} 个 Provider",
  "setup.ready": "就绪",
  "setup.models": "Model",
  "setup.none": "无",
  "setup.bot.name": "名称",
  "setup.bot.nameHint": "名称会出现在 Chat 中。",
  "setup.bot.accessTitle": "仅所有者",
  "setup.bot.accessHint": "只有你能使用这个 Bot。",
  "setup.bot.create": "创建 Bot",
  "setup.bot.creating": "创建中…",
  "setup.provider.panelTitle": "兼容 OpenAI 的 Provider",
  "setup.adminOnly": "仅管理员",
  "setup.baseUrl": "基础 URL",
  "setup.baseUrl.hint": "提供 OpenAI Chat Completions 的地址。",
  "setup.apiKey": "API 密钥",
  "setup.apiKey.hint": "在这个 Space 中加密保存，之后无法读取。",
  "setup.model": "Model 名称",
  "setup.model.hint": "Provider 期望的精确名称。",
  "setup.advanced": "高级",
  "setup.contextLimit": "上下文上限",
  "setup.maxOutput": "最大输出",
  "setup.connect": "连接 Provider",
  "setup.connecting": "连接中…",
  "setup.provider.note": "Provider 属于这个 Space，所有 Chat 共用。",
  "setup.noAdmin": "请 Space 管理员先连接 Provider，Bot 才能工作。",

  "chat.title": "Chat",
  "chat.aria": "与 {name} 的 Chat",
  "chat.loading": "加载中…",
  "chat.localComputer": "Local Computer",
  "chat.hostedComputer": "Hosted Computer",
  "chat.online": "在线",
  "chat.offline": "离线",
  "chat.back": "返回 Chat 列表",
  "chat.new.title": "和 {name} 打个招呼。",
  "chat.opening": "正在打开 Chat…",
  "chat.new.text": "用你自己的话把工作交给 Bot。它在这里回答，Chat 保留记录。",
  "chat.suggestion.hello": "打个招呼",
  "chat.suggestion.work": "你在忙什么？",
  "chat.suggestion.plan": "帮我规划下一步。",
  "chat.acting": "为你工作",
  "chat.working": "正在处理",
  "chat.queue.queued": "排队中",
  "chat.queue.waiting.one": "前面还有 1 个请求，排队中",
  "chat.queue.waiting.many": "前面还有 {count} 个请求，排队中",
  "chat.placeholder": "给 {name} 发消息…",
  "chat.placeholder.empty": "给 Bot 发消息…",
  "chat.hint": "Enter 发送 · Shift+Enter 换行",
  "chat.send": "发送消息",
  "chat.attach": "添加附件",
  "chat.attachment.remove": "移除 {name}",
  "chat.file.download": "下载 {name}",
  "chat.waiting": "等待你的审批",
  "chat.waiting.short": "等待中",
  "chat.approval.heading": "需要你确认",
  "chat.approval.reason.firstUse": "这是 {name} 第一次使用你的 Computer。",
  "chat.approval.reason.rule": "你设置的规则要求先确认这个动作。",
  "chat.approval.reason.ambiguous": "规则无法判断这个操作的形式。",
  "chat.approval.reason.grant": "这个动作要写你尚未授权的文件夹。",
  "chat.approval.undo.none": "这个操作无法撤销。",
  "chat.approval.undo.unknown": "这个操作能否撤销尚不清楚。",
  "chat.approval.lastingScope": "始终允许的范围：{scope}",
  "chat.approval.scope.shell": "以 \"{command}\" 开头的 shell 命令",
  "chat.approval.scope.files": "\"{path}\" 下的文件",
  "chat.approval.scope.root": "Workspace 根目录下的文件",
  "chat.approval.showFull": "显示完整动作",
  "chat.approval.showLess": "收起",
  "chat.approval.allowOnce": "允许一次",
  "chat.approval.alwaysAllow": "总是允许",
  "chat.approval.deny": "拒绝",
  "chat.approval.allowed": "已允许",
  "chat.approval.denied": "已拒绝",

  "select.title": "选择一个 Chat 打开。",
  "boot.opening": "正在打开 Space…",

  "format.today": "今天",
  "locale.switch.to.zh": "切换到中文",
  "locale.switch.to.en": "切换到英文",
  "locale.name.zh": "中文",
  "locale.name.en": "English",
};

const STORAGE_KEY = "neha.locale";

function storedLocale(): Locale | null {
  try {
    const value = localStorage.getItem(STORAGE_KEY);
    return value === "en-US" || value === "zh-CN" ? value : null;
  } catch {
    return null;
  }
}

function initialLocale(): Locale {
  const stored = storedLocale();
  if (stored) return stored;
  const language = typeof navigator === "undefined" ? "" : navigator.language;
  return language.toLowerCase().startsWith("zh") ? "zh-CN" : "en-US";
}

const [locale, setLocaleSignal] = createSignal<Locale>(initialLocale());

function applyLanguage(value: Locale) {
  if (typeof document !== "undefined") {
    document.documentElement.lang = value;
  }
}

applyLanguage(locale());

export { locale };

export function setLocale(value: Locale) {
  setLocaleSignal(value);
  applyLanguage(value);
  try {
    localStorage.setItem(STORAGE_KEY, value);
  } catch {
    // A browser that blocks storage still switches for this page.
  }
}

export function toggleLocale() {
  setLocale(locale() === "en-US" ? "zh-CN" : "en-US");
}

function interpolate(template: string, params?: Record<string, string | number>): string {
  if (!params) return template;
  return template.replace(/\{(\w+)\}/g, (match, name: string) =>
    name in params ? String(params[name]) : match,
  );
}

export function t(
  key: MessageKey,
  params?: Record<string, string | number>,
  language: Locale = locale(),
): string {
  return interpolate(language === "zh-CN" ? zh[key] : en[key], params);
}

export const catalogs = { "en-US": en, "zh-CN": zh } as const;

export type { MessageKey };
