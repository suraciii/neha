const assert = require("node:assert/strict");
const { test } = require("node:test");

const { parseStartupLine } = require("../src/startup");

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
