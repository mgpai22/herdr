import { afterEach, expect, test } from "bun:test";
import { rm } from "node:fs/promises";
import net, { createServer, type Server } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

const originalPlatform = process.platform;
const originalArgv = process.argv;
const originalCreateConnection = net.createConnection;
const originalEnvironment = {
  HERDR_ENV: process.env.HERDR_ENV,
  HERDR_OMP_IDLE_DEBOUNCE_MS: process.env.HERDR_OMP_IDLE_DEBOUNCE_MS,
  HERDR_OMP_SESSION_RETRY_MS: process.env.HERDR_OMP_SESSION_RETRY_MS,
  HERDR_PANE_ID: process.env.HERDR_PANE_ID,
  HERDR_SOCKET_PATH: process.env.HERDR_SOCKET_PATH,
  OMPCODE: process.env.OMPCODE,
  OMP_PROFILE: process.env.OMP_PROFILE,
  PI_PROFILE: process.env.PI_PROFILE,
};

let server: Server | undefined;
let socketPath: string | undefined;
let importCounter = 0;

afterEach(async () => {
  await new Promise<void>((resolve, reject) => {
    if (!server) {
      resolve();
      return;
    }
    server.close((error) => (error ? reject(error) : resolve()));
  });
  server = undefined;

  if (socketPath) {
    await rm(socketPath, { force: true });
    socketPath = undefined;
  }

  Object.defineProperty(process, "platform", { value: originalPlatform });
  net.createConnection = originalCreateConnection;
  process.argv = originalArgv;
  for (const [name, value] of Object.entries(originalEnvironment)) {
    if (value === undefined) {
      delete process.env[name];
    } else {
      process.env[name] = value;
    }
  }
});

const integrations = [
  { name: "Pi", modulePath: "./pi/herdr-agent-state.ts" },
  { name: "Oh My Pi", modulePath: "./omp/herdr-agent-state.ts" },
] as const;

const socketPlugins = [
  {
    name: "OpenCode",
    modulePath: "./opencode/herdr-agent-state.js",
    sessionID: "opencode-session",
  },
  { name: "Kilo", modulePath: "./kilo/herdr-agent-state.js", sessionID: "kilo-session" },
] as const;

function importFresh(modulePath: string) {
  importCounter += 1;
  return import(`${modulePath}?test=${importCounter}`);
}

type Handler = (event: unknown, context: unknown) => unknown;

function createExtensionHarness() {
  const handlers = new Map<string, Handler>();
  const eventHandlers = new Map<string, Handler>();
  return {
    handlers,
    eventHandlers,
    pi: {
      on(event: string, handler: Handler) {
        handlers.set(event, handler);
      },
      events: {
        on(event: string, handler: Handler) {
          eventHandlers.set(event, handler);
          return () => {};
        },
      },
    },
  };
}

function configureIntegrationEnvironment(recordingSocketPath: string) {
  // Tests may run inside an OMP shell; nested-session cases opt in explicitly.
  delete process.env.OMPCODE;
  delete process.env.OMP_PROFILE;
  delete process.env.PI_PROFILE;
  process.env.HERDR_ENV = "1";
  process.env.HERDR_SOCKET_PATH = recordingSocketPath;
  process.env.HERDR_PANE_ID = "test:p1";
}

function captureConnectionEndpoint() {
  let connectedEndpoint: unknown;
  net.createConnection = ((...args: unknown[]) => {
    connectedEndpoint = args[0];
    return Reflect.apply(originalCreateConnection, net, args);
  }) as typeof net.createConnection;
  return () => connectedEndpoint;
}

async function startRecordingServer(name: string): Promise<unknown[]> {
  const recordingSocketPath = join(tmpdir(), `herdr-${name}-${process.pid}.sock`);
  socketPath = recordingSocketPath;
  await rm(recordingSocketPath, { force: true });

  const requests: unknown[] = [];
  const recordingServer = createServer((socket) => {
    let input = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk) => {
      input += chunk;
      const newline = input.indexOf("\n");
      if (newline === -1) {
        return;
      }
      const request = JSON.parse(input.slice(0, newline));
      requests.push(request);
      socket.end(JSON.stringify({ id: request.id, result: { type: "ok" } }) + "\n");
    });
  });
  server = recordingServer;
  await new Promise<void>((resolve, reject) => {
    recordingServer.once("error", reject);
    recordingServer.listen(originalPlatform === "win32" ? `\\\\.\\pipe\\${recordingSocketPath}` : recordingSocketPath, resolve);
  });
  configureIntegrationEnvironment(recordingSocketPath);
  return requests;
}

