const assert = require("node:assert/strict");
const { test } = require("node:test");

const { normalizeRemoteUrl, probeUrl, settledRemoteUrl } = require("../src/selection");

test("accepts an https Space and keeps its path", () => {
  assert.equal(
    normalizeRemoteUrl(" https://space.example.com/chat "),
    "https://space.example.com/chat",
  );
});

test("accepts loopback http for local development", () => {
  assert.equal(normalizeRemoteUrl("http://127.0.0.1:7337"), "http://127.0.0.1:7337/");
  assert.equal(normalizeRemoteUrl("http://neha-abc.localhost:7337"), "http://neha-abc.localhost:7337/");
});

test("refuses plaintext remote Spaces", () => {
  assert.throws(() => normalizeRemoteUrl("http://space.example.com"), /https/);
});

test("refuses addresses carrying credentials", () => {
  assert.throws(() => normalizeRemoteUrl("https://user:secret@space.example.com"), /credentials/);
});

test("refuses values that are not URLs", () => {
  assert.throws(() => normalizeRemoteUrl("space.example.com"), /address/);
});

test("settles a consumed invite against the canonical redirected origin", () => {
  const selection = {
    mode: "remote",
    spaceUrl: "http://127.0.0.1:7337/invite#secret",
  };
  assert.equal(
    settledRemoteUrl(
      selection,
      "http://neha-abc.localhost:7337",
      "http://neha-abc.localhost:7337/",
    ),
    "http://neha-abc.localhost:7337/",
  );
  assert.equal(
    settledRemoteUrl(
      selection,
      "http://neha-abc.localhost:7337",
      "http://neha-abc.localhost:7337/invite#secret",
    ),
    null,
  );
});

test("probes a localhost Space through loopback", () => {
  assert.equal(
    probeUrl("http://neha-abc.localhost:7337"),
    "http://127.0.0.1:7337/",
  );
  assert.equal(
    probeUrl("https://space.example.com"),
    "https://space.example.com/",
  );
});
