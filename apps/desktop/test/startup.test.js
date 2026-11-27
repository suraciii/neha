const assert = require("node:assert/strict");
const { test } = require("node:test");

const {
  cookiePair,
  defaultDisplayName,
  generatePassword,
  parseStartupLine,
} = require("../src/startup");

test("parses the Space's startup lines", () => {
  assert.deepEqual(parseStartupLine("Neha is listening on http://127.0.0.1:4100"), {
    listening: "http://127.0.0.1:4100",
  });
  assert.deepEqual(parseStartupLine("Invite: http://127.0.0.1:4100/invite#secret"), {
    invite: "http://127.0.0.1:4100/invite#secret",
  });
  assert.deepEqual(
    parseStartupLine("Open in a browser: http://neha-abc.localhost:4100/invite#secret"),
    { browser: "http://neha-abc.localhost:4100/invite#secret" },
  );
  assert.equal(parseStartupLine("Press Ctrl-C to stop."), null);
});

test("reads the session cookie name and value from Set-Cookie", () => {
  assert.deepEqual(cookiePair("neha_session_ab=token; Path=/; HttpOnly; SameSite=Strict"), {
    name: "neha_session_ab",
    value: "token",
  });
  assert.equal(cookiePair("nonsense"), null);
  assert.equal(cookiePair("=token"), null);
});

test("uses the local account as a display name and caps it", () => {
  assert.equal(defaultDisplayName("ada"), "ada");
  assert.equal(defaultDisplayName("  "), "Admin");
  assert.equal(defaultDisplayName(undefined), "Admin");
  assert.equal(defaultDisplayName("x".repeat(80)).length, 64);
});

test("generates a password the Space accepts", () => {
  const first = generatePassword();
  const second = generatePassword();
  assert.ok(first.length >= 8);
  assert.notEqual(first, second);
  assert.match(first, /^[A-Za-z0-9_-]+$/);
});
