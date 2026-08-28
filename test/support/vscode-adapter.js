"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const http = require("node:http");
const net = require("node:net");
const path = require("node:path");

const extensionPath = process.argv[2];
const root = process.argv[3];
if (!extensionPath || !root) throw new Error("usage: vscode-adapter.js EXTENSION ROOT");

const commands = [];
const listeners = [];
const environment = new Map();
const activeTerminal = { processId: Promise.resolve(4242) };
const vscodeApi = {
  commands: {
    async executeCommand(command, ...values) {
      commands.push([command, ...values]);
    },
  },
  window: {
    activeTerminal,
    state: { focused: true },
    onDidChangeActiveTerminal(callback) {
      listeners.push(["terminal", callback]);
      return { dispose() {} };
    },
    onDidCloseTerminal(callback) {
      listeners.push(["close", callback]);
      return { dispose() {} };
    },
    onDidChangeWindowState(callback) {
      listeners.push(["window", callback]);
      return { dispose() {} };
    },
  },
};
global.__termnavVscode = vscodeApi;
const extension = require(extensionPath);

function message(token, overrides = {}) {
  return {
    version: 2,
    source: "nvim-test",
    cycle: 1,
    sequence: 1,
    observed: 1000,
    operation: "claim",
    token,
    ancestors: [4242, 1],
    ...overrides,
  };
}

async function request(socketPath, route, body) {
  const payload = Buffer.from(JSON.stringify(body));
  return new Promise((resolve, reject) => {
    const request = http.request(
      {
        socketPath,
        path: route,
        method: "POST",
        headers: {
          "content-type": "application/json",
          "content-length": payload.length,
        },
      },
      (response) => {
        const chunks = [];
        response.on("data", (chunk) => chunks.push(chunk));
        response.on("end", () => {
          resolve({ status: response.statusCode, body: Buffer.concat(chunks).toString("utf8") });
        });
      },
    );
    request.on("error", reject);
    request.end(payload);
  });
}

async function waitFor(predicate, description, timeoutMs = 3500) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error(`timed out waiting for ${description}`);
}

async function controllerBehavior() {
  const token = "a".repeat(64);
  let now = 10;
  let nextTimer = 0;
  const timers = new Map();
  const controller = extension.createFocusController({
    vscodeApi,
    token,
    leaseMs: 50,
    now: () => now,
    observedNow: () => 1000,
    setTimer(callback) {
      nextTimer += 1;
      timers.set(nextTimer, callback);
      return nextTimer;
    },
    clearTimer(timer) {
      timers.delete(timer);
    },
  });

  await controller.initialize();
  assert.deepEqual(commands.at(-1), ["setContext", "termnav.nvimFocused", false]);
  assert.equal(await controller.handle(message("b".repeat(64))), false, "wrong token accepted");
  assert.equal(await controller.handle(message(token)), true);
  assert.deepEqual(commands.at(-1), ["setContext", "termnav.nvimFocused", true]);
  assert.equal(
    await controller.handle(message(token, { sequence: 0 })),
    false,
    "stale source sequence accepted",
  );

  const lease = [...timers.values()].at(-1);
  assert.ok(lease, "focus lease was not armed");
  now += 50;
  lease();
  await controller.flush();
  assert.deepEqual(commands.at(-1), ["setContext", "termnav.nvimFocused", false]);

  now += 1;
  assert.equal(await controller.handle(message(token, { cycle: 2, observed: 1001 })), true);
  vscodeApi.window.state.focused = false;
  await controller.windowStateChanged({ focused: false });
  assert.deepEqual(commands.at(-1), ["setContext", "termnav.nvimFocused", false]);
  vscodeApi.window.state.focused = true;
  await controller.dispose();
}

async function serverBehavior() {
  const token = "c".repeat(64);
  const socketPath = path.join(root, "adapter.sock");
  const server = extension.createServer(null, token);
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(socketPath, resolve);
  });
  try {
    const denied = await request(socketPath, "/switch-tab", {
      token: "d".repeat(64),
      direction: "next",
    });
    assert.equal(denied.status, 403);
    const accepted = await request(socketPath, "/switch-tab", { token, direction: "next" });
    assert.equal(accepted.status, 200);
    assert.deepEqual(commands.at(-1), ["workbench.action.terminal.focusNext"]);

    let slowClosed = false;
    const slow = net.createConnection(socketPath);
    slow.once("close", () => {
      slowClosed = true;
    });
    await new Promise((resolve, reject) => {
      slow.once("connect", resolve);
      slow.once("error", reject);
    });
    await waitFor(() => slowClosed, "idle adapter connection timeout");
  } finally {
    await new Promise((resolve) => server.close(resolve));
  }
}

async function activationCleanup() {
  const collection = {
    persistent: true,
    replace(name, value) {
      environment.set(name, value);
    },
    delete(name) {
      environment.delete(name);
    },
  };
  const context = { environmentVariableCollection: collection, subscriptions: [] };
  await extension.activate(context);
  const socketPath = environment.get("TERMNAV_VSCODE_SOCKET");
  assert.match(environment.get("TERMNAV_VSCODE_TOKEN"), /^[0-9a-f]{64}$/);
  assert.ok(fs.lstatSync(socketPath).isSymbolicLink(), "published socket is not a symlink");
  assert.equal(collection.persistent, false);
  assert.equal(listeners.length, 3);
  await context.subscriptions.at(-1).dispose();
  assert.equal(environment.has("TERMNAV_VSCODE_SOCKET"), false);
  assert.equal(environment.has("TERMNAV_VSCODE_TOKEN"), false);
  assert.equal(fs.existsSync(socketPath), false, "published socket survived adapter disposal");
}

async function main() {
  assert.equal(extension.validateFocusMessage(message("a".repeat(64))), true);
  assert.equal(extension.validateFocusMessage(message("short")), false);
  assert.equal(
    extension.validateFocusMessage(message("a".repeat(64), { ancestors: [4242, 4242] })),
    false,
  );
  assert.ok(extension.socketLocation().socketPath.startsWith("/tmp/termnav-vscode-"));
  await controllerBehavior();
  await serverBehavior();
  await activationCleanup();
  console.log("vscode-adapter: production controller, server, and lifecycle pass");
}

main().catch((error) => {
  console.error(error.stack || error);
  process.exitCode = 1;
});
