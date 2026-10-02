// installed by herdr
// managed by herdr; reinstalling or updating the integration overwrites this file.
// add custom hooks/plugins beside this file instead of editing it.
// HERDR_INTEGRATION_ID=omp
// HERDR_INTEGRATION_VERSION=12
// @ts-nocheck

import net from "node:net";
import path from "node:path";

const HERDR_ENV = process.env.HERDR_ENV;
const socketPath = process.env.HERDR_SOCKET_PATH;
const socketEndpoint =
  process.platform === "win32" && socketPath ? `\\\\.\\pipe\\${socketPath}` : socketPath;
const paneId = process.env.HERDR_PANE_ID;
const source = "herdr:omp";
// OMP marks every shell it spawns with OMPCODE=1. A nested `omp` launched from
// a parent session's shell inherits it, so that process is not the pane's root
// agent and must not report its short-lived session over the parent's.
const nestedOmpSession = process.env.OMPCODE === "1";

function enabled() {
  return HERDR_ENV === "1" && !!socketPath && !!paneId && !nestedOmpSession;
}

let requestQueue: Promise<boolean> = Promise.resolve(true);

function sendRequestAttempt(request: any, timeoutMs: number): Promise<boolean> {
  if (!enabled()) {
    return Promise.resolve(true);
  }

  return new Promise((resolve) => {
    let done = false;
    let response = "";
    let timeout: ReturnType<typeof setTimeout> | undefined;
    const finish = (delivered: boolean) => {
      if (done) return;
      done = true;
      if (timeout) clearTimeout(timeout);
      socket.destroy();
      resolve(delivered);
    };

    const socket = net.createConnection(socketEndpoint!);
    socket.setEncoding("utf8");
    socket.on("error", () => finish(false));
    socket.on("connect", () => socket.write(`${JSON.stringify(request)}\n`));
    socket.on("data", (chunk) => {
      response += chunk;
      const newline = response.indexOf("\n");
      if (newline < 0) return;
      try {
        const parsed = JSON.parse(response.slice(0, newline));
        finish(parsed?.id === request.id && parsed?.result?.type === "ok");
      } catch {
        finish(false);
      }
    });
    socket.on("end", () => finish(false));
    timeout = setTimeout(() => finish(false), timeoutMs);
    timeout.unref?.();
  });
}

async function sendRequestNow(buildRequest: () => unknown): Promise<boolean> {
  if (await sendRequestAttempt(buildRequest(), 500)) return true;
  // A retry of the same wire object turns a lost-then-applied first attempt
  // into a stale duplicate, so each attempt builds a fresh request with a new
  // id and seq.
  return sendRequestAttempt(buildRequest(), 1500);
}

function sendRequest(buildRequest: () => unknown): Promise<boolean> {
  requestQueue = requestQueue.then(
    () => sendRequestNow(buildRequest),
    () => sendRequestNow(buildRequest),
  );
  return requestQueue;
}

type AgentState = "working" | "blocked" | "idle";

type QueuedState = {
  state: AgentState;
  message?: string;
};

const idleDebounceMs = parseDurationEnv("HERDR_OMP_IDLE_DEBOUNCE_MS", 250);
const retryGraceMs = parseDurationEnv("HERDR_OMP_RETRY_GRACE_MS", 2500);
// An unacknowledged session report (e.g. the server was too loaded to read it) is re-sent
// with doubling delays, so a resumed session registers without waiting for the next prompt.
const sessionRetryBaseMs = parseDurationEnv("HERDR_OMP_SESSION_RETRY_MS", 1000);
const sessionRetryLimit = 8;
let sessionRetryTimer: ReturnType<typeof setTimeout> | undefined;
const retryableErrorPattern =
  /overloaded|provider.?returned.?error|rate.?limit|too many requests|429|500|502|503|504|service.?unavailable|server.?error|internal.?error|network.?error|connection.?error|connection.?refused|connection.?lost|websocket.?closed|websocket.?error|other side closed|fetch failed|upstream.?connect|reset before headers|socket hang up|ended without|http2 request did not get a response|timed? out|timeout|terminated|retry delay/i;
