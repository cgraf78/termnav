const fs = require("fs");
const http = require("http");
const crypto = require("crypto");
const path = require("path");
const vscode = require("vscode");

const ENVIRONMENT_KEY = "TERMNAV_VSCODE_SOCKET";
const MAX_BODY_BYTES = 1024;
const TAB_COMMANDS = new Map([
  ["next", "workbench.action.terminal.focusNext"],
  ["previous", "workbench.action.terminal.focusPrevious"],
]);
let activeBridge;

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

function createServer() {
  return http.createServer(async (request, response) => {
    if (request.method !== "POST" || request.url !== "/switch-tab") {
      request.resume();
      sendJson(response, 404, { error: "not found" });
      return;
    }

    try {
      const body = await readJson(request);
      const command =
        body && typeof body === "object" ? TAB_COMMANDS.get(body.direction) : undefined;
      if (!command) {
        sendJson(response, 400, { error: "invalid direction" });
        return;
      }
      await vscode.commands.executeCommand(command);
      sendJson(response, 200, { ok: true });
    } catch (error) {
      sendJson(response, error.status || 500, { error: "request failed" });
    }
  });
}

async function activate(context) {
  if (activeBridge) await activeBridge.dispose();

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
  const server = createServer();

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
    throw error;
  }

  environment.replace(ENVIRONMENT_KEY, socketPath);
  let disposePromise;
  const bridge = {
    dispose() {
      if (disposePromise) return disposePromise;
      environment.delete(ENVIRONMENT_KEY);
      disposePromise = new Promise((resolve, reject) => {
        server.close(() => {
          let cleanupError;
          try {
            removePublishedSocket(socketPath, generationPath);
          } catch (error) {
            cleanupError = error;
          }
          try {
            releaseSocketClaim(claimPath, claimToken);
          } catch (error) {
            cleanupError ||= error;
          }
          if (activeBridge === bridge) activeBridge = undefined;
          if (cleanupError) reject(cleanupError);
          else resolve();
        });
      });
      return disposePromise;
    },
  };
  activeBridge = bridge;
  context.subscriptions.push(bridge);
}

async function deactivate() {
  if (activeBridge) await activeBridge.dispose();
}

module.exports = { activate, createServer, deactivate, socketLocation };
