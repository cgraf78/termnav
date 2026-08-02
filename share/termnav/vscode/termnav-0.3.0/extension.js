const fs = require("node:fs");
const http = require("node:http");
const crypto = require("node:crypto");
const path = require("node:path");
const vscode = require("vscode");

const SOCKET_ENVIRONMENT_KEY = "TERMNAV_VSCODE_SOCKET";
const TOKEN_ENVIRONMENT_KEY = "TERMNAV_VSCODE_TOKEN";
const NVIM_FOCUS_CONTEXT = "termnav.nvimFocused";
const DEFAULT_FOCUS_LEASE_MS = 3000;
const MAX_OBSERVATION_SKEW_MS = 10000;
const MAX_TRACKED_SOURCES = 256;
const MAX_ANCESTORS = 64;
const MAX_BODY_BYTES = 1024;
const TOKEN_PATTERN = /^[0-9a-f]{64}$/;
const TAB_COMMANDS = new Map([
  ["next", "workbench.action.terminal.focusNext"],
  ["previous", "workbench.action.terminal.focusPrevious"],
]);
const SERVER_CONNECTIONS = new WeakMap();
let activeBridge;

function positiveSafeInteger(value) {
  return Number.isSafeInteger(value) && value > 0;
}

function nonnegativeSafeInteger(value) {
  return Number.isSafeInteger(value) && value >= 0;
}

function validateFocusMessage(body) {
  if (!body || typeof body !== "object" || Array.isArray(body)) return false;
  if (body.version !== 2) return false;
  if (typeof body.source !== "string" || !/^[A-Za-z0-9_.:-]{1,128}$/.test(body.source)) {
    return false;
  }
  if (!nonnegativeSafeInteger(body.cycle) || !nonnegativeSafeInteger(body.sequence)) {
    return false;
  }
  if (!nonnegativeSafeInteger(body.observed) || !TOKEN_PATTERN.test(body.token)) {
    return false;
  }
  if (!Array.isArray(body.ancestors) || body.ancestors.length === 0) return false;
  if (body.ancestors.length > MAX_ANCESTORS) return false;
  const ancestors = new Set();
  for (const pid of body.ancestors) {
    if (!positiveSafeInteger(pid) || ancestors.has(pid)) return false;
    ancestors.add(pid);
  }
  return body.operation === "claim" || body.operation === "release";
}

function tokensEqual(actual, expected) {
  if (!TOKEN_PATTERN.test(actual) || !TOKEN_PATTERN.test(expected)) return false;
  return crypto.timingSafeEqual(Buffer.from(actual, "hex"), Buffer.from(expected, "hex"));
}

function compareSourcePosition(message, position) {
  if (!position) return 1;
  if (message.cycle !== position.cycle) return message.cycle > position.cycle ? 1 : -1;
  if (message.sequence !== position.sequence) {
    return message.sequence > position.sequence ? 1 : -1;
  }
  return 0;
}