for (const socketPlugin of socketPlugins) {
  test(`${socketPlugin.name} maps the Windows socket marker path to a named pipe endpoint`, async () => {
    const markerPath = `herdr-${socketPlugin.name.toLowerCase()}-${process.pid}.sock`;
    configureIntegrationEnvironment(markerPath);
    Object.defineProperty(process, "platform", { value: "win32" });
    const connectedEndpoint = captureConnectionEndpoint();

    process.argv = ["bun", "/$bunfs/root/src/index.js", "run"];
    const { HerdrAgentStatePlugin } = await importFresh(socketPlugin.modulePath);
    const plugin = await HerdrAgentStatePlugin();
    await plugin.event({
      event: {
        type: "session.updated",
        properties: { sessionID: socketPlugin.sessionID },
      },
    });

    expect(connectedEndpoint()).toBe(`\\\\.\\pipe\\${markerPath}`);
  });
}

test("OpenCode stays disabled without the Herdr socket environment", async () => {
  process.env.HERDR_ENV = "1";
  process.env.HERDR_PANE_ID = "test:p1";
  delete process.env.HERDR_SOCKET_PATH;

  const { HerdrAgentStatePlugin } = await importFresh("./opencode/herdr-agent-state.js");

  expect(await HerdrAgentStatePlugin()).toEqual({});
});

for (const integration of integrations) {
  test(`${integration.name} maps the Windows socket marker path to a named pipe endpoint`, async () => {
    const markerPath = `herdr-${integration.name.toLowerCase().replaceAll(" ", "-")}-${process.pid}.sock`;
    configureIntegrationEnvironment(markerPath);
    Object.defineProperty(process, "platform", { value: "win32" });
    const connectedEndpoint = captureConnectionEndpoint();
    const { handlers, pi } = createExtensionHarness();

    const { default: install } = await importFresh(integration.modulePath);
    install(pi);
    await handlers.get("session_start")?.(
      { reason: "startup" },
      {
        hasUI: true,
        mode: "tui",
        isIdle: () => true,
        sessionManager: {
          getSessionFile: () => undefined,
          getSessionId: () => "test-session",
        },
      },
    );

    expect(connectedEndpoint()).toBe(`\\\\.\\pipe\\${markerPath}`);
  });

  test(`${integration.name} reload preserves working state when the agent is active`, async () => {
    const requests = await startRecordingServer(
      integration.name.toLowerCase().replaceAll(" ", "-"),
    );
    const { handlers, pi } = createExtensionHarness();

    const { default: install } = await importFresh(integration.modulePath);
    install(pi);

    const sessionStart = handlers.get("session_start");
    expect(sessionStart).toBeDefined();
    await sessionStart?.(
      { reason: "reload" },
      {
        hasUI: true,
        mode: "tui",
        isIdle: () => false,
        sessionManager: {
          getSessionFile: () => undefined,
          getSessionId: () => undefined,
        },
      },
    );

    const reportedState = () => {
      for (const request of requests) {
        if (!isRecord(request) || request.method !== "pane.report_agent") {
          continue;
        }
        const params = request.params;
        if (isRecord(params) && typeof params.state === "string") {
          return params.state;
        }
      }
      return undefined;
    };

    const deadline = Date.now() + 1_000;
    while (Date.now() < deadline && reportedState() === undefined) {
      await Bun.sleep(5);
    }

    expect(reportedState()).toBe("working");
  });
}

test("OMP ignores nested sessions launched inside another OMP shell", async () => {
  const requests = await startRecordingServer("omp-nested");
  process.env.OMPCODE = "1";
  const { handlers, pi } = createExtensionHarness();

  const { default: install } = await importFresh("./omp/herdr-agent-state.ts");
  install(pi);

  // OMP sets OMPCODE on every shell it spawns. A nested `omp` inherits it and
  // must not claim the pane's session for its short-lived conversation.
  expect(handlers.size).toBe(0);
  await handlers.get("session_start")?.(
    { reason: "startup" },
    {
      hasUI: true,
      isIdle: () => true,
      sessionManager: {
        getSessionFile: () => "/tmp/omp-nested.jsonl",
        getSessionId: () => "omp-nested",
      },
    },
  );
  await Bun.sleep(25);

  expect(requests).toEqual([]);
});

test("OMP accepts POSIX and Windows session paths", async () => {
  const { isAbsoluteSessionPath } = await importFresh("./omp/herdr-agent-state.ts");

  expect(isAbsoluteSessionPath("/tmp/omp-session.jsonl")).toBe(true);
  expect(isAbsoluteSessionPath("C:\\Users\\User\\.omp\\agent\\sessions\\omp-session.jsonl")).toBe(
    true,
  );
  expect(isAbsoluteSessionPath("C:/Users/User/.omp/agent/sessions/omp-session.jsonl")).toBe(true);
  expect(isAbsoluteSessionPath("relative/omp-session.jsonl")).toBe(false);
});

test("Pi reports a Windows session path", async () => {
  const requests = await startRecordingServer("pi-windows-session-path");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const sessionPath = "C:\\Users\\User\\.pi\\agent\\sessions\\pi-session.jsonl";
  await handlers.get("session_start")?.(
    { reason: "startup" },
    {
      ...piContext(() => true),
      sessionManager: {
        getSessionFile: () => sessionPath,
        getSessionId: () => "pi-session",
      },
    },
  );
  await waitFor(() => requests.length === 2);

  expect(requests.map(requestSessionPath)).toEqual([sessionPath, sessionPath]);
});

