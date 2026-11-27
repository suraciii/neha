import type {
  Account,
  ApiErrorBody,
  FileSummary,
  PushMute,
  PushStatus,
  PushSubscriptionBody,
} from "../types";

export class ApiError extends Error {
  readonly status: number;
  readonly code: string;

  constructor(status: number, code: string, message: string) {
    super(message);
    this.name = "ApiError";
    this.status = status;
    this.code = code;
  }
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(path, {
    credentials: "same-origin",
    headers: { "content-type": "application/json", ...(init?.headers ?? {}) },
    ...init,
  });
  if (!response.ok) {
    let body: ApiErrorBody = { code: "http_error", message: `Request failed (${response.status})` };
    try {
      body = (await response.json()) as ApiErrorBody;
    } catch {
      // The auth endpoints can answer with a plain-text rejection.
    }
    throw new ApiError(response.status, body.code, body.message);
  }
  return (await response.json()) as T;
}

export const api = {
  me: () => request<Account>("/api/me"),
  join: async (secret: string, displayName: string, password: string): Promise<Account> =>
    request<Account>("/api/join", {
      method: "POST",
      body: JSON.stringify({ secret, display_name: displayName, password }),
    }),
  login: async (displayName: string, password: string): Promise<Account> =>
    request<Account>("/api/login", {
      method: "POST",
      body: JSON.stringify({ display_name: displayName, password }),
    }),
  // The bytes are the body; the name and media type ride in the query.
  uploadFile: async (chat: string, file: Blob, name: string, mediaType: string): Promise<FileSummary> =>
    request<FileSummary>(
      `/api/chats/${encodeURIComponent(chat)}/files?${new URLSearchParams({ name, media_type: mediaType })}`,
      { method: "POST", body: file, headers: { "content-type": mediaType } },
    ),
  pushStatus: () => request<PushStatus>("/api/push"),
  subscribePush: (body: PushSubscriptionBody) =>
    request<{ subscribed: boolean }>("/api/push/subscriptions", {
      method: "POST",
      body: JSON.stringify(body),
    }),
  unsubscribePush: (endpoint: string) =>
    request<{ subscribed: boolean }>("/api/push/subscriptions", {
      method: "DELETE",
      body: JSON.stringify({ endpoint }),
    }),
  mutePush: (minutes: number) =>
    request<PushMute>("/api/push/mute", {
      method: "POST",
      body: JSON.stringify({ minutes }),
    }),
  unmutePush: () => request<PushMute>("/api/push/unmute", { method: "POST" }),
};
