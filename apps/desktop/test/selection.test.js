const assert = require("node:assert/strict");
const { test } = require("node:test");

const { normalizeRemoteUrl } = require("../src/selection");

test("accepts an https Space and keeps its origin", () => {
  assert.equal(normalizeRemoteUrl(" https://space.example.com/chat "), "https://space.example.com");
});

test("accepts loopback http for local development", () => {
  assert.equal(normalizeRemoteUrl("http://127.0.0.1:7337"), "http://127.0.0.1:7337");
  assert.equal(normalizeRemoteUrl("http://neha-abc.localhost:7337"), "http://neha-abc.localhost:7337");
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