test("Pi reports idle only after the agent settles", async () => {
  const requests = await startRecordingServer("pi-settled");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  expect(completionHandlers(handlers)).toEqual(["agent_settled"]);
  let idle = true;
  const context = piContext(() => idle);
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  await waitFor(() => requestStates(requests).length === 1);

  idle = false;
  handlers.get("agent_start")?.({}, context);
  await waitFor(() => requestStates(requests).length === 2);
  expect(requestStates(requests)).toEqual(["idle", "working"]);
  expect(handlers.has("agent_end")).toBe(false);

  const requestCountBeforeStaleSettlement = requests.length;
  handlers.get("agent_settled")?.({}, context);
  await Bun.sleep(25);
  expect(requests).toHaveLength(requestCountBeforeStaleSettlement);
  expect(requestStates(requests)).toEqual(["idle", "working"]);

  idle = true;
  handlers.get("agent_settled")?.({}, context);
  await waitFor(() => requestStates(requests).length === 3);
  expect(requestStates(requests)).toEqual(["idle", "working", "idle"]);
});

test("Pi ignores RPC sessions even when UI APIs are available", async () => {
  const requests = await startRecordingServer("pi-rpc");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const context = {
    ...piContext(() => true),
    hasUI: true,
    mode: "rpc",
  };
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  handlers.get("agent_start")?.({}, context);
  handlers.get("agent_settled")?.({}, context);
  await Bun.sleep(25);

  expect(requests).toEqual([]);
});

test("Pi settlement preserves explicit blocked-state precedence", async () => {
  const requests = await startRecordingServer("pi-settled-blocked");
  const { eventHandlers, handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  let idle = true;
  const context = piContext(() => idle);
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  await waitFor(() => requestStates(requests).length === 1);
  idle = false;
  handlers.get("agent_start")?.({}, context);
  await waitFor(() => requestStates(requests).length === 2);
  eventHandlers.get("herdr:blocked")?.({ active: true, label: "approval" }, context);
  await waitFor(() => requestStates(requests).length === 3);

  idle = true;
  handlers.get("agent_settled")?.({}, context);
  await Bun.sleep(25);
  expect(requestStates(requests)).toEqual(["idle", "working", "blocked"]);

  eventHandlers.get("herdr:blocked")?.({ active: false }, context);
  await waitFor(() => requestStates(requests).length === 4);
  expect(requestStates(requests)).toEqual(["idle", "working", "blocked", "idle"]);
});

test("Pi reports the session replacement source", async () => {
  const requests = await startRecordingServer("pi-session-source");
  const { handlers, pi } = createExtensionHarness();

  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const sessionStart = handlers.get("session_start");
  expect(sessionStart).toBeDefined();
  await sessionStart?.(
    { reason: "new" },
    {
      hasUI: true,
      mode: "tui",
      isIdle: () => true,
      sessionManager: {
        getSessionFile: () => "/tmp/pi-new.jsonl",
        getSessionId: () => "pi-new",
      },
    },
  );

  const reportedSession = () =>
    requests.find((request) => isRecord(request) && request.method === "pane.report_agent_session");
  const deadline = Date.now() + 1_000;
  while (Date.now() < deadline && reportedSession() === undefined) {
    await Bun.sleep(5);
  }

  const request = reportedSession();
  expect(request).toBeDefined();
  expect(isRecord(request) && isRecord(request.params) ? request.params.session_start_source : null)
    .toBe("new");
});

test("Pi waits for a replacement session report before publishing state", async () => {
  const recordingSocketPath = join(tmpdir(), `herdr-pi-session-order-${process.pid}.sock`);
  socketPath = recordingSocketPath;
  await rm(recordingSocketPath, { force: true });

  const requests: unknown[] = [];
  let acknowledgeSessionReport: (() => void) | undefined;
  const recordingServer = createServer((socket) => {
    let input = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk) => {
      input += chunk;
      const newline = input.indexOf("\n");
      if (newline === -1) {
        return;
      }
      const request = JSON.parse(input.slice(0, newline));
      requests.push(request);
      if (isRecord(request) && request.method === "pane.report_agent_session") {
        acknowledgeSessionReport = () => socket.end("{}\n");
        return;
      }
      socket.end("{}\n");
    });
  });
  server = recordingServer;
  await new Promise<void>((resolve, reject) => {
    recordingServer.once("error", reject);
    recordingServer.listen(originalPlatform === "win32" ? `\\\\.\\pipe\\${recordingSocketPath}` : recordingSocketPath, resolve);
  });

  configureIntegrationEnvironment(recordingSocketPath);
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const sessionStart = handlers.get("session_start");
  expect(sessionStart).toBeDefined();
  const sessionStartResult = sessionStart?.(
    { reason: "new" },
    {
      hasUI: true,
      mode: "tui",
      isIdle: () => false,
      sessionManager: {
        getSessionFile: () => "/tmp/pi-new.jsonl",
        getSessionId: () => "pi-new",
      },
    },
  );

  const deadline = Date.now() + 1_000;
  while (Date.now() < deadline && acknowledgeSessionReport === undefined) {
    await Bun.sleep(5);
  }
  expect(acknowledgeSessionReport).toBeDefined();
  expect(
    requests.some((request) => isRecord(request) && request.method === "pane.report_agent"),
  ).toBe(false);

  acknowledgeSessionReport?.();
  await sessionStartResult;

  const stateDeadline = Date.now() + 1_000;
  while (
    Date.now() < stateDeadline &&
    !requests.some((request) => isRecord(request) && request.method === "pane.report_agent")
  ) {
    await Bun.sleep(5);
  }
  expect(requests.map((request) => (isRecord(request) ? request.method : undefined))).toEqual([
    "pane.report_agent_session",
    "pane.report_agent",
  ]);
});

