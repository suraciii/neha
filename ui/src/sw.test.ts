import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { runInNewContext } from "node:vm";

import { describe, expect, it, vi } from "vitest";

const SHELL = "neha-shell-v4";
const SHELL_HTML =
  '<!doctype html><div id="root"></div><script src="/assets/app.js"></script>';
const LAST_GOOD = "<!doctype html><p>last good shell</p>";

type Handler = (event: {
  request?: {
    method: string;
    url: string;
    mode: string;
    headers: Headers;
  };
  respondWith?: (promise: Promise<Response>) => void;
  waitUntil?: (promise: Promise<unknown>) => void;
}) => void;

function fakeWorker(fetchImpl: (input: unknown, init?: RequestInit) => Promise<Response>) {
  const handlers = new Map<string, Handler>();
  const stores = new Map<string, Map<string, Response>>();
  const keyOf = (key: unknown) =>
    new URL(
      typeof key === "string" ? key : (key as { url: string }).url,
      "https://space.example.com",
    ).href;
  const cacheFor = (name: string) => {
    let store = stores.get(name);
    if (!store) {
      store = new Map();
      stores.set(name, store);
    }
    return {
      put: async (key: unknown, response: Response) => {
        store!.set(keyOf(key), response.clone());
      },
      add: async (key: string) => {
        const response = await fetchImpl(key);
        if (!response.ok) throw new Error(`cannot add ${key}`);
        store!.set(keyOf(key), response.clone());
      },
      keys: async () => [...store!.keys()].map((url) => ({ url })),
      match: async (key: unknown) => store!.get(keyOf(key))?.clone(),
    };
  };
  const cacheApi = {
    open: async (name: string) => cacheFor(name),
    match: async (key: unknown) => {
      for (const name of stores.keys()) {
        const hit = await cacheFor(name).match(key);
        if (hit) return hit;
      }
      return undefined;
    },
    keys: async () => [...stores.keys()],
    delete: async (name: string) => stores.delete(name),
  };
  const fetchMock = vi.fn(fetchImpl);
  const self = {
    addEventListener: (type: string, handler: Handler) => handlers.set(type, handler),
    skipWaiting: async () => undefined,
    clients: { claim: async () => undefined },
  };

  runInNewContext(readFileSync(resolve(process.cwd(), "public", "sw.js"), "utf8"), {
    self,
    caches: cacheApi,
    fetch: fetchMock,
    Response,
    URL,
    location: { origin: "https://space.example.com" },
    console,
  });

  return { handlers, cacheFor, cacheApi, fetchMock };
}

async function seed(worker: ReturnType<typeof fakeWorker>) {
  await worker
    .cacheFor(SHELL)
    .put("/", new Response(LAST_GOOD, { status: 200, headers: { "content-type": "text/html" } }));
}

async function goodFetch(input: unknown): Promise<Response> {
  if (String(input).startsWith("/assets/")) {
    return new Response("console.log(1)", {
      status: 200,
      headers: { "content-type": "text/javascript" },
    });
  }
  return new Response(SHELL_HTML, {
    status: 200,
    headers: { "content-type": "text/html" },
  });
}

function navigate(handlers: Map<string, Handler>, path = "/"): Promise<Response> {
  let answered: Promise<Response> | undefined;
  handlers.get("fetch")?.({
    request: {
      method: "GET",
      url: `https://space.example.com${path}`,
      mode: "navigate",
      headers: new Headers(),
    },
    respondWith: (promise) => {
      answered = promise;
    },
  });
  if (!answered) throw new Error("the worker did not answer the navigation");
  return answered;
}

function resource(
  handlers: Map<string, Handler>,
  path: string,
  mode = "cors",
): Promise<Response> {
  let answered: Promise<Response> | undefined;
  handlers.get("fetch")?.({
    request: { method: "GET", url: `https://space.example.com${path}`, mode, headers: new Headers() },
    respondWith: (promise) => {
      answered = promise;
    },
  });
  if (!answered) throw new Error("the worker did not answer the request");
  return answered;
}

