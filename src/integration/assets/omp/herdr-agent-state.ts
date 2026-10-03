// installed by herdr
// managed by herdr; reinstalling or updating the integration overwrites this file.
// add custom hooks/plugins beside this file instead of editing it.
// HERDR_INTEGRATION_ID=omp
// HERDR_INTEGRATION_VERSION=14
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
// herdr sets TERM_PROGRAM=herdr in its panes. A terminal or multiplexer started inside a pane
// (tern, tmux) inherits the HERDR_* variables but sets its own TERM_PROGRAM, and an OMP running
// there is not the agent of the pane those variables name.
const inHerdrPane = process.env.TERM_PROGRAM === "herdr";

function enabled() {
  return (
    HERDR_ENV === "1" && !!socketPath && !!paneId && !nestedOmpSession && inHerdrPane
  );
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
// The shutdown report's single attempt; OMP gives a shutdown handler at most 2 s.
const shutdownReportTimeoutMs = 1000;
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
// instruction id, the unix ms after which the block must be discarded, the text's UTF-8 byte
// length, and this runtime's token, which only herdr learns (over the peer-checked socket): a
// person's paste that imitates a block lacks it and reaches the editor as a paste. OMP hands a
// paste to input listeners as one string, plus an Enter typed in the same read.
const INSTRUCTION =
  /^\x1b\[200~herdr-instruction:v3:([0-9a-f]{32}):(\d+):(\d+):([0-9A-Za-z-]{1,64})\n([\s\S]*)\x1b\[201~(\r\n|\r|\n)?$/;
// A herdr `agent.action` block: the same framing and token rules, around `{"op","args"}` JSON.
const ACTION =
  /^\x1b\[200~herdr-action:v1:([0-9a-f]{32}):(\d+):(\d+):([0-9A-Za-z-]{1,64})\n([\s\S]*)\x1b\[201~(\r\n|\r|\n)?$/;
// herdr sets the expiry a few seconds ahead; a block claiming a later one is not herdr's.
const instructionExpiryCapMs = 10_000;
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

// The result of an `agent.action`. `error` starts with a reason token. An ack with `keys` (a
// dialog answer) is not final: herdr writes the keys, at most two chunks with a short pause between
// them, and waits for a final ack without keys.
function ackAction(
  actionId: string,
  result: { ok: boolean; error?: string; data?: Record<string, unknown>; keys?: string[] },
  // A final result goes through the report queue, behind the detail report sent just before it,
  // so the `agent` herdr answers with shows the action's effect.
  queued = false,
): Promise<boolean> {
  return (queued ? sendRequest : sendRequestNow)(() => ({
    id: `${source}:action:${actionId}:${Date.now()}`,
    method: "pane.ack_action",
    params: {
      pane_id: paneId,
      action_id: actionId,
      agent_pid: process.pid,
      ok: result.ok,
      ...(result.error ? { error: result.error } : {}),
      data: result.data ?? {},
      ...(result.keys?.length ? { keys: result.keys } : {}),
    },
  }));
}

// Cuts by code point, so a cap never splits a surrogate pair (herdr refuses a lone surrogate).
function cap(text: unknown, max = 500): string {
  const value = typeof text === "string" ? text : String(text ?? "");
  if (value.length <= max) return value;
  const points = Array.from(value);
  return points.length > max ? `${points.slice(0, max - 1).join("")}…` : value;
}

// Mirrors OMP's getLatestTodoPhasesFromEntries (tools/todo.ts): the newest todo tool result, or
// the `user_todo_edit` entry the eval bridge persists for `tool.todo`.
function entryPhases(entry: any): unknown[] | undefined {
  if (entry?.type === "custom" && entry.customType === "user_todo_edit") {
    return Array.isArray(entry.data?.phases) ? entry.data.phases : undefined;
  }
  const message = entry?.type === "message" ? entry.message : undefined;
  if (
    message?.role !== "toolResult" ||
    message.toolName !== "todo" ||
    message.isError ||
    message.details?.op === "view"
  ) {
    return undefined;
  }
  return Array.isArray(message.details?.phases) ? message.details.phases : undefined;
}

function sessionEntries(ctx: any): any[] {
  try {
    const entries = ctx?.sessionManager?.getBranch?.() ?? ctx?.sessionManager?.getEntries?.();
    return Array.isArray(entries) ? entries : [];
  } catch {
    return [];
  }
}

function todoSummary(ctx: any) {
  const entries = sessionEntries(ctx);
  let phases: any[] | undefined;
  for (let i = entries.length - 1; i >= 0 && !phases; i -= 1) {
    phases = entryPhases(entries[i]) as any[] | undefined;
  }
  if (!phases) return undefined;
  const tasksOf = (phase: any) => (Array.isArray(phase?.tasks) ? phase.tasks : []);
  const counted = (tasks: any[]) => tasks.filter((task) => task?.status !== "abandoned");
  const done = (tasks: any[]) => tasks.filter((task) => task?.status === "completed").length;
  const tasks = phases.flatMap(tasksOf);
  const current =
    tasks.find((task) => task?.status === "in_progress") ??
    tasks.find((task) => task?.status === "pending");
  return {
    total: counted(tasks).length,
    done: done(tasks),
    ...(typeof current?.content === "string" ? { current: cap(current.content) } : {}),
    phases: phases.slice(0, 20).map((phase) => ({
      name: cap(phase?.name, 120),
      total: counted(tasksOf(phase)).length,
      done: done(tasksOf(phase)),
    })),
  };
}

// OMP's thinking selectors (`auto`, `off` and the effort levels).
const THINKING_LEVELS: Record<string, true> = {
  auto: true,
  off: true,
  minimal: true,
  low: true,
  medium: true,
  high: true,
  xhigh: true,
  max: true,
};
// A collapsed large paste in OMP's editor; its text lives outside the editor buffer, so a draft
// that holds one cannot be saved and restored as text.
const PASTE_MARKER = /\[Paste #\d+(?:, (?:\+\d+ lines|\d+ chars))?\]/;
// A herdr answer key: one bracketed paste per key, which the listener replaces with that key's
// bytes. The header carries the action id, the key's place in the answer, an expiry and this
// runtime's token, so no key a person types is ever taken for herdr's. The body is one key code,
// or `T` and a custom answer text.
const KEY_BLOCK =
  /^\x1b\[200~herdr-key:v1:([0-9a-f]{32}):(\d+):(\d+):([0-9A-Za-z-]{1,64})\n([\s\S]*)\x1b\[201~(\r\n|\r|\n)?$/;
const KEY_BYTES: Record<string, string> = { U: "\x1b[A", D: "\x1b[B", E: "\r", S: " ", X: "\x1b" };
// The detail herdr keeps is at most 16 KiB; a dialog report shrinks to stay under this.
const detailBudgetBytes = 15 * 1024;
// Dialog report sizes, largest first: questions, options per question, characters per text.
const DIALOG_TIERS = [
  { questions: 10, options: 20, text: 500, label: 200 },
  { questions: 10, options: 20, text: 160, label: 60 },
  { questions: 10, options: 20, text: 60, label: 24 },
  { questions: 5, options: 10, text: 40, label: 16 },
  { questions: 1, options: 5, text: 40, label: 12 },
];
// How long an answer waits for its dialog to close; herdr waits 8 s for the final ack.
const answerWaitMs = 5000;
// How long OMP may take to report that a dialog closed after a person's key.
const closeReportWaitMs = 300;

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

// A session report with a fresh id and sequence number, built when it is sent.
function sessionReport(sessionStartSource: string, sessionRef: Record<string, unknown>) {
  return {
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
      accepts_actions: instructionListener,
      runtime_instance: runtimeInstance,
      ...sessionRef,
    },
  };
}

// Set by the root session's binding: queues an `omp` detail report (at once when `now`).
let reportDetailSoon: ((now?: boolean) => void) | undefined;

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
  const delivered = await sendRequest(() => sessionReport(sessionStartSource, sessionRef));
  if (delivered && currentSessionKey() === sessionKey) {
    registeredSessionKey = sessionKey;
    // herdr drops the detail of a listener it replaces.
    if (instructionListener) reportDetailSoon?.(true);
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
    const [, instructionId, expiresMs, byteLength, token, text, enter] = match;
    if (token !== runtimeInstance || Number(expiresMs) > Date.now() + instructionExpiryCapMs) {
      return undefined;
    }
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

  // ---- Live detail (`AgentInfo.omp`) and structured actions (`agent.action`). ----

  // The latest root-session context, for the detail fields an event does not carry.
  let detailCtx: any;
  let detailTimer: ReturnType<typeof setTimeout> | undefined;
  let lastDetailAt = 0;
  // Tools the root session started and has not finished, oldest first.
  const runningTools = new Map<string, { name: string; call_id: string; started_ms: number }>();
  // The input each tool call runs with, by call id. OMP asks for approval after
  // `tool_execution_start`, whose `args` hold the input after every extension's `tool_call`
  // revision; `tool_call` input (before revisions) is kept only for calls the agent loop did not
  // start (nested dispatches emit no `tool_execution_start`).
  const toolInputs = new Map<string, unknown>();
  type OpenDialog = {
    id: string;
    kind: "approval" | "ask";
    tool: string;
    summary?: string;
    questions: { text: string; options: string[]; multi: boolean; recommended: number }[];
    // The last report showed fewer questions or options than the ask has.
    truncated: boolean;
    // A key that was not herdr's reached OMP while this dialog was on screen.
    touched: boolean;
  };
  // Open approval and `ask` dialogs, oldest first. OMP shows one dialog at a time and queues the
  // rest first in, first out, so the head is the one on screen.
  let dialogs: OpenDialog[] = [];
  // Dialogs that closed lately, so a late answer says it lost the race.
  const closedDialogs: string[] = [];
  // A person's draft cleared so an `ask` dialog takes keys; restored when the answer ends.
  let dialogDraft: { id: string; text: string } | undefined;
  // The answer in progress. herdr writes its key blocks after the first ack; each one reaches
  // OMP only in order, while the dialog is still the head and no person key came first. The
  // final ack goes out when the dialog closes, or when the answer can no longer finish.
  let answering:
    | {
        actionId: string;
        dialogId: string;
        codes: string[];
        next: number;
        spoiled: boolean;
        approve?: boolean;
        finish: (result: { ok: boolean; error?: string; data?: Record<string, unknown> }) => void;
        timer: ReturnType<typeof setTimeout>;
      }
    | undefined;
  // Keys OMP passes to extension listeners that never act on a dialog: focus reports and mouse.
  const TERMINAL_NOISE = /^\x1b\[(?:I|O|<[\d;]+[mM])$/;

  function dialogReport(open: OpenDialog, tier: (typeof DIALOG_TIERS)[number]) {
    const truncated =
      open.questions.length > tier.questions ||
      open.questions.some((question) => question.options.length > tier.options);
    return {
      truncated,
      report: {
        id: open.id,
        kind: open.kind,
        tool: open.tool,
        ...(open.summary !== undefined ? { summary: cap(open.summary, tier.text) } : {}),
        ...(open.kind === "ask"
          ? {
              questions: open.questions.slice(0, tier.questions).map((question) => ({
                text: cap(question.text, tier.text),
                options: question.options.slice(0, tier.options).map((option) => cap(option, tier.label)),
                multi: question.multi,
                // OMP 18.4.4 always offers "Other (type your own)".
                other_allowed: true,
              })),
            }
          : {}),
        ...(truncated ? { truncated: true } : {}),
        ...(dialogs.length > 1 ? { queued: dialogs.length - 1 } : {}),
      },
    };
  }

  function buildDetail() {
    const ctx = detailCtx;
    const detail: Record<string, unknown> = { updated_ms: Date.now() };
    const model = ctx?.model;
    if (model?.provider && model?.id) detail.model = cap(`${model.provider}/${model.id}`, 200);
    try {
      const thinking = pi.getThinkingLevel?.();
      if (typeof thinking === "string") detail.thinking = thinking;
    } catch {}
    const tool = [...runningTools.values()].at(-1);
    if (tool) detail.tool = tool;
    try {
      const usage = ctx?.getContextUsage?.();
      if (Number.isFinite(usage?.tokens) && Number.isFinite(usage?.contextWindow)) {
        detail.context = {
          tokens: Math.max(0, Math.round(usage.tokens)),
          window: Math.max(0, Math.round(usage.contextWindow)),
          percent: Math.max(0, Math.round(Number(usage.percent) || 0)),
        };
      }
    } catch {}
    const todos = todoSummary(ctx);
    if (todos) detail.todos = todos;
    const head = dialogs[0];
    if (head) {
      // The smallest report that fits herdr's limit; an answer refuses what it had to cut.
      for (const tier of DIALOG_TIERS) {
        const { truncated, report } = dialogReport(head, tier);
        detail.dialog = report;
        head.truncated = truncated;
        if (Buffer.byteLength(JSON.stringify(detail), "utf8") <= detailBudgetBytes) break;
      }
    }
    return detail;
  }

  function sendDetail() {
    clearTimeout(detailTimer);
    detailTimer = undefined;
    if (!rootSession || !instructionListener || !detailCtx) return;
    lastDetailAt = Date.now();
    void sendRequest(() => ({
      id: `${source}:detail:${Date.now()}:${Math.random().toString(36).slice(2)}`,
      method: "pane.report_omp_detail",
      params: {
        pane_id: paneId,
        agent_pid: process.pid,
        runtime_instance: runtimeInstance,
        omp: buildDetail(),
      },
    }));
  }

  // At most one report a second for usage and tool churn; a dialog reports at once.
  function scheduleDetail(ctx?: any, now = false) {
    if (ctx) detailCtx = ctx;
    if (now) {
      sendDetail();
      return;
    }
    if (detailTimer) return;
    detailTimer = setTimeout(sendDetail, Math.max(0, 1000 - (Date.now() - lastDetailAt)));
    detailTimer.unref?.();
  }

  function openDialog(next: OpenDialog) {
    dialogs.push(next);
    scheduleDetail(undefined, true);
  }

  // Ends the answer in progress with its final result. Later key blocks of it are dropped.
  function endAnswer(result: { ok: boolean; error?: string; data?: Record<string, unknown> }) {
    const current = answering;
    if (!current) return;
    answering = undefined;
    clearTimeout(current.timer);
    if (dialogDraft?.id === current.dialogId && dialogs.some((open) => open.id === current.dialogId)) {
      restoreDraft();
    }
    current.finish(result);
  }

  function restoreDraft() {
    const ui = detailCtx?.ui;
    if (dialogDraft && !(ui?.getEditorText?.() ?? "")) ui?.setEditorText?.(dialogDraft.text);
    dialogDraft = undefined;
  }

  // A person's key reached the dialog before all of herdr's: herdr's remaining keys are dropped.
  // Wait a moment for OMP to report a close the person's key caused.
  function spoilAnswer() {
    if (!answering || answering.spoiled) return;
    answering.spoiled = true;
    clearTimeout(answering.timer);
    answering.timer = setTimeout(
      () =>
        endAnswer({
          ok: false,
          error: "dialog_touched: a person used the dialog while the answer was on the way; herdr did not answer it",
        }),
      closeReportWaitMs,
    );
    answering.timer.unref?.();
  }

  function closeDialog(id: string | undefined, approved?: boolean) {
    const index = dialogs.findIndex((open) => open.id === id);
    if (!id || index < 0) return;
    dialogs.splice(index, 1);
    closedDialogs.push(id);
    closedDialogs.splice(0, closedDialogs.length - 16);
    if (dialogDraft?.id === id) restoreDraft();
    if (answering?.dialogId === id) {
      const all = !answering.spoiled && answering.next === answering.codes.length;
      endAnswer(
        !all
          ? { ok: false, error: "answered_by_other: the dialog closed before herdr's answer reached it" }
          : answering.approve !== undefined && approved !== undefined && approved !== answering.approve
            ? { ok: false, error: "answered_by_other: the dialog closed with the other choice" }
            : { ok: true, data: { dialog_id: id } },
      );
      // The result was sent behind a detail report without this dialog.
      return;
    }
    scheduleDetail(undefined, true);
  }

  // Key codes that choose the answer in OMP 18.4.4's dialogs, for a dialog no person touched (so
  // the cursor is where OMP put it). Cursor moves stop at the first and last rows (no wrap), so
  // one more Up than needed lands on the first row. Approval: Approve, Deny. Ask: each question
  // lists its options, then "Other"; its cursor starts on the recommended option. Enter picks
  // (single) or confirms (multi, Space toggles) and moves to the next question, or submits a
  // one-question dialog; a dialog with several questions ends on a review tab that Enter submits.
  // Esc cancels. "Other" opens an editor that takes focus a moment later, so a custom text goes in
  // a second chunk, and only where nothing follows it: a one-question single-select dialog.
  function answerCodes(open: OpenDialog, args: any): { chunks: string[][] } | { error: string } {
    if (open.kind === "approval") {
      if (typeof args.approve !== "boolean") {
        return { error: "invalid_args: an approval answer needs approve: true or false" };
      }
      return { chunks: [args.approve ? ["U", "E"] : ["D", "E"]] };
    }
    if (args.cancel === true) return { chunks: [["X"]] };
    const questions = open.questions;
    const answers =
      questions.length === 1 && !Array.isArray(args.answers) ? [args] : args.answers;
    if (!Array.isArray(answers) || answers.length !== questions.length) {
      return { error: `invalid_args: give answers, one per question (${questions.length})` };
    }
    const chunks: string[][] = [[]];
    for (const [index, question] of questions.entries()) {
      const answer = answers[index] ?? {};
      const count = question.options.length;
      const kinds = ["option_index", "selections", "text"].filter((key) => answer[key] !== undefined);
      if (kinds.length !== 1) {
        return {
          error: `invalid_args: question ${index + 1} needs exactly one of option_index, selections, text`,
        };
      }
      const start = Math.min(Math.max(question.recommended, 0), Math.max(count - 1, 0));
      const top = Array(start + 1).fill("U");
      const down = (rows: number) => Array(rows).fill("D");
      if (answer.text !== undefined) {
        const text = answer.text;
        if (questions.length !== 1 || question.multi) {
          return { error: "invalid_args: a text answer fits only a one-question single-select dialog" };
        }
        if (typeof text !== "string" || !text.trim() || Array.from(text).length > 1000 || /[\x00-\x1f\x7f]/.test(text)) {
          return { error: "invalid_args: text must be one line of 1-1000 characters" };
        }
        chunks[0].push(...down(count - start), "E");
        chunks.push([`T${text}`, "E"]);
      } else if (answer.option_index !== undefined) {
        const option = answer.option_index;
        if (question.multi || !Number.isInteger(option) || option < 0 || option >= count) {
          return { error: `invalid_args: question ${index + 1} takes option_index 0-${count - 1} only when single-select` };
        }
        chunks[0].push(...top, ...down(option), "E");
      } else {
        const picked = answer.selections;
        if (
          !question.multi ||
          !Array.isArray(picked) ||
          picked.length === 0 ||
          new Set(picked).size !== picked.length ||
          picked.some((option) => !Number.isInteger(option) || option < 0 || option >= count)
        ) {
          return { error: `invalid_args: question ${index + 1} takes distinct selections 0-${count - 1} only when multi-select` };
        }
        const last = Math.max(...picked);
        chunks[0].push(...top);
        for (let row = 0; row <= last; row += 1) {
          if (picked.includes(row)) chunks[0].push("S");
          chunks[0].push(row < last ? "D" : "E");
        }
      }
    }
    if (questions.length > 1) chunks[0].push("E");
    return { chunks };
  }

  // Takes a herdr key block. A block of the answer in progress becomes its key, in order, while
  // the dialog is still the head and no person key came first; every other block with this
  // runtime's token is dropped. A person's Enter read together with the block passes on, and
  // counts as the person's key.
  function takeKey(data: string) {
    const match = KEY_BLOCK.exec(data);
    if (!match) return undefined;
    const [, actionId, seq, expiresMs, token, code, enter] = match;
    if (token !== runtimeInstance) return undefined;
    const current = answering?.actionId === actionId ? answering : undefined;
    const usable =
      current &&
      !current.spoiled &&
      !enter &&
      Number(seq) === current.next &&
      Date.now() <= Number(expiresMs) &&
      dialogs[0]?.id === current.dialogId &&
      current.codes[current.next] === code;
    if (!usable) {
      if (current && current.next < current.codes.length) spoilAnswer();
      if (enter && dialogs[0]) dialogs[0].touched = true;
      return enter ? { data: enter } : { consume: true };
    }
    current.next += 1;
    const bytes = code.startsWith("T") ? `\x1b[200~${code.slice(1)}\x1b[201~` : KEY_BYTES[code];
    return { data: bytes };
  }

  // Runs an `agent.action` block. Returns the input result for OMP: the block is consumed, and an
  // Enter typed in the same read passes on.
  function takeAction(ctx: any, data: string) {
    const match = ACTION.exec(data);
    if (!match) {
      return undefined;
    }
    const [, actionId, expiresMs, byteLength, token, body, enter] = match;
    if (token !== runtimeInstance || Number(expiresMs) > Date.now() + instructionExpiryCapMs) {
      return undefined;
    }
    const rest = enter ? { data: enter } : { consume: true };
    if (Date.now() > Number(expiresMs) || Buffer.byteLength(body, "utf8") !== Number(byteLength)) {
      void reportSession();
      return rest;
    }
    const finish = (result: { ok: boolean; error?: string; data?: Record<string, unknown> }) => {
      scheduleDetail(ctx, true);
      void ackAction(actionId, result, true).then((acked) => {
        // A lost ack withdraws the listener in herdr until the next session report.
        if (!acked) void reportSession();
      });
    };
    const refuse = (error: string) => {
      finish({ ok: false, error });
      return rest;
    };
    let request: any;
    try {
      request = JSON.parse(body);
    } catch {
      return refuse("invalid_args: the action body is not JSON");
    }
    const args = request?.args && typeof request.args === "object" ? request.args : {};
    const idle = ctx?.isIdle?.() === true;
    switch (request?.op) {
      case "abort": {
        // OMP's approval prompt has no abort signal: the turn stays blocked until someone answers.
        // An ask takes the tool's signal and closes when the turn aborts.
        const approval = dialogs.find((open) => open.kind === "approval");
        if (approval) {
          return refuse(
            `dialog_open: approval ${approval.id} is open; deny it first (answer with approve: false)`,
          );
        }
        ctx?.abort?.();
        finish({ ok: true, data: { was_idle: idle } });
        return rest;
      }
      case "set_model": {
        const spec = args.spec;
        if (typeof spec !== "string" || !spec.trim() || spec.length > 200) {
          return refuse("invalid_args: set_model needs spec");
        }
        const model = ctx?.models?.resolve?.(spec.trim());
        if (!model) return refuse(`unknown_model: no available model matches ${cap(spec, 200)}`);
        Promise.resolve(pi.setModel(model)).then(
          (applied) =>
            finish(
              applied
                ? { ok: true, data: { model: `${model.provider}/${model.id}` } }
                : { ok: false, error: `model_unavailable: ${model.provider}/${model.id} has no API key` },
            ),
          (error) => finish({ ok: false, error: `failed: ${cap(error?.message ?? error, 200)}` }),
        );
        return rest;
      }
      case "set_thinking": {
        const level = args.level;
        if (typeof level !== "string" || !Object.hasOwn(THINKING_LEVELS, level)) {
          return refuse(`invalid_level: use one of ${Object.keys(THINKING_LEVELS).join(", ")}`);
        }
        try {
          pi.setThinkingLevel(level);
        } catch (error) {
          return refuse(`failed: ${cap(error?.message ?? error, 200)}`);
        }
        finish({ ok: true, data: { thinking: pi.getThinkingLevel?.() ?? level } });
        return rest;
      }
      case "compact": {
        const instructions = args.instructions;
        if (instructions !== undefined && (typeof instructions !== "string" || instructions.length > 2000)) {
          return refuse("invalid_args: instructions must be a string of at most 2000 characters");
        }
        if (!idle || compacting) return refuse("not_idle: compact only an idle session");
        // Compaction runs a model call; its end shows in the session, not in this result.
        Promise.resolve(ctx?.compact?.(instructions || undefined)).catch(() => {});
        finish({ ok: true, data: { started: true } });
        return rest;
      }
      case "answer":
        return answerDialog(ctx, actionId, args, finish, refuse, rest);
      case "command":
        return runCommand(args, finish, refuse, rest);
      default:
        return refuse(`unsupported_op: ${cap(request?.op, 40)}`);
    }
  }

  function answerDialog(ctx: any, actionId: string, args: any, finish: any, refuse: any, rest: any) {
    const id = args.dialog_id;
    if (typeof id !== "string" || !id) return refuse("invalid_args: answer needs dialog_id");
    const head = dialogs[0];
    if (head?.id !== id) {
      if (dialogs.some((open) => open.id === id)) {
        return refuse("dialog_queued: OMP shows an earlier dialog first; answer the head dialog");
      }
      if (closedDialogs.includes(id)) return refuse("answered_by_other: the dialog closed before this answer");
      return refuse(head ? "dialog_changed: another dialog is open now" : "no_dialog: no dialog is open");
    }
    if (answering) return refuse("busy: an earlier answer is still in progress");
    // Approve and deny do not depend on where the cursor is. An ask choice does, and a cancel
    // (Esc) on a touched ask may only close the custom-answer editor a person is typing in.
    if (head.kind === "ask" && head.touched) {
      return refuse("dialog_touched: a person already used this dialog");
    }
    const cancel = head.kind === "ask" && args.cancel === true;
    if (head.kind === "ask" && !cancel && head.truncated) {
      return refuse("dialog_truncated: the dialog has more questions or options than reported");
    }
    const answer = answerCodes(head, args);
    if ("error" in answer) return refuse(answer.error);
    const expires = Date.now() + answerWaitMs;
    let seq = 0;
    const keys = answer.chunks.map((chunk) =>
      chunk
        .map(
          (code) =>
            `\x1b[200~herdr-key:v1:${actionId}:${seq++}:${expires}:${runtimeInstance}\n${code}\x1b[201~`,
        )
        .join(""),
    );
    // While an `ask` dialog is open, OMP sends every key but Esc to a person's unfinished draft.
    if (head.kind === "ask" && !cancel) {
      const draft = ctx?.ui?.getEditorText?.() ?? "";
      if (draft && PASTE_MARKER.test(draft)) {
        return refuse("draft_open: the person's draft holds a collapsed paste; it must be sent or cleared first");
      }
      if (draft) {
        dialogDraft = { id, text: draft };
        ctx.ui.setEditorText("");
      }
    }
    const timer = setTimeout(() => {
      if (answering?.actionId !== actionId) return;
      const sent = answering.next === answering.codes.length;
      endAnswer({
        ok: false,
        error: sent
          ? "answer_unconfirmed: herdr's keys reached the dialog but it did not close; check agent.get"
          : "failed: herdr's answer keys did not arrive; nothing was answered",
      });
    }, answerWaitMs);
    timer.unref?.();
    answering = {
      actionId,
      dialogId: id,
      codes: answer.chunks.flat(),
      next: 0,
      spoiled: false,
      ...(head.kind === "approval" ? { approve: args.approve } : {}),
      finish,
      timer,
    };
    // The first ack hands herdr the keys; the result follows when the dialog closes. If herdr
    // refused or never got them, nothing will arrive.
    void ackAction(actionId, { ok: true, keys }).then((acked) => {
      if (!acked && answering?.actionId === actionId) {
        endAnswer({ ok: false, error: "failed: herdr did not write the answer keys; nothing was answered" });
      }
    });
    return rest;
  }

  // `name` is the only session command that 18.4.4 lets an extension run without driving the
  // editor (session changes need a command handler, and an editor submit can reach a picker).
  function runCommand(args: any, finish: any, refuse: any, rest: any) {
    const name = args.name;
    const sub = args.args && typeof args.args === "object" ? args.args : {};
    if (name !== "name") return refuse(`unsupported_op: command ${cap(name, 40)}`);
    const title = sub.title;
    if (typeof title !== "string" || !title.trim() || title.length > 200 || /[\x00-\x1f\x7f]/.test(title)) {
      return refuse("invalid_args: name needs a one-line title of at most 200 characters");
    }
    Promise.resolve(pi.setSessionName(title.trim())).then(
      () => finish({ ok: true, data: { name: title.trim() } }),
      (error) => finish({ ok: false, error: `failed: ${cap(error?.message ?? error, 200)}` }),
    );
    return rest;
  }

  function registerInstructionListener(ctx: {
    isIdle?: () => boolean;
    ui?: { onTerminalInput?: (handler: (data: string) => unknown) => unknown };
  }) {
    // OMP drops extension input listeners on every session change (/new, /resume, branch,
    // reload), sometimes without a session event, so each activation and each turn start
    // registers a fresh one.
    unsubscribeInstructions?.();
    unsubscribeInstructions = ctx.ui?.onTerminalInput?.((data: string) => {
      const key = takeKey(data);
      if (key) return key;
      const taken = takeAction(ctx, data) ?? takeInstruction(ctx, data);
      if (!taken && !TERMINAL_NOISE.test(data)) {
        // A person's input. It always reaches OMP; while a dialog is open it makes positions
        // unknown, and it stops an answer whose keys have not all arrived.
        if (dialogs[0]) dialogs[0].touched = true;
        if (answering && answering.next < answering.codes.length) spoilAnswer();
      }
      return taken;
    });
    instructionListener = typeof unsubscribeInstructions === "function";
  }

  function activateRootSession(ctx: any, sessionStartSource = "startup"): boolean {
    if (ctx?.hasUI !== true) {
      return false;
    }
    rootSession = true;
    // A new or changed session has no compaction of its own running.
    compacting = false;
    // A new or changed session has no running tool or open dialog of its own.
    runningTools.clear();
    dialogs = [];
    detailCtx = ctx;
    reportDetailSoon = (now) => scheduleDetail(undefined, now);
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
    scheduleDetail(ctx);
  });

  pi.on("tool_approval_requested", (event, ctx) => {
    if (!rootSession && !activateRootSession(ctx)) {
      return;
    }
    const label = event?.reason || `${event?.toolName || "Tool"} approval`;
    activateBlocked(label);
    if (!event?.toolCallId) return;
    const input: any = toolInputs.get(event.toolCallId) ?? {};
    let detail = "";
    try {
      detail = typeof input?.command === "string" ? input.command : JSON.stringify(input) ?? "";
    } catch {}
    openDialog({
      id: event.toolCallId,
      kind: "approval",
      tool: cap(event.toolName || "tool", 120),
      // The input after every extension's revision, as OMP's prompt shows it (see `toolInputs`).
      summary: cap(`${event.toolName ?? "tool"} ${detail}`.trim(), 2000),
      questions: [],
      truncated: false,
      touched: false,
    });
  });

  pi.on("tool_approval_resolved", (event, ctx) => {
    if (!rootSession && !activateRootSession(ctx)) {
      return;
    }
    deactivateBlocked();
    closeDialog(event?.toolCallId, event?.approved);
  });

  pi.on("tool_execution_start", (event, ctx) => {
    if (event?.toolName !== "ask") {
      if (rootSession && event?.toolCallId) {
        toolInputs.set(event.toolCallId, event.args);
        runningTools.set(event.toolCallId, {
          name: cap(event.toolName || "tool", 120),
          call_id: event.toolCallId,
          started_ms: Date.now(),
        });
        scheduleDetail(ctx);
      }
      return;
    }
    if (!rootSession && !activateRootSession(ctx)) {
      return;
    }
    activateBlocked(askBlockedMessage(event.args));
    const questions = Array.isArray(event.args?.questions) ? event.args.questions : [];
    detailCtx = ctx ?? detailCtx;
    const options = (question: any) => (Array.isArray(question?.options) ? question.options : []);
    openDialog({
      id: event.toolCallId,
      kind: "ask",
      tool: "ask",
      questions: questions.map((question: any) => ({
        text: cap(question?.question, 2000),
        options: options(question).map((option: any) => cap(option?.label, 200)),
        multi: question?.multi === true,
        recommended: Number.isInteger(question?.recommended) ? question.recommended : 0,
      })),
      truncated: false,
      touched: false,
    });
  });

  pi.on("tool_execution_end", (event, ctx) => {
    if (event?.toolName !== "ask") {
      if (rootSession) {
        runningTools.delete(event?.toolCallId);
        toolInputs.delete(event?.toolCallId);
        scheduleDetail(ctx);
      }
      return;
    }
    if (!rootSession && !activateRootSession(ctx)) {
      return;
    }
    deactivateBlocked();
    detailCtx = ctx ?? detailCtx;
    closeDialog(event?.toolCallId);
  });

  pi.on("agent_end", (event, ctx) => {
    if (!rootSession) {
      return;
    }
    scheduleDetail(ctx);
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

  // Detail for `AgentInfo.omp` from events the state above does not need.
  pi.on("tool_call", (event) => {
    if (!rootSession || !event?.toolCallId) return;
    toolInputs.set(event.toolCallId, event.input);
    for (const key of toolInputs.keys()) {
      if (toolInputs.size <= 32) break;
      toolInputs.delete(key);
    }
  });
  for (const event of ["turn_end", "tool_result"]) {
    pi.on(event, (_event, ctx) => {
      if (rootSession) scheduleDetail(ctx);
    });
  }

  pi.on("session_shutdown", async () => {
    if (!rootSession) {
      return;
    }
    clearPendingTimers();
    clearTimeout(detailTimer);
    detailTimer = undefined;
    if (answering) clearTimeout(answering.timer);
    answering = undefined;
    dialogs = [];
    unsubscribeInstructions?.();
    unsubscribeInstructions = undefined;
    instructionListener = false;
    stopAdmissionWatch();
    compacting = false;
    // Tell herdr the listener is gone before OMP goes on (it awaits this handler): `/restart`
    // execs a new image, which reads the terminal before it registers its own listener. herdr
    // then refuses instructions until the next registration instead of writing a block nobody
    // can confirm; the same runtime id keeps in-flight deliveries followed. One attempt outside
    // the report queue, well under OMP's 2 s cap for shutdown handlers, so a slow herdr never
    // stalls the exit. Sending it ahead of queued state reports is safe: each carries the seq
    // minted when it is built. No retry: a later report is the next image's to send.
    clearTimeout(sessionRetryTimer);
    sessionRetryTimer = undefined;
    const sessionRef = currentSessionRef();
    if (sessionRef) {
      await sendRequestAttempt(sessionReport("startup", sessionRef), shutdownReportTimeoutMs);
    }
  });
}