async function startDroppedFirstResponseServer(name: string) {
  const recordingSocketPath = join(tmpdir(), `herdr-${name}-${process.pid}.sock`);
  socketPath = recordingSocketPath;
  await rm(recordingSocketPath, { force: true });

  let connectionCount = 0;
  const attemptedRequests: unknown[] = [];
  const deliveredRequests: unknown[] = [];
  const recordingServer = createServer((socket) => {
    connectionCount += 1;
    const connectionNumber = connectionCount;
    let input = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk) => {
      input += chunk;
      const newline = input.indexOf("\n");
      if (newline === -1) {
        return;
      }
      const request = JSON.parse(input.slice(0, newline));
      attemptedRequests.push(request);
      if (connectionNumber === 1) {
        return;
      }
      deliveredRequests.push(request);
      socket.end(JSON.stringify({ id: request.id, result: { type: "ok" } }) + "\n");
    });
  });
  server = recordingServer;
  await new Promise<void>((resolve, reject) => {
    recordingServer.once("error", reject);
    recordingServer.listen(originalPlatform === "win32" ? `\\\\.\\pipe\\${recordingSocketPath}` : recordingSocketPath, resolve);
  });

  configureIntegrationEnvironment(recordingSocketPath);
  return {
    attemptedRequests,
    deliveredRequests,
    connectionCount: () => connectionCount,
  };
}

test("Oh My Pi ignores non-UI agent sessions after root activation", async () => {
  const requests = await startRecordingServer("omp-root-only");
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./omp/herdr-agent-state.ts");
  install(pi);
  const rootContext = {
    hasUI: true,
    mode: "tui",
    isIdle: () => true,
    sessionManager: {
      getSessionFile: () => "/tmp/root-session.jsonl",
      getSessionId: () => "root-session",
    },
  };
  await handlers.get("session_start")?.({ reason: "startup" }, rootContext);
  await waitFor(() => requests.some((request) =>
    isRecord(request) && request.method === "pane.report_agent_session_v2"));
  const before = requests.length;

  await handlers.get("agent_start")?.({}, {
    ...rootContext,
    hasUI: false,
    sessionManager: {
      getSessionFile: () => "/tmp/task-session.jsonl",
      getSessionId: () => "task-session",
    },
  });
  await Bun.sleep(25);

  expect(requests).toHaveLength(before);
  expect(requests.some((request) => JSON.stringify(request).includes("task-session"))).toBe(false);
});

test("Oh My Pi old-server rejection cannot create a resumable legacy ref", async () => {
  const recordingSocketPath = join(tmpdir(), `herdr-omp-old-server-${process.pid}.sock`);
  socketPath = recordingSocketPath;
  await rm(recordingSocketPath, { force: true });
  const requests: any[] = [];
  const recordingServer = createServer((socket) => {
    let input = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk) => {
      input += chunk;
      const newline = input.indexOf("\n");
      if (newline < 0) return;
      const request = JSON.parse(input.slice(0, newline));
      requests.push(request);
      const response = request.method === "pane.report_agent_session_v2"
        ? { id: request.id, error: { code: "invalid_request", message: "unknown method" } }
        : { id: request.id, result: { type: "ok" } };
      socket.end(JSON.stringify(response) + "\n");
    });
  });
  server = recordingServer;
  await new Promise<void>((resolve, reject) => {
    recordingServer.once("error", reject);
    recordingServer.listen(recordingSocketPath, resolve);
  });
  configureIntegrationEnvironment(recordingSocketPath);
  process.env.OMP_PROFILE = "restricted";
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./omp/herdr-agent-state.ts");
  install(pi);
  await handlers.get("session_start")?.({ reason: "startup" }, {
    hasUI: true,
    mode: "tui",
    isIdle: () => true,
    sessionManager: {
      getSessionFile: () => "/tmp/restricted-session.jsonl",
      getSessionId: () => "restricted-session",
    },
  });
  await waitFor(() => requests.some((request) => request.method === "pane.report_agent"));
  const v2 = requests.filter((request) => request.method === "pane.report_agent_session_v2");
  expect(v2).toHaveLength(2);
  expect(v2[0].params.launch_profile).toBe("restricted");
  expect(v2[0].params.agent_pid).toBe(process.pid);
  expect(v2[0].params.session_start_source).toBe("startup");
  const state = requests.find((request) => request.method === "pane.report_agent");
  expect(state.params.agent_session_path).toBeUndefined();
  expect(state.params.agent_session_id).toBeUndefined();
});