function createFocusController(options = {}) {
  const vscodeApi = options.vscodeApi || vscode;
  const leaseMs = options.leaseMs || DEFAULT_FOCUS_LEASE_MS;
  const now = options.now || Date.now;
  const observedNow = options.observedNow || (() => Number(process.hrtime.bigint() / 1000000n));
  const setTimer = options.setTimer || setTimeout;
  const clearTimer = options.clearTimer || clearTimeout;
  const token = options.token;
  if (!TOKEN_PATTERN.test(token)) throw new Error("termnav: invalid window token");
  const latestBySource = new Map();
  let activeTerminal = vscodeApi.window.activeTerminal;
  let generation = 0;
  let invalidatedAt = -1;
  let latestObserved = -1;
  let owner;
  let leaseTimer;
  let leaseRevision = 0;
  let publishChain = Promise.resolve();

  function publish(value) {
    const operation = publishChain.then(() =>
      vscodeApi.commands.executeCommand("setContext", NVIM_FOCUS_CONTEXT, value),
    );
    publishChain = operation.catch(() => {});
    return operation;
  }

  function rememberSource(message) {
    latestBySource.delete(message.source);
    latestBySource.set(message.source, {
      cycle: message.cycle,
      sequence: message.sequence,
    });
    while (latestBySource.size > MAX_TRACKED_SOURCES) {
      latestBySource.delete(latestBySource.keys().next().value);
    }
  }

  function cancelLease() {
    leaseRevision += 1;
    if (leaseTimer !== undefined) {
      clearTimer(leaseTimer);
      leaseTimer = undefined;
    }
  }

  function clearOwner() {
    cancelLease();
    owner = undefined;
    return publish(false);
  }

  function armLease() {
    cancelLease();
    const revision = leaseRevision;
    const expiresAt = owner.expiresAt;
    const expire = () => {
      if (!owner || revision !== leaseRevision) return;
      const remaining = expiresAt - now();
      if (remaining > 0) {
        leaseTimer = setTimer(expire, remaining);
        return;
      }
      owner = undefined;
      leaseTimer = undefined;
      publish(false).catch(() => {});
    };
    leaseTimer = setTimer(expire, Math.max(0, expiresAt - now()));
  }

  async function initialize() {
    owner = undefined;
    generation += 1;
    await clearOwner();
  }

  async function handle(message) {
    if (!validateFocusMessage(message)) return false;
    if (!tokensEqual(message.token, token)) return false;
    if (Math.abs(message.observed - observedNow()) > MAX_OBSERVATION_SKEW_MS) return false;
    if (message.observed <= invalidatedAt) return false;

    const requestGeneration = generation;
    const terminal = vscodeApi.window.activeTerminal;
    if (!terminal || !vscodeApi.window.state.focused) return false;

    let terminalPid;
    try {
      terminalPid = await terminal.processId;
    } catch {
      return false;
    }
    if (!positiveSafeInteger(terminalPid)) return false;
    if (!message.ancestors.includes(terminalPid)) return false;
    if (
      requestGeneration !== generation ||
      terminal !== vscodeApi.window.activeTerminal ||
      !vscodeApi.window.state.focused ||
      Math.abs(message.observed - observedNow()) > MAX_OBSERVATION_SKEW_MS ||
      message.observed <= invalidatedAt
    ) {
      return false;
    }

    if (message.observed < latestObserved) return false;
    if (compareSourcePosition(message, latestBySource.get(message.source)) <= 0) return false;
    latestObserved = message.observed;
    rememberSource(message);

    if (message.operation === "release") {
      if (owner && owner.source === message.source && owner.cycle === message.cycle) {
        await clearOwner();
      }
      return true;
    }

    owner = {
      source: message.source,
      cycle: message.cycle,
      sequence: message.sequence,
      observed: message.observed,
      expiresAt: now() + leaseMs,
    };
    armLease();
    await publish(true);
    return true;
  }

  async function invalidate() {
    generation += 1;
    activeTerminal = vscodeApi.window.activeTerminal;
    invalidatedAt = Math.max(invalidatedAt, observedNow());
    await clearOwner();
  }

  async function activeTerminalChanged() {
    await invalidate();
  }

  async function terminalClosed(terminal) {
    if (terminal === activeTerminal || terminal === vscodeApi.window.activeTerminal) {
      await invalidate();
    }
  }

  async function windowStateChanged(state) {
    if (!state.focused) await invalidate();
  }

  async function dispose() {
    await invalidate();
  }

  return {
    activeTerminalChanged,
    dispose,
    flush: () => publishChain,
    handle,
    initialize,
    terminalClosed,
    windowStateChanged,
  };
}

function randomSocketName() {
  return `window-${crypto.randomBytes(10).toString("hex")}.sock`;
}

function socketLocation() {
  const uid = typeof process.getuid === "function" ? process.getuid() : "user";
  const directory = path.join("/tmp", `termnav-vscode-${uid}`);
  const socketName = randomSocketName();
  return { directory, socketPath: path.join(directory, socketName), socketName };
}

function claimSocketLocation() {
  const preferred = socketLocation();
  for (let attempt = 0; attempt < 32; attempt += 1) {
    const socketName = attempt === 0 ? preferred.socketName : randomSocketName();
    const socketPath = path.join(preferred.directory, socketName);
    const claimPath = `${socketPath}.claim`;
    const claimToken = `${process.pid}:${crypto.randomBytes(12).toString("hex")}`;
    try {
      fs.writeFileSync(claimPath, claimToken, { flag: "wx", mode: 0o600 });
      return {
        directory: preferred.directory,
        socketPath,
        socketName,
        claimPath,
        claimToken,
      };
    } catch (error) {
      if (error.code !== "EEXIST") throw error;
    }
  }
  throw new Error("termnav: cannot claim a unique VS Code window socket");
}

