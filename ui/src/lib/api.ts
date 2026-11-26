import type { Account, ApiErrorBody } from "../types";

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
};