test("Oh My Pi retries working before a queued idle state", async () => {
  const { attemptedRequests } = await startDroppedFirstResponseServer("omp-retry");
  process.env.HERDR_OMP_IDLE_DEBOUNCE_MS = "0";
  const { handlers, pi } = createExtensionHarness();

  const { default: install } = await importFresh("./omp/herdr-agent-state.ts");
  install(pi);

  const context = {
    hasUI: true,
    isIdle: () => false,
    sessionManager: {
      getSessionFile: () => undefined,
      getSessionId: () => undefined,
    },
  };
  handlers.get("session_start")?.({ reason: "startup" }, context);
  handlers.get("agent_end")?.({ messages: [] }, context);

  const deadline = Date.now() + 2_500;
  while (Date.now() < deadline && attemptedRequests.length < 3) {
    await Bun.sleep(5);
  }

  expect(attemptedRequests).toHaveLength(3);
  // The test server records the parsed wire requests the extension sent.
  const [first, retry] = attemptedRequests as ReportRequest[];
  expect(retry.method).toBe(first.method);
  expect(retry.id).not.toBe(first.id);
  expect(retry.params.seq).toBeGreaterThan(first.params.seq);
  expect(requestState(retry)).toBe("working");
  expect(requestState(attemptedRequests[0])).toBe("working");
  expect(requestState(attemptedRequests[2])).toBe("idle");
});

test("Oh My Pi keeps working when a turn ends with a scheduled continuation", async () => {
  const requests = await startRecordingServer("omp-will-continue");
  process.env.HERDR_OMP_IDLE_DEBOUNCE_MS = "0";
  const { handlers, pi } = createExtensionHarness();

  const { default: install } = await importFresh("./omp/herdr-agent-state.ts");
  install(pi);

  let idle = true;
  const context = {
    hasUI: true,
    isIdle: () => idle,
    sessionManager: {
      getSessionFile: () => undefined,
      getSessionId: () => undefined,
    },
  };

  handlers.get("session_start")?.({ reason: "startup" }, context);
  await waitFor(() => requestStates(requests).length === 1);

  idle = false;
  handlers.get("agent_start")?.({}, context);
  await waitFor(() => requestStates(requests).length === 2);
  expect(requestStates(requests)).toEqual(["idle", "working"]);

  // OMP already scheduled an automatic continuation, so this loop end is not a
  // user-visible settle and must not publish idle. See issue #2851.
  handlers.get("agent_end")?.({ messages: [], willContinue: true }, context);
  await Bun.sleep(50);
  expect(requestStates(requests)).toEqual(["idle", "working"]);

  // The real terminal end still settles the pane.
  idle = true;
  handlers.get("agent_end")?.({ messages: [] }, context);
  await waitFor(() => requestStates(requests).length === 3);
  expect(requestStates(requests)).toEqual(["idle", "working", "idle"]);
});

test("Pi retries working state after an unanswered socket attempt", async () => {
  const { attemptedRequests, deliveredRequests, connectionCount } =
    await startDroppedFirstResponseServer("pi-retry");
  const { handlers, pi } = createExtensionHarness();

  const { default: install } = await importFresh("./pi/herdr-agent-state.ts");
  install(pi);

  const sessionStart = handlers.get("session_start");
  expect(sessionStart).toBeDefined();
  await sessionStart?.(
    { reason: "startup" },
    {
      hasUI: true,
      mode: "tui",
      isIdle: () => false,
      sessionManager: {
        getSessionFile: () => undefined,
        getSessionId: () => undefined,
      },
    },
  );

  const reportedWorking = () =>
    deliveredRequests.some((request) => {
      if (!isRecord(request) || request.method !== "pane.report_agent") {
        return false;
      }
      const params = request.params;
      return isRecord(params) && params.state === "working";
    });

  const deadline = Date.now() + 2_500;
  while (Date.now() < deadline && !reportedWorking()) {
    await Bun.sleep(5);
  }

  expect(connectionCount()).toBeGreaterThanOrEqual(2);
  expect(attemptedRequests.length).toBeGreaterThanOrEqual(2);
  expect(attemptedRequests[1]).toEqual(attemptedRequests[0]);
  expect(reportedWorking()).toBe(true);
});

function completionHandlers(handlers: Map<string, Handler>): string[] {
  return ["agent_end", "agent_settled"].filter((event) => handlers.has(event));
}

function piContext(isIdle: () => boolean) {
  return {
    hasUI: true,
    mode: "tui",
    isIdle,
    sessionManager: {
      getSessionFile: () => undefined,
      getSessionId: () => undefined,
    },
  };
}

function requestStates(requests: unknown[]): unknown[] {
  return requests
    .filter((request) => isRecord(request) && request.method === "pane.report_agent")
    .map(requestState);
}