function releaseSocketClaim(claimPath, claimToken) {
  try {
    if (fs.readFileSync(claimPath, "utf8") === claimToken) {
      fs.unlinkSync(claimPath);
    }
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
  }
}

function prepareDirectory(directory) {
  const uid = typeof process.getuid === "function" ? process.getuid() : undefined;
  fs.mkdirSync(directory, { mode: 0o700, recursive: true });
  const directoryStat = fs.lstatSync(directory);
  if (
    !directoryStat.isDirectory() ||
    directoryStat.isSymbolicLink() ||
    (uid !== undefined && directoryStat.uid !== uid) ||
    (directoryStat.mode & 0o077) !== 0
  ) {
    throw new Error("termnav: unsafe socket directory");
  }
}

function publishSocket(socketPath, generationPath) {
  const uid = typeof process.getuid === "function" ? process.getuid() : undefined;
  const temporaryLink = `${socketPath}.new-${process.pid}`;
  try {
    const socketStat = fs.lstatSync(socketPath);
    if (
      (!socketStat.isSocket() && !socketStat.isSymbolicLink()) ||
      (uid !== undefined && socketStat.uid !== uid)
    ) {
      throw new Error("termnav: refusing to replace an unsafe socket path");
    }
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
  }

  try {
    fs.unlinkSync(temporaryLink);
  } catch (error) {
    if (error.code !== "ENOENT") throw error;
  }
  fs.symlinkSync(path.basename(generationPath), temporaryLink);
  fs.renameSync(temporaryLink, socketPath);
}

function removePublishedSocket(socketPath, generationPath) {
  try {
    if (fs.readlinkSync(socketPath) === path.basename(generationPath)) {
      fs.unlinkSync(socketPath);
    }
  } catch (error) {
    if (error.code !== "ENOENT" && error.code !== "EINVAL") throw error;
  }
}

function sendJson(response, status, body) {
  if (response.destroyed || response.writableEnded) return;
  response.writeHead(status, { "content-type": "application/json" });
  response.end(JSON.stringify(body));
}

function readJson(request) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    let settled = false;

    request.on("data", (chunk) => {
      if (settled) return;
      size += chunk.length;
      if (size > MAX_BODY_BYTES) {
        settled = true;
        reject(Object.assign(new Error("request too large"), { status: 413 }));
        request.resume();
        return;
      }
      chunks.push(chunk);
    });
    request.on("end", () => {
      if (settled) return;
      settled = true;
      try {
        resolve(JSON.parse(Buffer.concat(chunks).toString("utf8")));
      } catch (error) {
        reject(Object.assign(error, { status: 400 }));
      }
    });
    request.on("error", reject);
  });
}

function createServer(focusController, token) {
  if (!TOKEN_PATTERN.test(token)) throw new Error("termnav: invalid window token");
  const server = http.createServer(async (request, response) => {
    response.on("error", () => {});
    if (request.method !== "POST") {
      request.resume();
      sendJson(response, 404, { error: "not found" });
      return;
    }

    try {
      const body = await readJson(request);
      if (!body || typeof body !== "object" || Array.isArray(body)) {
        sendJson(response, 400, { error: "invalid request" });
        return;
      }
      if (request.url !== "/nvim-focus" && request.url !== "/switch-tab") {
        sendJson(response, 404, { error: "not found" });
        return;
      }
      if (!tokensEqual(body.token, token)) {
        sendJson(response, 403, { error: "forbidden" });
        return;
      }
      if (request.url === "/nvim-focus") {
        if (!focusController || !validateFocusMessage(body)) {
          sendJson(response, 400, { error: "invalid focus message" });
          return;
        }
        const accepted = await focusController.handle(body);
        sendJson(response, 200, { ok: true, accepted });
        return;
      }
      const command = TAB_COMMANDS.get(body.direction);
      if (!command) {
        sendJson(response, 400, { error: "invalid direction" });
        return;
      }
      await vscode.commands.executeCommand(command);
      sendJson(response, 200, { ok: true });
    } catch (error) {
      const status =
        error && Number.isInteger(error.status) && error.status >= 400 && error.status <= 599
          ? error.status
          : 500;
      sendJson(response, status, { error: "request failed" });
    }
  });
  const connections = new Set();
  SERVER_CONNECTIONS.set(server, connections);
  server.on("connection", (socket) => {
    connections.add(socket);
    socket.setTimeout(2000);
    socket.once("timeout", () => socket.destroy());
    socket.once("close", () => connections.delete(socket));
  });
  server.on("clientError", (_error, socket) => socket.destroy());
  return server;
}