let reportSeq = Date.now() * 1000;
let currentAgentSessionId: string | undefined;
let currentAgentSessionPath: string | undefined;
let registeredSessionKey: string | undefined;
const launchProfile = (process.env.OMP_PROFILE ?? process.env.PI_PROFILE)?.trim() || "default";
// Identifies this JS runtime. An extension reload keeps it (same globalThis), so herdr keeps
// following deliveries the running prompt still owns; an exec restart (`/restart`, same pid
// and start time) starts a new runtime, so herdr knows the old deliveries are gone.
const runtimeSlot = globalThis as { [key: symbol]: string | undefined };
const RUNTIME_KEY = Symbol.for("herdr.omp.runtime");
const runtimeInstance = (runtimeSlot[RUNTIME_KEY] ??= crypto.randomUUID());
// A herdr `agent.instruct` delivery: one bracketed paste with no Enter. The header carries the
// instruction id, the unix ms after which the block must be discarded, and the text's UTF-8
// byte length. OMP hands a paste to input listeners as one string, plus an Enter typed in the
// same read.
const INSTRUCTION =
  /^\x1b\[200~herdr-instruction:v2:([0-9a-f]{32}):(\d+):(\d+)\n([\s\S]*)\x1b\[201~(\r\n|\r|\n)?$/;
// herdr follows a taken delivery this long; a later ack only gets instruction_not_found.
const admissionWaitMs = 120_000;
// How often an idle delivery checks whether OMP went idle again without starting its turn.
const admissionPollMs = parseDurationEnv("HERDR_OMP_INSTRUCTION_POLL_MS", 250);
// A model mention, as OMP's editor finds it. OMP rewrites a mention of a known model into an
// agent tag before the turn starts.
const MODEL_MENTION = /(^|\s)\^[^\s^]+(?=\s|$)/g;
let instructionListener = false;
let unsubscribeInstructions: (() => void) | undefined;
// Idle deliveries handed to OMP and acked as taken, acked again when OMP starts their turn or
// goes idle without starting it.
let awaitingAdmission: {
  instructionId: string;
  turnText: RegExp;
  idlePolls: number;
  until: number;
}[] = [];
let admissionTimer: ReturnType<typeof setInterval> | undefined;
// A compaction holds new prompts while the session looks idle.
let compacting = false;

// `pending`: OMP took the text and prepares its turn. `prompt`: the turn started. `aside`:
// queued for the running turn. `dropped`: OMP went idle without starting the turn, so nothing
// ran. Sent outside the report queue, so an ack never waits behind state reports.
function ackInstruction(
  instructionId: string,
  outcome: "pending" | "prompt" | "aside" | "dropped",
): void {
  void sendRequestNow(() => ({
    id: `${source}:instruction:${instructionId}:${outcome}:${Date.now()}`,
    method: "pane.ack_instruction",
    params: {
      pane_id: paneId,
      instruction_id: instructionId,
      agent_pid: process.pid,
      outcome,
    },
  }));
}

// Matches the text of the user message OMP starts the turn with: the delivered text, with each
// model mention allowed to have been rewritten.
function turnTextPattern(text: string): RegExp {
  const escape = (part: string) => part.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  let pattern = "";
  let last = 0;
  for (const mention of text.matchAll(MODEL_MENTION)) {
    const start = mention.index + mention[1].length;
    pattern += `${escape(text.slice(last, start))}[\\s\\S]*?`;
    last = start + mention[0].length - mention[1].length;
  }
  return new RegExp(`^${pattern}${escape(text.slice(last))}$`);
}

function stopAdmissionWatch(): void {
  clearInterval(admissionTimer);
  admissionTimer = undefined;
  awaitingAdmission = [];
}

function nextReportSeq(): number {
  reportSeq += 1;
  return reportSeq;
}

export function isAbsoluteSessionPath(file: unknown): file is string {
  return (
    typeof file === "string" &&
    (path.posix.isAbsolute(file) || path.win32.isAbsolute(file))
  );
}

function updateSessionRef(ctx: any): void {
  const previousKey = currentSessionKey();
  try {
    const file = ctx?.sessionManager?.getSessionFile?.();
    currentAgentSessionPath = isAbsoluteSessionPath(file) ? file : undefined;
  } catch {
    currentAgentSessionPath = undefined;
  }

  try {
    const id = ctx?.sessionManager?.getSessionId?.();
    currentAgentSessionId = typeof id === "string" && id.length > 0 ? id : undefined;
  } catch {
    currentAgentSessionId = undefined;
  }
  if (currentSessionKey() !== previousKey) registeredSessionKey = undefined;
}

function currentSessionKey(): string | undefined {
  if (currentAgentSessionPath) return `path\0${currentAgentSessionPath}`;
  if (currentAgentSessionId) return `id\0${currentAgentSessionId}`;
  return undefined;
}

function withRegisteredSessionRef(params: Record<string, unknown>): Record<string, unknown> {
  if (registeredSessionKey !== currentSessionKey()) return params;
  if (currentAgentSessionPath) return { ...params, agent_session_path: currentAgentSessionPath };
  if (currentAgentSessionId) return { ...params, agent_session_id: currentAgentSessionId };
  return params;
}

function parseDurationEnv(name: string, fallback: number): number {
  const raw = process.env[name];
  if (!raw) {
    return fallback;
  }
  const parsed = Number.parseInt(raw, 10);
  if (!Number.isFinite(parsed) || parsed < 0) {
    return fallback;
  }
  return parsed;
}

function currentSessionRef(): Record<string, unknown> | undefined {
  if (currentAgentSessionPath) {
    return { agent_session_path: currentAgentSessionPath };
  }
  if (currentAgentSessionId) {
    return { agent_session_id: currentAgentSessionId };
  }
  return undefined;
}

async function reportSession(sessionStartSource = "startup", attempt = 0): Promise<void> {
  const sessionRef = currentSessionRef();
  const sessionKey = currentSessionKey();
  registeredSessionKey = undefined;
  clearTimeout(sessionRetryTimer);
  sessionRetryTimer = undefined;
  if (!sessionRef || !sessionKey) return;

  // Each retry attempt mints a fresh id and sequence number. If the server
  // applied the first attempt but its reply was lost, an identical retry
  // would be rejected as stale and leave the session unregistered with no
  // later session event to recover it. A fresh retry replays the same
  // profile, PID, and session through every server check and converges.
  const delivered = await sendRequest(() => ({
    id: `${source}:session:${Date.now()}:${Math.random().toString(36).slice(2)}`,
    method: "pane.report_agent_session_v2",
    params: {
      pane_id: paneId,
      source,
      agent: "omp",
      seq: nextReportSeq(),
      session_start_source: sessionStartSource,
      launch_profile: launchProfile,
      agent_pid: process.pid,
      accepts_instructions: instructionListener,
      runtime_instance: runtimeInstance,
      ...sessionRef,
    },
  }));
  if (delivered && currentSessionKey() === sessionKey) {
    registeredSessionKey = sessionKey;
  } else if (!delivered && currentSessionKey() === sessionKey && attempt < sessionRetryLimit) {
    sessionRetryTimer = setTimeout(() => {
      sessionRetryTimer = undefined;
      if (currentSessionKey() === sessionKey && registeredSessionKey !== sessionKey) {
        void reportSession(sessionStartSource, attempt + 1);
      }
    }, Math.min(sessionRetryBaseMs * 2 ** attempt, 30_000));
    sessionRetryTimer.unref?.();
  }
}

async function sendState(state: AgentState, message?: string): Promise<void> {
  await requestQueue;
  // The server keeps one seq watermark per source across report methods, so
  // the seq is minted when the queue sends the request, never when it is
  // queued. Every attempt, retries included, gets a seq above anything sent
  // before it. The session ref is fixed at the first attempt: a retry must
  // not drop a ref that was registered when this report was first sent.
  let params: Record<string, unknown> | undefined;
  await sendRequest(() => {
    params ??= withRegisteredSessionRef({ pane_id: paneId, source, agent: "omp", state, message });
    return {
      id: `${source}:${Date.now()}:${Math.random().toString(36).slice(2)}`,
      method: "pane.report_agent",
      params: { ...params, seq: nextReportSeq() },
    };
  });
}

let sendInFlight = false;
let queuedState: QueuedState | undefined;

function queueState(state: AgentState, message?: string): void {
  queuedState = { state, message };
  if (!sendInFlight) {
    void drainStateQueue();
  }
}

async function drainStateQueue(): Promise<void> {
  if (sendInFlight) {
    return;
  }

  sendInFlight = true;
  try {
    while (queuedState) {
      const next = queuedState;
      queuedState = undefined;
      await sendState(next.state, next.message);
    }
  } finally {
    sendInFlight = false;
    if (queuedState) {
      void drainStateQueue();
    }
  }
}

function lastAssistantMessage(messages: unknown[]): any | undefined {
  for (let i = messages.length - 1; i >= 0; i -= 1) {
    const message = messages[i] as any;
    if (message?.role === "assistant") {
      return message;
    }
  }
  return undefined;
}

function retryableErrorMessage(event: any): string | undefined {
  const messages = Array.isArray(event?.messages) ? event.messages : [];
  const assistant = lastAssistantMessage(messages);
  if (assistant?.stopReason !== "error") {
    return undefined;
  }

  const errorMessage = String(assistant.errorMessage ?? "");
  if (!retryableErrorPattern.test(errorMessage)) {
    return undefined;
  }
  return errorMessage || "retryable provider error";
}

function askBlockedMessage(args: any): string {
  const questions = Array.isArray(args?.questions) ? args.questions : [];
  const firstQuestion = questions.find((question: any) => typeof question?.question === "string");
  if (firstQuestion?.question) {
    return firstQuestion.question;
  }
  return "waiting for user input";
}

export default function (pi) {
  if (!enabled()) {
    return;
  }

  let agentActive = false;
  let retryHoldActive = false;
  let failureBlocked = false;
  let failureMessage: string | undefined;
  let blockedCount = 0;
  let blockedMessage: string | undefined;
  let lastState: AgentState | undefined;
  let lastMessage: string | undefined;
  let idleTimer: ReturnType<typeof setTimeout> | undefined;
  let retryTimer: ReturnType<typeof setTimeout> | undefined;
  let rootSession = false;

  function clearTimer(timer: ReturnType<typeof setTimeout> | undefined) {
    if (timer) {
      clearTimeout(timer);
    }
  }

  function clearPendingTimers() {
    clearTimer(idleTimer);
    clearTimer(retryTimer);
    idleTimer = undefined;
    retryTimer = undefined;
  }

  function clearFailureState() {
    retryHoldActive = false;
    failureBlocked = false;
    failureMessage = undefined;
  }

  function desiredState() {
    if (blockedCount > 0) {
      return { state: "blocked" as const, message: blockedMessage };
    }
    if (failureBlocked) {
      return { state: "blocked" as const, message: failureMessage };
    }
    if (agentActive || retryHoldActive) {
      return { state: "working" as const, message: undefined };
    }
    return { state: "idle" as const, message: undefined };
  }

  function publishState(force = false) {
    const next = desiredState();
    if (!force && next.state === lastState && next.message === lastMessage) {
      return;
    }
    lastState = next.state;
    lastMessage = next.message;
    queueState(next.state, next.message);
  }

  function scheduleIdle() {
    clearPendingTimers();
    clearFailureState();
    idleTimer = setTimeout(() => {
      idleTimer = undefined;
      publishState();
    }, idleDebounceMs);
    idleTimer.unref?.();
  }

  function holdForRetry(message: string) {
    clearPendingTimers();
    retryHoldActive = true;
    failureBlocked = false;
    failureMessage = message;
    publishState();

    retryTimer = setTimeout(() => {
      retryTimer = undefined;
      retryHoldActive = false;
      failureBlocked = true;
      publishState();
    }, retryGraceMs);
    retryTimer.unref?.();
  }

  // Runs before OMP's editor and dialogs, so a delivery never mixes with a person's draft or
  // picks a dialog item. sendUserMessage bypasses `/`, `!` and `$` command handling.
  function takeInstruction(ctx: { isIdle?: () => boolean }, data: string) {
    const match = INSTRUCTION.exec(data);
    if (!match) {
      return undefined;
    }
    const [, instructionId, expiresMs, byteLength, text, enter] = match;
    const rest = enter ? { data: enter } : { consume: true };
    // herdr has given up on a late block, and a paste OMP cut short is not the whole text: drop
    // both, unsent and unacked. The report tells herdr that the listener still exists.
    if (Date.now() > Number(expiresMs) || Buffer.byteLength(text, "utf8") !== Number(byteLength)) {
      void reportSession();
      return rest;
    }
    const idle = ctx?.isIdle?.() === true;
    try {
      // Always an aside: if a turn starts before OMP dispatches the text, it waits for the next
      // step instead of becoming a steer that aborts the running tool batch; on an idle session
      // an aside starts a turn. Not awaited: an idle send resolves only after the whole turn.
      pi.sendUserMessage(text, { deliverAs: "aside" })?.catch?.(() => {});
    } catch {
      // No ack: the caller reports the delivery as unconfirmed.
      return rest;
    }
    if (!idle) {
      ackInstruction(instructionId, "aside");
      return rest;
    }
    // OMP may spend a long time before the turn starts (compaction, hooks), and shows a failed
    // idle send (no model, no API key) only on screen. So the take is acked at once, which tells
    // herdr the text must never be sent again, and the turn start is acked when it happens.
    ackInstruction(instructionId, "pending");
    awaitingAdmission.push({
      instructionId,
      turnText: turnTextPattern(text),
      idlePolls: 0,
      until: Date.now() + admissionWaitMs,
    });
    watchAdmissions(ctx);
    return rest;
  }

  // OMP's send gives no result to an extension. A send it drops (no model or API key, usage
  // limit, Esc during preparation, session change) leaves the session idle without a turn:
  // that, seen on two polls in a row outside a manual compaction, is a drop.
  function watchAdmissions(ctx: { isIdle?: () => boolean; hasPendingMessages?: () => boolean }) {
    if (admissionTimer) {
      return;
    }
    admissionTimer = setInterval(() => {
      const now = Date.now();
      const idle =
        !compacting && ctx?.isIdle?.() === true && ctx?.hasPendingMessages?.() !== true;
      awaitingAdmission = awaitingAdmission.filter((entry) => {
        entry.idlePolls = idle ? entry.idlePolls + 1 : 0;
        if (entry.idlePolls >= 2) {
          ackInstruction(entry.instructionId, "dropped");
          return false;
        }
        return entry.until > now;
      });
      if (awaitingAdmission.length === 0) {
        stopAdmissionWatch();
      }
    }, admissionPollMs);
    admissionTimer.unref?.();
  }

  function registerInstructionListener(ctx: {
    isIdle?: () => boolean;
    ui?: { onTerminalInput?: (handler: (data: string) => unknown) => unknown };
  }) {
    // OMP drops extension input listeners on every session change (/new, /resume, branch,
    // reload), sometimes without a session event, so each activation and each turn start
    // registers a fresh one.
    unsubscribeInstructions?.();
    unsubscribeInstructions = ctx.ui?.onTerminalInput?.((data: string) =>
      takeInstruction(ctx, data),
    );
    instructionListener = typeof unsubscribeInstructions === "function";
  }

  function activateRootSession(ctx: any, sessionStartSource = "startup"): boolean {
    if (ctx?.hasUI !== true) {
      return false;
    }
    rootSession = true;
    // A new or changed session has no compaction of its own running.
    compacting = false;
    registerInstructionListener(ctx);
    updateSessionRef(ctx);
    void reportSession(sessionStartSource);
    return true;
  }

  function resetSessionState() {
    clearPendingTimers();
    clearFailureState();
    agentActive = false;
    blockedCount = 0;
    blockedMessage = undefined;
  }

  function activateBlocked(message: string | undefined) {
    clearPendingTimers();
    blockedCount += 1;
    blockedMessage = message;
    publishState();
  }

  function deactivateBlocked() {
    blockedCount = Math.max(0, blockedCount - 1);
    if (blockedCount === 0) {
      blockedMessage = undefined;
    }
    publishState();
  }

  pi.events.on("herdr:blocked", (data) => {
    if (!rootSession) {
      return;
    }
    if (!data?.active) {
      deactivateBlocked();
      return;
    }

    activateBlocked(data.label);
  });

  pi.on("session_start", (_event, ctx) => {
    if (!activateRootSession(ctx)) {
      return;
    }
    // A reload can replace this extension mid-run without emitting another agent_start.
    agentActive = ctx?.isIdle?.() === false;
    publishState(true);
  });

  // Only notification events: any `session_before_compact` handler makes OMP treat the
  // extension as one that may veto compaction, which turns off its background (speculative)
  // compaction. Automatic compactions announce start and end, including an Esc abort. A manual
  // `/compact` announces nothing before `session.compacting`, which also comes from background
  // speculation, so session changes and turn starts clear the flag too.
  //
  // The instruction state (listener, awaited deliveries, compaction hold) is module-level, and
  // OMP binds this same module again for every task subagent in the process. Each handler that
  // touches it acts only for the root session, so a subagent's events never change it.
  pi.on("auto_compaction_start", () => {
    if (rootSession) {
      compacting = true;
    }
  });
  // Background speculation also emits it, from inside a turn; only a manual compaction emits it
  // while the session is idle (its abort ended the turn), so only then does it hold prompts.
  pi.on("session.compacting", (_event, ctx) => {
    if (rootSession && ctx?.isIdle?.() === true) {
      compacting = true;
    }
  });
  for (const event of ["auto_compaction_end", "session_compact"]) {
    pi.on(event, () => {
      if (rootSession) {
        compacting = false;
      }
    });
  }

  // Branch and tree navigation end without session_switch, after OMP dropped the listener.
  for (const event of ["session_branch", "session_tree"]) {
    pi.on(event, (_event, ctx) => {
      activateRootSession(ctx, "branch");
    });
  }

  pi.on("message_start", (event) => {
    const message = event?.message;
    if (!rootSession || awaitingAdmission.length === 0 || message?.role !== "user") {
      return;
    }
    const content = message.content;
    const text =
      typeof content === "string"
        ? content
        : Array.isArray(content)
          ? content
              .filter((part) => part?.type === "text")
              .map((part) => part.text)
              .join("\n")
          : undefined;
    // Only the delivered text starts its turn; a person's own prompt never claims it.
    const index =
      typeof text === "string"
        ? awaitingAdmission.findIndex((entry) => entry.turnText.test(text))
        : -1;
    if (index >= 0) {
      ackInstruction(awaitingAdmission.splice(index, 1)[0].instructionId, "prompt");
    }
  });

  pi.on("session_switch", (event, ctx) => {
    if (!activateRootSession(ctx, event?.reason || "resume")) {
      return;
    }
    resetSessionState();
    publishState(true);
  });

  pi.on("agent_start", (_event, ctx) => {
    if (ctx?.hasUI !== true) {
      return;
    }
    if (!rootSession && !activateRootSession(ctx)) {
      return;
    }
    compacting = false;
    registerInstructionListener(ctx);
    updateSessionRef(ctx);
    void reportSession();
    clearPendingTimers();
    clearFailureState();
    agentActive = true;
    publishState();
  });

  pi.on("tool_approval_requested", (event, ctx) => {
    if (!rootSession && !activateRootSession(ctx)) {
      return;
    }
    const label = event?.reason || `${event?.toolName || "Tool"} approval`;
    activateBlocked(label);
  });

  pi.on("tool_approval_resolved", (_event, ctx) => {
    if (!rootSession && !activateRootSession(ctx)) {
      return;
    }
    deactivateBlocked();
  });

  pi.on("tool_execution_start", (event, ctx) => {
    if (event?.toolName !== "ask") {
      return;
    }
    if (!rootSession && !activateRootSession(ctx)) {
      return;
    }
    activateBlocked(askBlockedMessage(event.args));
  });

  pi.on("tool_execution_end", (event, ctx) => {
    if (event?.toolName !== "ask") {
      return;
    }
    if (!rootSession && !activateRootSession(ctx)) {
      return;
    }
    deactivateBlocked();
  });

  pi.on("agent_end", (event) => {
    if (!rootSession) {
      return;
    }
    if (!agentActive) {
      // OMP can emit duplicate/late end events while auto-retry is already
      // holding the pane in Working. Do not let an unqualified duplicate end
      // cancel the retry hold and publish a false Idle.
      return;
    }
    if (event?.willContinue === true) {
      // A continuation is already scheduled, so this end is not a settle.
      // Older builds omit the field and fall through as before.
      return;
    }

    agentActive = false;

    const retryableMessage = retryableErrorMessage(event);
    if (retryableMessage) {
      holdForRetry(retryableMessage);
      return;
    }

    scheduleIdle();
  });

  pi.on("session_shutdown", () => {
    if (!rootSession) {
      return;
    }
    clearPendingTimers();
    unsubscribeInstructions?.();
    unsubscribeInstructions = undefined;
    instructionListener = false;
    stopAdmissionWatch();
    compacting = false;
  });
}