async function waitFor(predicate: () => boolean, timeoutMs = 1_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline && !predicate()) {
    await Bun.sleep(5);
  }
  expect(predicate()).toBe(true);
}

function requestState(request: unknown): unknown {
  if (!isRecord(request) || !isRecord(request.params)) {
    return undefined;
  }
  return request.params.state;
}

function requestSessionPath(request: unknown): unknown {
  if (!isRecord(request) || !isRecord(request.params)) {
    return undefined;
  }
  return request.params.agent_session_path;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

test("Oh My Pi dropped first v2 response still registers via a fresh retry", async () => {
  // The test server mirrors production: the first report applies but its
  // reply is lost, and an identical id retry is rejected as stale. The hook
  // retry must therefore mint a fresh id and sequence number so the session
  // registers and later state reports keep it.
  const recordingSocketPath = join(tmpdir(), `herdr-omp-v2-retry-${process.pid}.sock`);
  socketPath = recordingSocketPath;
  await rm(recordingSocketPath, { force: true });
  const requests: any[] = [];
  const seenIds = new Set<unknown>();
  const recordingServer = createServer((socket) => {
    let input = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk) => {
      input += chunk;
      const newline = input.indexOf("\n");
      if (newline < 0) return;
      const request = JSON.parse(input.slice(0, newline));
      requests.push(request);
      if (requests.length === 1) {
        seenIds.add(request.id);
        return;
      }
      if (seenIds.has(request.id)) {
        socket.end(
          JSON.stringify({ id: request.id, error: { code: "stale_report", message: "stale" } }) + "\n",
        );
        return;
      }
      seenIds.add(request.id);
      socket.end(JSON.stringify({ id: request.id, result: { type: "ok" } }) + "\n");
    });
  });
  server = recordingServer;
  await new Promise<void>((resolve, reject) => {
    recordingServer.once("error", reject);
    recordingServer.listen(recordingSocketPath, resolve);
  });
  configureIntegrationEnvironment(recordingSocketPath);
  process.env.HERDR_OMP_IDLE_DEBOUNCE_MS = "0";
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./omp/herdr-agent-state.ts");
  install(pi);
  const context = {
    hasUI: true,
    mode: "tui",
    isIdle: () => true,
    sessionManager: {
      getSessionFile: () => "/tmp/retry-session.jsonl",
      getSessionId: () => "retry-session",
    },
  };
  await handlers.get("session_start")?.({ reason: "startup" }, context);
  await waitFor(
    () => requests.filter((request) => request.method === "pane.report_agent_session_v2").length >= 2,
    5_000,
  );
  const v2 = requests.filter((request) => request.method === "pane.report_agent_session_v2");
  expect(v2).toHaveLength(2);
  expect(v2[0].params.launch_profile).toBe("default");
  expect(v2[0].params.agent_pid).toBe(process.pid);
  expect(v2[1].id).not.toBe(v2[0].id);
  expect(v2[1].params.seq).toBeGreaterThan(v2[0].params.seq);
  expect(v2[1].params.agent_session_path).toBe("/tmp/retry-session.jsonl");
  expect(v2[1].params.launch_profile).toBe("default");
  await handlers.get("agent_end")?.({ messages: [] }, context);
  await waitFor(
    () => requests.some((request) => request.method === "pane.report_agent"),
    5_000,
  );
  const state = requests.find((request) => request.method === "pane.report_agent");
  expect(state.params.agent_session_path).toBe("/tmp/retry-session.jsonl");
}, 15_000);

test("Oh My Pi reports stay above the shared per-source seq watermark", async () => {
  // Mirrors the server: one seq watermark per source across report methods.
  // A report at or below it is acknowledged but dropped as stale. The first
  // reply of each method is lost after the server applied it, forcing retries.
  const recordingSocketPath = join(tmpdir(), `herdr-omp-watermark-${process.pid}.sock`);
  socketPath = recordingSocketPath;
  await rm(recordingSocketPath, { force: true });
  const received: ReportRequest[] = [];
  const stale: ReportRequest[] = [];
  const droppedMethods = new Set<string>();
  let watermark = -Infinity;
  const recordingServer = createServer((socket) => {
    let input = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk) => {
      input += chunk;
      const newline = input.indexOf("\n");
      if (newline < 0) return;
      const request: ReportRequest = JSON.parse(input.slice(0, newline));
      received.push(request);
      if (request.params.seq <= watermark) {
        stale.push(request);
      } else {
        watermark = request.params.seq;
      }
      if (!droppedMethods.has(request.method)) {
        droppedMethods.add(request.method);
        return;
      }
      socket.end(JSON.stringify({ id: request.id, result: { type: "ok" } }) + "\n");
    });
  });
  server = recordingServer;
  await new Promise<void>((resolve, reject) => {
    recordingServer.once("error", reject);
    recordingServer.listen(recordingSocketPath, resolve);
  });
  configureIntegrationEnvironment(recordingSocketPath);
  process.env.HERDR_OMP_IDLE_DEBOUNCE_MS = "0";
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./omp/herdr-agent-state.ts");
  install(pi);
  const context = {
    hasUI: true,
    mode: "tui",
    isIdle: () => true,
    sessionManager: {
      getSessionFile: () => "/tmp/watermark-session.jsonl",
      getSessionId: () => "watermark-session",
    },
  };
  handlers.get("session_start")?.({ reason: "startup" }, context);
  handlers.get("agent_start")?.({}, context);
  await waitFor(() => requestStates(received).includes("working"), 5_000);
  handlers.get("agent_end")?.({ messages: [] }, context);
  await waitFor(() => requestStates(received).at(-1) === "idle", 5_000);

  expect(stale).toEqual([]);
  // Both methods were retried after a lost reply: the retry is the next
  // request of the same method, with a fresh id and a higher seq.
  for (const method of ["pane.report_agent_session_v2", "pane.report_agent"]) {
    const retried = received.some((request, index) => {
      const next = received[index + 1];
      return request.method === method
        && next?.method === method
        && next.id !== request.id
        && next.params.seq > request.params.seq;
    });
    expect(retried).toBe(true);
  }
}, 15_000);