function closeServer(server) {
  for (const socket of SERVER_CONNECTIONS.get(server) || []) socket.destroy();
  return new Promise((resolve, reject) => {
    server.close((error) => {
      if (error) reject(error);
      else resolve();
    });
  });
}

async function activate(context) {
  if (activeBridge) await activeBridge.dispose();

  const token = crypto.randomBytes(32).toString("hex");
  const focusController = createFocusController({ token });
  await focusController.initialize();
  const focusSubscriptions = [];
  const environment = context.environmentVariableCollection;
  environment.persistent = false;
  const { directory: candidateDirectory } = socketLocation();
  prepareDirectory(candidateDirectory);
  const {
    directory: socketDirectory,
    socketPath,
    socketName,
    claimPath,
    claimToken,
  } = claimSocketLocation();
  const generationPath = path.join(
    socketDirectory,
    `${socketName.slice(0, -5)}-${process.pid}-${crypto.randomBytes(6).toString("hex")}.sock`,
  );
  const server = createServer(focusController, token);

  try {
    await new Promise((resolve, reject) => {
      server.once("error", reject);
      server.listen(generationPath, resolve);
    });
    fs.chmodSync(generationPath, 0o600);
    publishSocket(socketPath, generationPath);
  } catch (error) {
    try {
      server.close();
    } catch {}
    try {
      releaseSocketClaim(claimPath, claimToken);
    } catch {}
    await focusController.dispose().catch(() => {});
    throw error;
  }

  const handleEvent = (operation) => {
    Promise.resolve(operation).catch(() => {});
  };
  try {
    focusSubscriptions.push(
      vscode.window.onDidChangeActiveTerminal(() =>
        handleEvent(focusController.activeTerminalChanged()),
      ),
      vscode.window.onDidCloseTerminal((terminal) =>
        handleEvent(focusController.terminalClosed(terminal)),
      ),
      vscode.window.onDidChangeWindowState((state) =>
        handleEvent(focusController.windowStateChanged(state)),
      ),
    );
    environment.replace(TOKEN_ENVIRONMENT_KEY, token);
    environment.replace(SOCKET_ENVIRONMENT_KEY, socketPath);
  } catch (error) {
    environment.delete(SOCKET_ENVIRONMENT_KEY);
    environment.delete(TOKEN_ENVIRONMENT_KEY);
    for (const subscription of focusSubscriptions) subscription.dispose();
    await focusController.dispose().catch(() => {});
    await closeServer(server).catch(() => {});
    try {
      removePublishedSocket(socketPath, generationPath);
    } catch {}
    try {
      releaseSocketClaim(claimPath, claimToken);
    } catch {}
    throw error;
  }
  let disposePromise;
  const bridge = {
    dispose() {
      if (disposePromise) return disposePromise;
      environment.delete(SOCKET_ENVIRONMENT_KEY);
      environment.delete(TOKEN_ENVIRONMENT_KEY);
      for (const subscription of focusSubscriptions) subscription.dispose();
      const focusDispose = focusController.dispose();
      disposePromise = (async () => {
        let cleanupError;
        try {
          await closeServer(server);
        } catch (error) {
          cleanupError = error;
        }
        try {
          removePublishedSocket(socketPath, generationPath);
        } catch (error) {
          cleanupError ||= error;
        }
        try {
          releaseSocketClaim(claimPath, claimToken);
        } catch (error) {
          cleanupError ||= error;
        }
        if (activeBridge === bridge) activeBridge = undefined;
        try {
          await focusDispose;
        } catch (error) {
          cleanupError ||= error;
        }
        if (cleanupError) throw cleanupError;
      })();
      return disposePromise;
    },
  };
  activeBridge = bridge;
  context.subscriptions.push(bridge);
}

async function deactivate() {
  if (activeBridge) await activeBridge.dispose();
}

module.exports = {
  activate,
  createFocusController,
  createServer,
  deactivate,
  socketLocation,
  validateFocusMessage,
};