describe("the installed shell worker", () => {
  it("keeps the last good shell when the proxy returns an error", async () => {
    const worker = fakeWorker(
      async () =>
        new Response("bad gateway", {
          status: 502,
          headers: { "content-type": "text/html" },
        }),
    );
    await seed(worker);

    const response = await navigate(worker.handlers, "/invite");
    expect(await response.text()).toBe(LAST_GOOD);
    expect(await (await worker.cacheFor(SHELL).match("/"))!.text()).toBe(LAST_GOOD);
  });

  it("publishes a verified shell", async () => {
    const worker = fakeWorker(goodFetch);

    const response = await navigate(worker.handlers);
    expect(await response.text()).toContain('id="root"');
    expect(await (await worker.cacheFor(SHELL).match("/"))!.text()).toContain('id="root"');
  });

  it("does not publish a successful page that is not the app", async () => {
    const worker = fakeWorker(
      async () =>
        new Response("<html><p>maintenance</p></html>", {
          status: 200,
          headers: { "content-type": "text/html" },
        }),
    );
    await seed(worker);

    const response = await navigate(worker.handlers);
    expect(await response.text()).toBe(LAST_GOOD);
    expect(await (await worker.cacheFor(SHELL).match("/"))!.text()).toBe(LAST_GOOD);
  });

  it("falls back to the shell when the network is down", async () => {
    const worker = fakeWorker(async () => {
      throw new TypeError("offline");
    });
    await seed(worker);

    const response = await navigate(worker.handlers);
    expect(await response.text()).toBe(LAST_GOOD);
  });

  it("leaves the API to the Space", () => {
    const worker = fakeWorker(async () => new Response("not used"));
    let answered = false;
    worker.handlers.get("fetch")?.({
      request: {
        method: "GET",
        url: "https://space.example.com/api/me",
        mode: "cors",
        headers: new Headers(),
      },
      respondWith: () => {
        answered = true;
      },
    });
    expect(answered).toBe(false);
  });

  it("revalidates a stable shell resource", async () => {
    const worker = fakeWorker(async () => new Response("new manifest"));
    await worker.cacheFor(SHELL).put("/manifest.webmanifest", new Response("old manifest"));

    const response = await resource(worker.handlers, "/manifest.webmanifest");
    expect(await response.text()).toBe("new manifest");
    expect(
      await (await worker.cacheFor(SHELL).match("/manifest.webmanifest"))!.text(),
    ).toBe("new manifest");
    expect(worker.fetchMock.mock.calls.at(-1)?.[1]).toEqual({ cache: "no-cache" });
  });

  it("serves a content-named asset from the cache first", async () => {
    const worker = fakeWorker(async () => new Response("new asset"));
    await worker.cacheFor(SHELL).put("/assets/app.js", new Response("old asset"));

    const response = await resource(worker.handlers, "/assets/app.js");
    expect(await response.text()).toBe("old asset");
    expect(worker.fetchMock).not.toHaveBeenCalled();
  });

  it("does not install an error page as the shell", async () => {
    const worker = fakeWorker(async (input) => {
      if (input === "/") {
        return new Response("bad gateway", {
          status: 502,
          headers: { "content-type": "text/html" },
        });
      }
      return new Response("ok");
    });

    let installed: Promise<unknown> | undefined;
    worker.handlers.get("install")?.({
      waitUntil: (promise) => {
        installed = promise;
      },
    });
    await expect(installed!).rejects.toThrow();

    expect(await worker.cacheFor(SHELL).match("/")).toBeUndefined();
  });

  it("does not install a shell whose asset is an error page", async () => {
    const worker = fakeWorker(async (input) => {
      if (input === "/") {
        return new Response(SHELL_HTML, {
          status: 200,
          headers: { "content-type": "text/html" },
        });
      }
      if (String(input).startsWith("/assets/")) {
        return new Response("maintenance", {
          status: 200,
          headers: { "content-type": "text/html" },
        });
      }
      return new Response("ok");
    });

    let installed: Promise<unknown> | undefined;
    worker.handlers.get("install")?.({
      waitUntil: (promise) => {
        installed = promise;
      },
    });
    await expect(installed!).rejects.toThrow();

    expect(await worker.cacheFor(SHELL).match("/")).toBeUndefined();
  });

  it("keeps a working older shell through a failed update and an offline open", async () => {
    const oldCache = "neha-shell-v3";
    const worker = fakeWorker(async (input) => {
      if (input === "/") {
        return new Response(SHELL_HTML, {
          status: 200,
          headers: { "content-type": "text/html" },
        });
      }
      if (String(input).startsWith("/assets/")) {
        return new Response("missing", { status: 500 });
      }
      return new Response("ok");
    });
    await worker
      .cacheFor(oldCache)
      .put("/", new Response(LAST_GOOD, { status: 200, headers: { "content-type": "text/html" } }));

    let installed: Promise<unknown> | undefined;
    worker.handlers.get("install")?.({
      waitUntil: (promise) => {
        installed = promise;
      },
    });
    await expect(installed!).rejects.toThrow();

    // The failed update never activates, so the older cache is still there.
    expect(await worker.cacheFor(SHELL).match("/")).toBeUndefined();
    expect(await (await worker.cacheFor(oldCache).match("/"))!.text()).toBe(LAST_GOOD);

    // Even if activation runs, a worker with no published shell must not
    // delete the last working cache.
    let activated: Promise<unknown> | undefined;
    worker.handlers.get("activate")?.({
      waitUntil: (promise) => {
        activated = promise;
      },
    });
    await activated;
    expect(await (await worker.cacheFor(oldCache).match("/"))!.text()).toBe(LAST_GOOD);

    // An offline open still has a shell to show.
    worker.fetchMock.mockRejectedValue(new TypeError("offline"));
    const response = await navigate(worker.handlers);
    expect(await response.text()).toBe(LAST_GOOD);
  });

  it("serves the last complete shell when a candidate's asset fails", async () => {
    const worker = fakeWorker(async (input) => {
      if (String(input).startsWith("/assets/")) {
        return new Response("missing", { status: 500 });
      }
      return new Response(SHELL_HTML, {
        status: 200,
        headers: { "content-type": "text/html" },
      });
    });
    await seed(worker);

    const response = await navigate(worker.handlers);
    expect(await response.text()).toBe(LAST_GOOD);
    expect(await (await worker.cacheFor(SHELL).match("/"))!.text()).toBe(LAST_GOOD);
  });

  it("does not publish a page with no entry script", async () => {
    const worker = fakeWorker(
      async () =>
        new Response(
          '<!doctype html><div id="root"></div><link href="/assets/app.css" rel="stylesheet">',
          { status: 200, headers: { "content-type": "text/html" } },
        ),
    );
    await seed(worker);

    const response = await navigate(worker.handlers);
    expect(await response.text()).toBe(LAST_GOOD);
    expect(await (await worker.cacheFor(SHELL).match("/"))!.text()).toBe(LAST_GOOD);
  });

  it("drops an older shell cache on activation", async () => {
    const worker = fakeWorker(async () => new Response("ok"));
    await worker.cacheFor("neha-shell-v3").put("/", new Response("old"));
    await worker.cacheFor(SHELL).put("/", new Response("current"));

    let activated: Promise<unknown> | undefined;
    worker.handlers.get("activate")?.({
      waitUntil: (promise) => {
        activated = promise;
      },
    });
    await activated;

    expect(await worker.cacheApi.keys()).toEqual([SHELL]);
  });
});