test("Oh My Pi state retry keeps the session ref registered at its first attempt", async () => {
  // The first working reply is lost. A new agent_start during the retry window
  // re-reports the session, which clears the registered key until its ack. The
  // retry must still carry the ref, or the server clears the saved session.
  const recordingSocketPath = join(tmpdir(), `herdr-omp-retry-ref-${process.pid}.sock`);
  socketPath = recordingSocketPath;
  await rm(recordingSocketPath, { force: true });
  const received: Array<ReportRequest & { params: { agent_session_path?: string } }> = [];
  let droppedWorking = false;
  let onFirstWorking: (() => void) | undefined;
  const recordingServer = createServer((socket) => {
    let input = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk) => {
      input += chunk;
      const newline = input.indexOf("\n");
      if (newline < 0) return;
      const request = JSON.parse(input.slice(0, newline));
      received.push(request);
      if (request.params.state === "working" && !droppedWorking) {
        droppedWorking = true;
        onFirstWorking?.();
        return;
      }
      socket.end(JSON.stringify({ id: request.id, result: { type: "ok" } }) + "\n");
    });
  });
  server = recordingServer;
  await new Promise<void>((resolve, reject) => {
    recordingServer.once("error", reject);
    recordingServer.listen(recordingSocketPath, resolve);
  });
  configureIntegrationEnvironment(recordingSocketPath);
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./omp/herdr-agent-state.ts");
  install(pi);
  const context = {
    hasUI: true,
    mode: "tui",
    isIdle: () => true,
    sessionManager: {
      getSessionFile: () => "/tmp/retry-ref-session.jsonl",
      getSessionId: () => "retry-ref-session",
    },
  };
  onFirstWorking = () => handlers.get("agent_start")?.({}, context);
  handlers.get("session_start")?.({ reason: "startup" }, context);
  handlers.get("agent_start")?.({}, context);
  const working = () => received.filter((request) => request.params.state === "working");
  await waitFor(() => working().length >= 2, 5_000);

  expect(working().map((request) => request.params.agent_session_path)).toEqual(
    working().map(() => "/tmp/retry-ref-session.jsonl"),
  );
}, 15_000);

test("Oh My Pi re-sends an unacknowledged session report without a new event", async () => {
  // A loaded server drops both attempts of the first session report. The hook
  // must register the session on its own, so later state reports carry the ref.
  const recordingSocketPath = join(tmpdir(), `herdr-omp-session-retry-${process.pid}.sock`);
  socketPath = recordingSocketPath;
  await rm(recordingSocketPath, { force: true });
  const received: Array<ReportRequest & { params: { agent_session_path?: string } }> = [];
  let droppedSessionReports = 0;
  const recordingServer = createServer((socket) => {
    let input = "";
    socket.setEncoding("utf8");
    socket.on("data", (chunk) => {
      input += chunk;
      const newline = input.indexOf("\n");
      if (newline < 0) return;
      const request = JSON.parse(input.slice(0, newline));
      received.push(request);
      if (request.method === "pane.report_agent_session_v2" && droppedSessionReports < 2) {
        droppedSessionReports += 1;
        socket.destroy();
        return;
      }
      socket.end(JSON.stringify({ id: request.id, result: { type: "ok" } }) + "\n");
    });
  });
  server = recordingServer;
  await new Promise<void>((resolve, reject) => {
    recordingServer.once("error", reject);
    recordingServer.listen(recordingSocketPath, resolve);
  });
  configureIntegrationEnvironment(recordingSocketPath);
  process.env.HERDR_OMP_SESSION_RETRY_MS = "10";
  process.env.HERDR_OMP_IDLE_DEBOUNCE_MS = "0";
  const { handlers, pi } = createExtensionHarness();
  const { default: install } = await importFresh("./omp/herdr-agent-state.ts");
  install(pi);
  const context = {
    hasUI: true,
    mode: "tui",
    isIdle: () => true,
    sessionManager: {
      getSessionFile: () => "/tmp/session-retry.jsonl",
      getSessionId: () => "session-retry",
    },
  };
  handlers.get("session_start")?.({ reason: "startup" }, context);
  const sessionReports = () =>
    received.filter((request) => request.method === "pane.report_agent_session_v2");
  await waitFor(() => sessionReports().length >= 3, 5_000);
  handlers.get("tool_approval_requested")?.({ toolName: "bash" }, context);
  await waitFor(() => received.at(-1)?.params.state === "blocked", 5_000);

  expect(sessionReports()).toHaveLength(3);
  expect(received.at(-1)?.params.agent_session_path).toBe("/tmp/session-retry.jsonl");
}, 15_000);

type ReportRequest = {
  id: string;
  method: string;
  params: { seq: number; state?: unknown };
};

type TerminalInputHandler = (data: string) => { consume?: boolean; data?: string } | undefined;

async function installOmpWithTerminalInput(name: string, ui: Record<string, unknown> | undefined) {
  const requests = await startRecordingServer(name);
  const { handlers, pi } = createExtensionHarness();
  const sent: unknown[][] = [];
  const { default: install } = await importFresh("./omp/herdr-agent-state.ts");
  install({ ...pi, sendUserMessage: (...args: unknown[]) => sent.push(args) });
  let idle = true;
  const context = {
    hasUI: true,
    mode: "tui",
    isIdle: () => idle,
    ui,
    sessionManager: {
      getSessionFile: () => "/tmp/omp-instruct.jsonl",
      getSessionId: () => "omp-instruct",
    },
  };
  const reports = () =>
    requests.filter((request) => isRecord(request) && request.method === "pane.report_agent_session_v2") as {
      params: Record<string, unknown>;
    }[];
  return { requests, handlers, sent, context, reports, setIdle: (value: boolean) => (idle = value) };
}

test("Oh My Pi consumes a herdr instruction paste and acks it from the agent process", async () => {
  let listener: TerminalInputHandler | undefined;
  const harness = await installOmpWithTerminalInput("omp-instruct", {
    onTerminalInput(handler: TerminalInputHandler) {
      listener = handler;
      return () => {};
    },
  });
  await harness.handlers.get("session_start")?.({ reason: "startup" }, harness.context);
  await waitFor(() => harness.reports().length === 1);
  expect(harness.reports()[0].params.accepts_instructions).toBe(true);

  const id = "0123456789abcdef0123456789abcdef";
  const block = `\x1b[200~herdr-instruction:v1:${id}\nReport status\nthen stop\x1b[201~`;
  const acks = () =>
    harness.requests.filter((request) => isRecord(request) && request.method === "pane.ack_instruction") as {
      params: Record<string, unknown>;
    }[];
  expect(listener?.(block)).toEqual({ consume: true });
  expect(harness.sent).toEqual([["Report status\nthen stop", undefined]]);
  await waitFor(() => acks().length === 1);
  expect(acks()[0].params).toEqual({
    pane_id: "test:p1",
    instruction_id: id,
    agent_pid: process.pid,
    delivered_as: "prompt",
  });

  harness.setIdle(false);
  // A person's Enter that arrived in the same read must still reach the editor.
  expect(listener?.(`${block}\r`)).toEqual({ data: "\r" });
  expect(harness.sent[1]).toEqual(["Report status\nthen stop", { deliverAs: "aside" }]);
  await waitFor(() => acks().length === 2);
  expect(acks()[1].params.delivered_as).toBe("aside");

  for (const ordinary of ["\x1b[200~herdr-instruction:v1:short\nx\x1b[201~", "\x1b[200~pasted\x1b[201~", "a", "\r"]) {
    expect(listener?.(ordinary)).toBeUndefined();
  }
  expect(harness.sent).toHaveLength(2);
  expect(acks()).toHaveLength(2);
});

test("Oh My Pi registers a fresh instruction listener on every session switch", async () => {
  const registered: TerminalInputHandler[] = [];
  let unsubscribed = 0;
  const harness = await installOmpWithTerminalInput("omp-instruct-switch", {
    onTerminalInput(handler: TerminalInputHandler) {
      registered.push(handler);
      return () => {
        unsubscribed += 1;
      };
    },
  });
  await harness.handlers.get("session_start")?.({ reason: "startup" }, harness.context);
  // OMP clears extension input listeners before /new or /resume emits session_switch.
  await harness.handlers.get("session_switch")?.({ reason: "new" }, harness.context);
  await waitFor(() => harness.reports().length === 2);

  expect(registered).toHaveLength(2);
  expect(unsubscribed).toBe(1);
  expect(harness.reports().map((report) => report.params.accepts_instructions)).toEqual([true, true]);
});

test("Oh My Pi without terminal input support reports no instruction listener", async () => {
  const harness = await installOmpWithTerminalInput("omp-instruct-none", undefined);
  await harness.handlers.get("session_start")?.({ reason: "startup" }, harness.context);
  await waitFor(() => harness.reports().length === 1);

  expect(harness.reports()[0].params.accepts_instructions).toBe(false);
});
