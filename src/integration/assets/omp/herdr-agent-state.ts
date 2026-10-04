// installed by herdr
// managed by herdr; reinstalling or updating the integration overwrites this file.
// add custom hooks/plugins beside this file instead of editing it.
// HERDR_INTEGRATION_ID=omp
// HERDR_INTEGRATION_VERSION=15
// @ts-nocheck

import { createHash } from "node:crypto";
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

// What became of a request: `ok`, `unreachable` (no answer: herdr is down, busy or gone), or the
// error code herdr answered with.
type Reply = string;

let requestQueue: Promise<Reply> = Promise.resolve("ok");

function sendRequestAttempt(request: any, timeoutMs: number): Promise<Reply> {
  if (!enabled()) {
    return Promise.resolve("ok");
  }

  return new Promise((resolve) => {
    let done = false;
    let response = "";
    let timeout: ReturnType<typeof setTimeout> | undefined;
    const finish = (reply: Reply) => {
      if (done) return;
      done = true;
      if (timeout) clearTimeout(timeout);
      socket.destroy();
      resolve(reply);
    };

    const socket = net.createConnection(socketEndpoint!);
    socket.setEncoding("utf8");
    socket.on("error", () => finish("unreachable"));
    socket.on("connect", () => socket.write(`${JSON.stringify(request)}\n`));
    socket.on("data", (chunk) => {
      response += chunk;
      const newline = response.indexOf("\n");
      if (newline < 0) return;
      try {
        const parsed = JSON.parse(response.slice(0, newline));
        if (parsed?.id === request.id && parsed?.result?.type === "ok") finish("ok");
        else finish(typeof parsed?.error?.code === "string" ? parsed.error.code : "invalid_response");
      } catch {
        finish("invalid_response");
      }
    });
    socket.on("end", () => finish("unreachable"));
    timeout = setTimeout(() => finish("unreachable"), timeoutMs);
    timeout.unref?.();
  });
}

async function sendRequestNow(buildRequest: () => unknown): Promise<Reply> {
  if ((await sendRequestAttempt(buildRequest(), 500)) === "ok") return "ok";
  // A retry of the same wire object turns a lost-then-applied first attempt
  // into a stale duplicate, so each attempt builds a fresh request with a new
  // id and seq.
  return sendRequestAttempt(buildRequest(), 1500);
}

function sendRequest(buildRequest: () => unknown): Promise<Reply> {
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
// The token herdr puts in instruction and action blocks: only herdr learns it, over the
// peer-checked socket. It changes on every session registration, and at once when a block is found
// in OMP's editor (where a person, the session file and the model could read it), so a leaked token
// stops working. After a normal change the previous token keeps working until herdr confirms the
// new one and 10 s more, for a block herdr wrote before it learned the new one. Every retired
// token is remembered for the life of the runtime (36 bytes per session change), so whichever one
// herdr applied, even one whose reply was lost or came after a later rotation, is recognized: a
// block that carries one (or the runtime id, the token of a herdr server from before block tokens)
// is this runtime's but stale, so it is consumed and refused instead of reaching the editor.
type BlockTokens = {
  current: string;
  previous?: string;
  previousUntil?: number;
  retired: string[];
};
const tokenSlot = globalThis as { [key: symbol]: BlockTokens | undefined };
const TOKEN_KEY = Symbol.for("herdr.omp.blockToken");
const blockTokens = (tokenSlot[TOKEN_KEY] ??= { current: crypto.randomUUID(), retired: [] });
blockTokens.retired ??= [];
const tokenGraceMs = 10_000;

function rotateBlockToken(keepPrevious: boolean): void {
  blockTokens.retired.push(blockTokens.current);
  blockTokens.previous = keepPrevious ? blockTokens.current : undefined;
  // Valid until herdr confirms the new token (`confirmBlockToken`).
  blockTokens.previousUntil = keepPrevious ? Number.POSITIVE_INFINITY : undefined;
  blockTokens.current = crypto.randomUUID();
}

// herdr applied a registration that named `token`.
function confirmBlockToken(token: string): void {
  if (token === blockTokens.current && blockTokens.previousUntil === Number.POSITIVE_INFINITY) {
    blockTokens.previousUntil = Date.now() + tokenGraceMs;
  }
}

function isBlockToken(token: string): boolean {
  return (
    token === blockTokens.current ||
    (token === blockTokens.previous && Date.now() < (blockTokens.previousUntil ?? 0))
  );
}

function isStaleBlockToken(token: string): boolean {
  return (
    !isBlockToken(token) &&
    (token === runtimeInstance || blockTokens.retired.includes(token))
  );
}

// Action and v4 instruction bodies end with this line, so a block that reached OMP's editor can
// be cut out exactly, whatever OMP's paste handling did to the text (tabs, NFC).
function blockEnd(id: string): string {
  return `\nherdr-end:${id}`;
}
// A herdr `agent.instruct` delivery: one bracketed paste with no Enter. The header carries the
// instruction id, the unix ms after which the block must be discarded, the text's UTF-8 byte
// length, and this runtime's token, which only herdr learns (over the peer-checked socket): a
// person's paste that imitates a block lacks it and reaches the editor as a paste. OMP hands a
// paste to input listeners as one string, plus an Enter typed in the same read.
// v4 adds the tag of the session herdr checked (see `sessionTag`).
const INSTRUCTION =
  /^\x1b\[200~herdr-instruction:(?:v3|v4):([0-9a-f]{32}):(\d+):(\d+):([0-9A-Za-z-]{1,64})(?::([0-9a-f]{32}))?\n([\s\S]*)\x1b\[201~(\r\n|\r|\n)?$/;
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
  text: string;
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
  })).then((reply) => {
    // herdr withdraws a listener whose delivery it never heard back about.
    if (reply !== "ok") reportLost();
  });
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
): Promise<Reply> {
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
  // A lone surrogate (already in the input) makes herdr refuse the whole report.
  const value = (typeof text === "string" ? text : String(text ?? "")).toWellFormed();
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
// bytes. The header carries the action id, the key's place in the answer, an expiry and a nonce the
// integration minted for this answer and gave herdr alone, in the keyed ack. No key a person types
// is taken for herdr's, and a block forged with a leaked block token cannot answer a dialog. The body is one key code,
// or `T` and a custom answer text.
const KEY_BLOCK =
  /^\x1b\[200~herdr-key:v1:([0-9a-f]{32}):(\d+):(\d+):([0-9a-f]{32})\n([\s\S]*)\x1b\[201~(\r\n|\r|\n)?$/;
const KEY_BYTES: Record<string, string> = { U: "\x1b[A", D: "\x1b[B", E: "\r", S: " ", X: "\x1b" };
// The detail herdr keeps is at most 32 KiB; a report shrinks to stay under this (see `buildDetail`).
const detailBudgetBytes = 30 * 1024;
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
// How long a dropped instruction's text is looked for in the editor.
const droppedWatchMs = 10_000;
// How long a withdrawal before a session change waits for the change before it is undone.
// Longer than OMP's 30 s cap for one extension handler, so a capped handler of another
// extension (OMP then goes on with the change) is seen as a change still applying.
const switchWaitMs = parseDurationEnv("HERDR_OMP_SWITCH_WAIT_MS", 40_000);
// Interval knobs are at least 10 ms: 0 would spin for the life of the pane.
const switchPollMs = Math.max(parseDurationEnv("HERDR_OMP_SWITCH_POLL_MS", 1000), 10);
// How often the integration looks for a moved session file (`/move`, `/wt`).
const moveWatchMs = Math.max(parseDurationEnv("HERDR_OMP_MOVE_WATCH_MS", 3000), 10);
// How long OMP may take to report that a dialog closed after a person's key.
const closeReportWaitMs = 300;
// The service tiers OMP 18.4.4 accepts per provider family (`ExtensionServiceTier`).
const SERVICE_TIERS: Record<string, string[]> = {
  openai: ["auto", "default", "flex", "scale", "priority", "ultrafast"],
  anthropic: ["priority"],
  google: ["flex", "priority"],
};
// The one status-line key the `status` op uses, so it never touches another extension's status.
const STATUS_KEY = "herdr-sahur";
const SUBAGENT_ID = /^[A-Za-z0-9][A-Za-z0-9._-]{0,79}$/;
const SUBAGENT_STATUSES = new Set(["running", "idle", "parked", "aborted"]);
// Roster sizes, largest first; the detail uses the first that fits.
const ROSTER_TIERS = [30, 10, 0];
const toolListMax = 200;
// How many of herdr's own steers into one subagent run are remembered until they arrive.
const steerMemory = 64;
// How long a subagent cancel waits for OMP's release before it answers.
const subagentCancelWaitMs = 5000;
// What the `status` op set. OMP keeps statuses in memory, as this module keeps this: an OMP
// restart or an extension reload clears both.
let statusText: string | undefined;
// What this module adds to OMP's agent registry, keyed by registry id. Subagent bindings fill the
// agent type, the running tool and `person` (a message that neither the task executor nor herdr
// sent reached the current run: a person's chat from OMP's agent view, or another client's).
// The root binding counts runs: OMP's registry flips a subagent between running and idle at every
// turn of one task run, and a new run starts only once the previous one is over: OMP's executor
// finished it (with a result or not; it then hands the subagent to the lifecycle manager), it
// handed over its result, or it was parked. A `task` resume, a wake and a person's chat with the
// finished subagent each start a new run. Inside a woken run every turn counts as a new run: that
// over-count only makes a caller read again. A subagent that a run with `person` set spawns
// starts with `person` set too: it does that person's work.
type SubagentInfo = {
  type?: string;
  tool?: { name: string; call_id?: string; started_ms: number };
  run: number;
  status?: string;
  accepted: boolean;
  person: boolean;
  // Texts herdr steered that have not arrived yet, so their user messages are not taken for a
  // person's. A steer queued in a run's last step can arrive in the next run.
  steers: string[];
};
const subagentInfo = new Map<string, SubagentInfo>();

function subagentEntry(id: string): SubagentInfo {
  let info = subagentInfo.get(id);
  if (!info) {
    info = { run: 1, accepted: false, person: false, steers: [] };
    subagentInfo.set(id, info);
  }
  return info;
}

// Follows a subagent's registry ref from one observation to the next; returns its entry.
function observeRun(ref: any): SubagentInfo {
  const info = subagentEntry(ref.id);
  if (info.status === undefined) {
    info.accepted = ref.lifecycle?.acceptedAt !== undefined;
    if (typeof ref.parentId === "string" && subagentInfo.get(ref.parentId)?.person) info.person = true;
  } else if (
    ref.status === "running" &&
    info.status !== "running" &&
    (info.accepted || info.status === "parked" || runFinished(ref))
  ) {
    info.run += 1;
    info.accepted = false;
    info.person = false;
  }
  if (ref.lifecycle?.acceptedAt !== undefined) info.accepted = true;
  info.status = ref.status;
  return info;
}
// Set by the root binding: a subagent's binding saw a change.
let onSubagentChange: (() => void) | undefined;
// OMP's agent lifecycle manager, loaded once; undefined when it does not load as OMP's own.
let lifecycleManager: Promise<any> | undefined;
// The same manager once loaded, for the synchronous registry listener.
let lifecycleLoaded: any;

// OMP's executor finished the subagent's run: it adopts every finished or failed subagent it keeps
// alive (`finalizeSubagentLifecycle`), and only then.
function runFinished(ref: any): boolean {
  try {
    return lifecycleLoaded?.global?.().has?.(ref.id, ref) === true;
  } catch {
    return false;
  }
}

function loadLifecycleManager(registryClass: unknown): Promise<any> {
  lifecycleManager ??= (async () => {
    const [lifecycle, registry] = await Promise.all([
      import("@oh-my-pi/pi-coding-agent/registry/agent-lifecycle"),
      import("@oh-my-pi/pi-coding-agent/registry/agent-registry"),
    ]);
    // OMP maps `@oh-my-pi/*` imports to its own bundled modules; a copy would hold another
    // registry, and a release there would cancel nothing.
    if (registryClass === undefined || registry?.AgentRegistry !== registryClass) return undefined;
    const manager = lifecycle?.AgentLifecycleManager;
    if (typeof manager?.global !== "function") return undefined;
    lifecycleLoaded = manager;
    return manager;
  })().catch(() => undefined);
  return lifecycleManager;
}

// C0 and C1 controls, line and paragraph separators, and format characters: OMP's renderer
// strips some of these and keeps others, so the text a person sees could differ from the text
// herdr reports, or reorder in a terminal behind herdr.
// Format characters (every bidi control among them) are invisible; the joiners and tag characters
// that emoji sequences need stay allowed.
const UNSAFE_TEXT = /[\x00-\x1f\x7f-\x9f\u2028\u2029]|(?![\u200c\u200d\u{e0020}-\u{e007f}])\p{Cf}/u;

function isOneLine(text: unknown, max: number): text is string {
  return typeof text === "string" && text.trim().length > 0 && text.length <= max && !UNSAFE_TEXT.test(text);
}

// The text of a user message's content.
function messageText(content: unknown): string | undefined {
  return typeof content === "string"
    ? content
    : Array.isArray(content)
      ? content
          .filter((part) => part?.type === "text")
          .map((part) => part.text)
          .join("\n")
      : undefined;
}

function toolNames(names: unknown): string[] {
  if (!Array.isArray(names)) return [];
  return names.filter((name): name is string => typeof name === "string" && name.length > 0).slice(0, toolListMax).map((name) => cap(name, 128));
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
  const previousId = currentAgentSessionId;
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
  if (currentAgentSessionId !== previousId) movedSessionTags = [];
}

// The session OMP runs now, read from `ctx` without storing it.
// The session OMP runs now by id (a move keeps it, a switch changes it), else by path.
function readSessionIdKey(ctx: any): string | undefined {
  try {
    const id = ctx?.sessionManager?.getSessionId?.();
    if (typeof id === "string" && id.length > 0) return `id\0${id}`;
  } catch {
    return undefined;
  }
  return readSessionKey(ctx);
}

function readSessionKey(ctx: any): string | undefined {
  let file: unknown;
  let id: unknown;
  try {
    file = ctx?.sessionManager?.getSessionFile?.();
    id = ctx?.sessionManager?.getSessionId?.();
  } catch {
    return undefined;
  }
  if (isAbsoluteSessionPath(file)) return `path\0${file}`;
  if (typeof id === "string" && id.length > 0) return `id\0${id}`;
  return undefined;
}

// OMP already runs another session than the one this runtime registered: it is between a
// session change's swap and its `session_switch` event.
// A session switch changes the session id (a new session, fork, branch or resume each have their
// own). `/move` and `/wt` keep the id and only rename the session file, so they are not a switch.
function liveSessionMoved(ctx: any): boolean {
  let id: unknown;
  try {
    id = ctx?.sessionManager?.getSessionId?.();
  } catch {
    return false;
  }
  return (
    typeof id === "string" &&
    id.length > 0 &&
    currentAgentSessionId !== undefined &&
    id !== currentAgentSessionId
  );
}

// Tags of earlier files of the current session (`/move`, `/wt`): a block herdr checked against
// one of them is for this conversation.
let movedSessionTags: string[] = [];

function tagOf(value: string): string {
  return createHash("sha256").update(value).digest("hex").slice(0, 32);
}

function isCurrentSessionTag(tag: string): boolean {
  return tag === sessionTag() || movedSessionTags.includes(tag);
}

// OMP moved the session file (same id, new path), which emits no event: follow it and tell herdr,
// so herdr shows, checks and resumes the new path. Returns whether it moved.
function followSessionMove(ctx: any): boolean {
  const before = currentAgentSessionPath;
  if (before === undefined || liveSessionMoved(ctx)) return false;
  let file: unknown;
  try {
    file = ctx?.sessionManager?.getSessionFile?.();
  } catch {
    return false;
  }
  if (!isAbsoluteSessionPath(file) || file === before) return false;
  movedSessionTags.push(tagOf(before));
  movedSessionTags.splice(0, movedSessionTags.length - 8);
  updateSessionRef(ctx);
  // Nothing reports a move again (only the next turn would), so keep trying while herdr is
  // unreachable, as for a lost report: a herdr restart must resume the moved file.
  reportsLost = true;
  void reportSession();
  return true;
}

function currentSessionKey(): string | undefined {
  if (currentAgentSessionPath) return `path\0${currentAgentSessionPath}`;
  if (currentAgentSessionId) return `id\0${currentAgentSessionId}`;
  return undefined;
}

// The current session as herdr tags it in a block: the first 16 bytes of the SHA-256 of the
// session path, else its id, in hex.
function sessionTag(): string | undefined {
  const value = currentAgentSessionPath ?? currentAgentSessionId;
  return value === undefined ? undefined : tagOf(value);
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
      block_token: blockTokens.current,
      ...sessionRef,
    },
  };
}

// Set by the root session's binding, called after herdr registered the session: sends the
// detail again (herdr drops the detail of a listener it replaces), and after a lost report the
// state too, because herdr keeps whatever it last received.
let onRegistered: ((lost: boolean) => void) | undefined;
// A state, detail or ack report that herdr never got. Each report gets two attempts, and a state
// equal to the last one is not sent again, so a report lost in a socket outage would stay lost
// until the next change. Recovery re-registers the session (with its own retries) and then sends
// state and detail again. Recoveries are spaced: the gap starts at the session retry base and
// doubles, up to a minute, while reports keep getting lost; it starts over after a minute without
// a loss. A report herdr answered with an error is refused, not lost: registering again cannot
// change that answer, so it never starts a recovery.
let reportsLost = false;
let recoveryTimer: ReturnType<typeof setTimeout> | undefined;
let recoveryGapMs = 0;
let lastLossAt = 0;
const recoveryGapMaxMs = 60_000;
const recoveryQuietMs = 60_000;

function reportLost(): void {
  const now = Date.now();
  if (now - lastLossAt > recoveryQuietMs) recoveryGapMs = sessionRetryBaseMs;
  lastLossAt = now;
  if (reportsLost) return;
  reportsLost = true;
  recoveryTimer = setTimeout(() => {
    recoveryTimer = undefined;
    void reportSession();
  }, recoveryGapMs);
  recoveryTimer.unref?.();
  recoveryGapMs = Math.min(Math.max(recoveryGapMs, 1) * 2, recoveryGapMaxMs);
}

async function reportSession(sessionStartSource = "startup", attempt = 0): Promise<void> {
  const sessionRef = currentSessionRef();
  const sessionKey = currentSessionKey();
  registeredSessionKey = undefined;
  clearTimeout(sessionRetryTimer);
  sessionRetryTimer = undefined;
  if (!sessionRef || !sessionKey) {
    reportsLost = false;
    return;
  }
  clearTimeout(recoveryTimer);
  recoveryTimer = undefined;

  // Each retry attempt mints a fresh id and sequence number. If the server
  // applied the first attempt but its reply was lost, an identical retry
  // would be rejected as stale and leave the session unregistered with no
  // later session event to recover it. A fresh retry replays the same
  // profile, PID, and session through every server check and converges.
  let sentToken: string | undefined;
  let sentListener = false;
  const reply = await sendRequest(() => {
    sentToken = blockTokens.current;
    sentListener = instructionListener;
    return sessionReport(sessionStartSource, sessionRef);
  });
  const delivered = reply === "ok";
  if (delivered && sentListener && sentToken) confirmBlockToken(sentToken);
  if (delivered && currentSessionKey() === sessionKey) {
    registeredSessionKey = sessionKey;
    const lost = reportsLost;
    reportsLost = false;
    onRegistered?.(lost);
  } else if (
    !delivered &&
    currentSessionKey() === sessionKey &&
    // After the retries, keep trying slowly while a report is lost and herdr does not answer, so
    // state and detail are resent whenever herdr comes back.
    (attempt < sessionRetryLimit || (reportsLost && reply === "unreachable"))
  ) {
    sessionRetryTimer = setTimeout(() => {
      sessionRetryTimer = undefined;
      if (currentSessionKey() === sessionKey && registeredSessionKey !== sessionKey) {
        void reportSession(sessionStartSource, attempt + 1);
      }
    }, Math.min(sessionRetryBaseMs * 2 ** attempt, 30_000));
    sessionRetryTimer.unref?.();
  } else if (!delivered && currentSessionKey() === sessionKey) {
    // Out of retries: a later lost report may try again. If the session changed, a newer report
    // owns the flag (a move sets it before its report).
    reportsLost = false;
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
  const reply = await sendRequest(() => {
    params ??= withRegisteredSessionRef({ pane_id: paneId, source, agent: "omp", state, message });
    return {
      id: `${source}:${Date.now()}:${Math.random().toString(36).slice(2)}`,
      method: "pane.report_agent",
      params: { ...params, seq: nextReportSeq() },
    };
  });
  if (reply === "unreachable") reportLost();
}

let sendInFlight = false;
let queuedState: QueuedState | undefined;
// Settles once every state queued so far was sent.
let stateSent: Promise<void> = Promise.resolve();

function queueState(state: AgentState, message?: string): void {
  queuedState = { state, message };
  if (!sendInFlight) {
    stateSent = drainStateQueue();
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
      stateSent = drainStateQueue();
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
    const [, instructionId, expiresMs, byteLength, token, session, framed, enter] = match;
    const stale = isStaleBlockToken(token);
    if ((!stale && !isBlockToken(token)) || Number(expiresMs) > Date.now() + instructionExpiryCapMs) {
      return undefined;
    }
    const rest = enter ? { data: enter } : { consume: true };
    // A block for a token this runtime retired, or a herdr from before block tokens: nothing runs.
    if (stale) {
      ackInstruction(instructionId, "dropped");
      // herdr holds an older registration than OMP: register again (herdr just wrote, so it
      // answers).
      void reportSession();
      return rest;
    }
    // v4 bodies end with the end line; v3 bodies have none.
    const text =
      session !== undefined && framed.endsWith(blockEnd(instructionId))
        ? framed.slice(0, -blockEnd(instructionId).length)
        : session !== undefined
          ? undefined
          : framed;
    if (text === undefined) {
      void reportSession();
      return rest;
    }
    // OMP is mid-switch: it runs another session (a new id) and the activation has not come yet.
    // Nothing is sent; the activation registers the new session.
    if (liveSessionMoved(ctx)) {
      ackInstruction(instructionId, "dropped");
      return rest;
    }
    followSessionMove(ctx);
    // herdr checked the caller's session against the last one OMP reported; OMP has moved on
    // (`/new`, `/resume`, a branch) before that report reached herdr. Nothing is sent.
    if (session !== undefined && !isCurrentSessionTag(session)) {
      ackInstruction(instructionId, "dropped");
      // herdr holds an older registration than OMP: register again (herdr just wrote, so it
      // answers).
      void reportSession();
      return rest;
    }
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
      text,
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
  // Texts OMP took but dropped without a turn. A person's session change while OMP prepared the
  // turn makes OMP put the text back into the (new, empty) editor, where the person's next Enter
  // would send it. The integration takes it out again: only when the editor holds exactly that
  // text, or that text, a newline and what the person typed after it (how OMP puts a cancelled
  // submission back), keeping the person's part. Checked at the drop, a few times after it, and
  // at the next activation.
  let droppedTexts: { text: string; until: number }[] = [];

  function clearDroppedText(ui: any) {
    const now = Date.now();
    droppedTexts = droppedTexts.filter((entry) => entry.until > now);
    const editor = ui?.getEditorText?.();
    if (typeof editor !== "string" || !editor) return;
    for (const [index, entry] of droppedTexts.entries()) {
      // OMP's editor stores the text with each tab as 3 spaces, CRs as newlines and other control
      // characters removed (`sanitizeLoadedText`); herdr refuses control characters but tabs and
      // newlines.
      const shown = entry.text.replaceAll("\t", "   ");
      let rest: string | undefined;
      for (const form of shown === entry.text ? [shown] : [shown, entry.text]) {
        if (editor === form) rest = "";
        else if (editor.startsWith(`${form}\n`)) rest = editor.slice(form.length + 1);
        if (rest !== undefined) break;
      }
      if (rest === undefined) continue;
      ui.setEditorText?.(rest);
      droppedTexts.splice(index, 1);
      return;
    }
  }

  function rememberDropped(ui: any, text: string) {
    droppedTexts.push({ text, until: Date.now() + droppedWatchMs });
    droppedTexts.splice(0, droppedTexts.length - 8);
    clearDroppedText(ui);
    for (const delay of [300, 1_000, 3_000]) {
      const check = setTimeout(() => clearDroppedText(ui ?? detailCtx?.ui), delay);
      check.unref?.();
    }
  }

  function watchAdmissions(ctx: {
    isIdle?: () => boolean;
    hasPendingMessages?: () => boolean;
    ui?: unknown;
  }) {
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
          rememberDropped(ctx?.ui, entry.text);
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
  // herdr refused a detail report; set until the next session event.
  let detailRefused = false;
  let moveWatch: ReturnType<typeof setInterval> | undefined;
  let unsubscribeRegistry: (() => void) | undefined;
  // Counts activations and shutdowns, so a pending withdrawal undo sees that a session event came.
  let activations = 0;
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
        nonce: string;
        dialogId: string;
        codes: string[];
        next: number;
        spoiled: boolean;
        approve?: boolean;
        finish: (result: { ok: boolean; error?: string; data?: Record<string, unknown> }) => void;
        timer: ReturnType<typeof setTimeout>;
      }
    | undefined;
  // The nonces of recent answers, so their late key blocks are dropped.
  const answerNonces: string[] = [];
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

  // OMP's process-wide agent registry (`pi.pi` is OMP's own package module); undefined when this
  // OMP has none.
  function agentRegistry(): any {
    try {
      const registry = pi.pi?.AgentRegistry?.global?.();
      return typeof registry?.list === "function" && typeof registry?.get === "function" ? registry : undefined;
    } catch {
      return undefined;
    }
  }

  // A subagent of the root session: its transcript sits under the root session file's directory
  // stem (`<session>/<id>.jsonl`, nested ones deeper). This leaves out advisors, agents of an
  // earlier session of this process and agents without a file.
  function isSessionSubagent(ref: any, stem: string | undefined): boolean {
    const file = ref?.sessionFile;
    return (
      stem !== undefined &&
      ref?.kind === "sub" &&
      typeof ref.id === "string" &&
      typeof ref.status === "string" &&
      typeof file === "string" &&
      file.length > stem.length + 1 &&
      file.startsWith(stem) &&
      (file[stem.length] === "/" || file[stem.length] === "\\")
    );
  }

  function sessionStem(): string | undefined {
    let file: unknown;
    try {
      file = detailCtx?.sessionManager?.getSessionFile?.();
    } catch {
      return undefined;
    }
    return isAbsoluteSessionPath(file) && file.endsWith(".jsonl") ? file.slice(0, -".jsonl".length) : undefined;
  }

  function sessionSubagent(registry: any, id: string): any {
    let ref: any;
    try {
      ref = registry.get(id);
    } catch {
      return undefined;
    }
    return isSessionSubagent(ref, sessionStem()) ? ref : undefined;
  }

  // The root session's subagents, running first (newest first), then the rest by last activity.
  function subagentRoster(): { total: number; running: number; items: Record<string, unknown>[] } | undefined {
    const registry = agentRegistry();
    if (!registry) return undefined;
    let refs: any[];
    try {
      refs = registry.list();
    } catch {
      return undefined;
    }
    for (const id of subagentInfo.keys()) {
      if (!refs.some((ref) => ref?.id === id)) subagentInfo.delete(id);
    }
    const stem = sessionStem();
    const subs = refs.filter((ref) => isSessionSubagent(ref, stem));
    for (const ref of subs) observeRun(ref);
    if (subs.length === 0) return undefined;
    const ms = (value: unknown) => (Number.isFinite(value) ? Math.max(0, Math.round(value as number)) : 0);
    subs.sort((a, b) =>
      (a.status === "running") !== (b.status === "running")
        ? a.status === "running"
          ? -1
          : 1
        : a.status === "running"
          ? ms(b.createdAt) - ms(a.createdAt)
          : ms(b.lastActivity) - ms(a.lastActivity),
    );
    const items = subs.slice(0, ROSTER_TIERS[0]).map((ref) => {
      const info = subagentInfo.get(ref.id);
      const model = ref.session?.model;
      return {
        id: ref.id,
        name: cap(typeof ref.displayName === "string" && ref.displayName ? ref.displayName : ref.id, 80),
        ...(info?.type ? { type: cap(info.type, 80) } : {}),
        ...(typeof ref.parentId === "string" && ref.parentId !== "Main" ? { parent: cap(ref.parentId, 80) } : {}),
        status: SUBAGENT_STATUSES.has(ref.status) ? ref.status : cap(ref.status, 20),
        run: info?.run ?? 1,
        ...((info?.run ?? 1) > 1 ? { revived: true } : {}),
        ...(info?.person ? { person: true } : {}),
        ...(typeof ref.activity === "string" && ref.activity ? { activity: cap(ref.activity, 160) } : {}),
        ...(info?.tool ? { tool: { name: info.tool.name, started_ms: info.tool.started_ms } } : {}),
        ...(model?.provider && model?.id ? { model: cap(`${model.provider}/${model.id}`, 200) } : {}),
        started_ms: ms(ref.createdAt),
        active_ms: ms(ref.lastActivity),
      };
    });
    return { total: subs.length, running: subs.filter((ref) => ref.status === "running").length, items };
  }

  function serviceTiers(): Record<string, string> {
    const tiers: Record<string, string> = {};
    try {
      const current = pi.getServiceTiers?.();
      for (const family of Object.keys(SERVICE_TIERS)) {
        if (typeof current?.[family] === "string") tiers[family] = current[family];
      }
    } catch {}
    return tiers;
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
    const tiers = serviceTiers();
    if (Object.keys(tiers).length > 0) detail.service_tiers = tiers;
    if (statusText !== undefined) detail.status_text = statusText;
    const fits = () => Buffer.byteLength(JSON.stringify(detail), "utf8") <= detailBudgetBytes;
    const head = dialogs[0];
    if (head) {
      // The smallest report that fits herdr's limit; an answer refuses what it had to cut.
      for (const tier of DIALOG_TIERS) {
        const { truncated, report } = dialogReport(head, tier);
        detail.dialog = report;
        head.truncated = truncated;
        if (fits()) break;
      }
    }
    // The dialog has its room; the roster and the tool lists take what is left, in that order of
    // priority: fewer roster rows first, then no inactive tools, then no active tools.
    let active: string[] = [];
    let inactive: string[] = [];
    try {
      active = toolNames(pi.getActiveTools?.());
      const on = new Set(pi.getActiveTools?.());
      inactive = toolNames((pi.getAllTools?.() ?? []).map((tool: any) => tool?.name).filter((name: unknown) => !on.has(name)));
    } catch {}
    const roster = subagentRoster();
    const candidates: [number, boolean, boolean][] = [
      ...ROSTER_TIERS.map((rows): [number, boolean, boolean] => [rows, true, true]),
      [0, true, false],
      [0, false, false],
    ];
    for (const [rows, withActive, withInactive] of candidates) {
      delete detail.subagents;
      delete detail.active_tools;
      delete detail.inactive_tools;
      if (roster) {
        detail.subagents = {
          total: roster.total,
          running: roster.running,
          items: roster.items.slice(0, rows),
          ...(roster.total > Math.min(rows, roster.items.length) ? { truncated: true } : {}),
        };
      }
      if (withActive && active.length > 0) detail.active_tools = active;
      if (withInactive && inactive.length > 0) detail.inactive_tools = inactive;
      if (fits()) break;
    }
    return detail;
  }

  function sendDetail() {
    clearTimeout(detailTimer);
    detailTimer = undefined;
    if (!rootSession || !instructionListener || !detailCtx || detailRefused) return;
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
    })).then((reply) => {
      // herdr withdrew the listener: register again. Any other error is herdr refusing the
      // report (a server without `pane.report_omp_detail`, or detail it rejects); sending it again
      // cannot help, so detail stops until the next session event.
      if (reply === "unreachable" || reply === "process_mismatch") reportLost();
      else if (reply !== "ok") detailRefused = true;
    });
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
    const [, actionId, seq, expiresMs, nonce, code, enter] = match;
    const current = answering?.actionId === actionId && answering.nonce === nonce ? answering : undefined;
    // A block of an answer that ended is dropped; one with a nonce this runtime never minted is a
    // person's paste (or a forgery) and passes as such.
    if (!current && !answerNonces.includes(nonce)) return undefined;
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
    const [, actionId, expiresMs, byteLength, token, framed, enter] = match;
    const stale = isStaleBlockToken(token);
    if ((!stale && !isBlockToken(token)) || Number(expiresMs) > Date.now() + instructionExpiryCapMs) {
      return undefined;
    }
    const rest = enter ? { data: enter } : { consume: true };
    if (stale) {
      void ackAction(actionId, { ok: false, error: "failed: the block carried a retired token; nothing ran; read agent.get again" });
      void reportSession();
      return rest;
    }
    const body = framed.endsWith(blockEnd(actionId)) ? framed.slice(0, -blockEnd(actionId).length) : undefined;
    if (
      body === undefined ||
      Date.now() > Number(expiresMs) ||
      Buffer.byteLength(body, "utf8") !== Number(byteLength)
    ) {
      void reportSession();
      return rest;
    }
    // The result follows the state and detail reports that show the action's effect, so the
    // `agent` herdr answers with is current.
    const finish = async (result: { ok: boolean; error?: string; data?: Record<string, unknown> }) => {
      publishState(true);
      await stateSent;
      scheduleDetail(ctx, true);
      void ackAction(actionId, result, true).then((reply) => {
        // A lost ack withdraws the listener in herdr until the next session report.
        if (reply !== "ok") reportLost();
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
    // herdr checked the caller's session against the last one OMP reported; OMP has moved on
    // (`/new`, `/resume`, a branch) before that report reached herdr.
    if (liveSessionMoved(ctx)) {
      return refuse("session_changed: OMP is switching to another session now; read agent.get again");
    }
    followSessionMove(ctx);
    if (typeof request?.session === "string" && !isCurrentSessionTag(request.session)) {
      void reportSession();
      return refuse("session_changed: OMP runs another session now; read agent.get again");
    }
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
        // The result waits (up to 3 s) for the turn to end, so its status shows the abort.
        const until = Date.now() + 3000;
        const settled = setInterval(() => {
          if (agentActive && Date.now() < until) return;
          clearInterval(settled);
          void finish({ ok: true, data: { was_idle: idle } });
        }, 50);
        settled.unref?.();
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
      case "set_service_tier": {
        const { family, tier } = args;
        if (typeof family !== "string" || !Object.hasOwn(SERVICE_TIERS, family)) {
          return refuse("invalid_args: family must be one of openai, anthropic, google");
        }
        if (tier !== null && typeof tier !== "string") {
          return refuse("invalid_args: tier must be a tier name, or null to clear the session's tier");
        }
        if (tier !== null && !SERVICE_TIERS[family].includes(tier)) {
          return refuse(`invalid_tier: ${family} takes ${SERVICE_TIERS[family].join(", ")}`);
        }
        try {
          pi.setServiceTier(family, tier ?? undefined);
        } catch (error) {
          return refuse(`failed: ${cap(error?.message ?? error, 200)}`);
        }
        finish({ ok: true, data: { service_tiers: serviceTiers() } });
        return rest;
      }
      case "set_tools":
        return setTools(args, finish, refuse, rest);
      case "notify": {
        const level = args.level ?? "info";
        if (level === "error") {
          return refuse("invalid_args: level error is refused: OMP's error notice also clears the person's pending input; use warning");
        }
        if (level !== "info" && level !== "warning") return refuse("invalid_args: level must be info or warning");
        if (!isOneLine(args.text, 500)) return refuse("invalid_args: notify needs a one-line text of 1-500 characters");
        if (typeof ctx?.ui?.notify !== "function") return refuse("unsupported_op: this OMP shows no notices");
        try {
          ctx.ui.notify(args.text, level);
        } catch (error) {
          return refuse(`failed: ${cap(error?.message ?? error, 200)}`);
        }
        finish({ ok: true, data: {} });
        return rest;
      }
      case "status": {
        const text = args.text;
        if (text !== undefined && text !== null && !isOneLine(text, 80)) {
          return refuse("invalid_args: status text must be one line of 1-80 characters, or null to clear it");
        }
        if (typeof ctx?.ui?.setStatus !== "function") return refuse("unsupported_op: this OMP has no status line");
        const next = typeof text === "string" ? text : undefined;
        try {
          ctx.ui.setStatus(STATUS_KEY, next);
        } catch (error) {
          return refuse(`failed: ${cap(error?.message ?? error, 200)}`);
        }
        statusText = next;
        finish({ ok: true, data: next === undefined ? {} : { status_text: next } });
        return rest;
      }
      case "subagent_steer":
      case "subagent_cancel":
        return subagentAction(request.op, args, finish, refuse, rest);
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
    const nonce = crypto.randomUUID().replaceAll("-", "");
    answerNonces.push(nonce);
    answerNonces.splice(0, answerNonces.length - 16);
    let seq = 0;
    const keys = answer.chunks.map((chunk) =>
      chunk
        .map(
          (code) =>
            `\x1b[200~herdr-key:v1:${actionId}:${seq++}:${expires}:${nonce}\n${code}\x1b[201~`,
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
      nonce,
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
    void ackAction(actionId, { ok: true, keys }).then((reply) => {
      if (reply === "unreachable") reportLost();
      if (reply !== "ok" && answering?.actionId === actionId) {
        endAnswer({ ok: false, error: "failed: herdr did not write the answer keys; nothing was answered" });
      }
    });
    return rest;
  }

  // Computes the new set from OMP's active tools at apply time, so a caller's older read never
  // undoes a change made since.
  function setTools(args: any, finish: any, refuse: any, rest: any) {
    const list = (value: unknown) =>
      value === undefined ||
      (Array.isArray(value) &&
        value.length <= 64 &&
        value.every((name) => typeof name === "string" && name.length >= 1 && name.length <= 128));
    if (!list(args.enable) || !list(args.disable)) {
      return refuse("invalid_args: enable and disable are lists of at most 64 tool names of 1-128 characters");
    }
    const enable: string[] = args.enable ?? [];
    const disable: string[] = args.disable ?? [];
    if (enable.length + disable.length === 0) return refuse("invalid_args: set_tools needs enable or disable");
    const both = enable.find((name) => disable.includes(name));
    if (both !== undefined) return refuse(`invalid_args: ${cap(both, 128)} is in both enable and disable`);
    let known: Set<unknown>;
    let current: string[];
    try {
      known = new Set((pi.getAllTools() ?? []).map((tool: any) => tool?.name));
      current = [...pi.getActiveTools()];
    } catch (error) {
      return refuse(`failed: ${cap(error?.message ?? error, 200)}`);
    }
    const unknown = [...new Set([...enable, ...disable])].filter((name) => !known.has(name));
    if (unknown.length > 0) {
      return refuse(`unknown_tool: ${unknown.slice(0, 10).map((name) => cap(name, 128)).join(", ")}`);
    }
    const next = [...current.filter((name) => !disable.includes(name)), ...enable.filter((name) => !current.includes(name))];
    if (next.length === 0) return refuse("invalid_args: no tools would remain");
    Promise.resolve(pi.setActiveTools(next)).then(
      () => finish({ ok: true, data: { active_tools: toolNames(pi.getActiveTools?.()) } }),
      (error) => finish({ ok: false, error: `failed: ${cap(error?.message ?? error, 200)}` }),
    );
    return rest;
  }

  // Steer and cancel a subagent of the root session in OMP's process, as OMP's Agent Hub and its
  // RPC mode do: an aside into the running turn, or a release with a tombstone plus an abort.
  function subagentAction(op: string, args: any, finish: any, refuse: any, rest: any) {
    const id = args.subagent_id;
    if (typeof id !== "string" || !SUBAGENT_ID.test(id)) {
      return refuse("invalid_args: subagent_id must be an id from omp.subagents");
    }
    const expectedRun = args.expected_run;
    if (!Number.isSafeInteger(expectedRun) || expectedRun < 1) {
      return refuse("invalid_args: expected_run must be the run from omp.subagents");
    }
    const text = args.text;
    if (op === "subagent_steer" && (typeof text !== "string" || !text.trim() || text.length > 3000)) {
      return refuse("invalid_args: subagent_steer needs text of 1-3000 characters");
    }
    const registry = agentRegistry();
    if (!registry) return refuse("unsupported_op: this OMP has no agent registry");
    const ref = sessionSubagent(registry, id);
    if (!ref) return refuse(`no_subagent: ${id} is not a subagent of this session`);
    const session = ref.session;
    // Checked in this order: a run that ended reports its end, even when the caller read an older
    // one.
    const stillOwned = () => {
      const info = observeRun(ref);
      if (ref.status !== "running" || !ref.session) return `subagent_not_running: ${id} is ${cap(ref.status, 20)}`;
      if (info.run !== expectedRun) {
        return `subagent_changed: ${id} is on run ${info.run} now, not run ${expectedRun}; read agent.get again`;
      }
      if (info.person) {
        return `subagent_person_chat: someone else (a person in OMP's agent view, or another client) sent ${id}'s run a message; nothing was sent`;
      }
      return undefined;
    };
    const owned = stillOwned();
    if (owned) return refuse(owned);
    if (op === "subagent_steer") {
      // The run's result is with its parent; an aside now would start a turn after it.
      if (ref.lifecycle?.responseAt !== undefined) {
        return refuse(`subagent_not_running: ${id} has handed over its result`);
      }
      // Between turns an aside would start a new turn; the executor's own reminders run there.
      if (session.isStreaming !== true) {
        return refuse(`subagent_busy: ${id} is between turns; retry in a few seconds`);
      }
      const info = subagentEntry(id);
      // Queued asides reach the subagent together at its next step boundary, so every one queued in
      // this run is kept (up to a bound far above what one step boundary sees).
      info.steers = [...info.steers.slice(-(steerMemory - 1)), text];
      Promise.resolve(session.sendUserMessage(text, { deliverAs: "aside" })).then(
        () => finish({ ok: true, data: { subagent_id: id, delivered: "aside" } }),
        (error) => finish({ ok: false, error: `failed: ${cap(error?.message ?? error, 200)}` }),
      );
      return rest;
    }
    void (async () => {
      const manager = await loadLifecycleManager(pi.pi?.AgentRegistry);
      if (!manager) {
        return finish({
          ok: false,
          error: "unsupported_op: OMP's agent lifecycle module did not load as OMP's own; reload the extension or restart OMP",
        });
      }
      // The import awaited: the subagent may have ended or moved on meanwhile.
      if (registry.get(id) !== ref || ref.session !== session) {
        return finish({ ok: false, error: `subagent_not_running: ${id} ended before the cancel` });
      }
      const changed = stillOwned();
      if (changed) return finish({ ok: false, error: changed });
      // Release first: it marks the ref aborted at once, which the task executor turns into the
      // task's abort; a bare abort would only end the subagent's turn.
      let timer: ReturnType<typeof setTimeout> | undefined;
      const result = await Promise.race([
        Promise.allSettled([
          Promise.resolve().then(() => manager.global().release(id, ref, { tombstone: true })),
          Promise.resolve().then(() => session.abort({ reason: "Interrupted by herdr" })),
        ]),
        new Promise<undefined>((resolve) => {
          timer = setTimeout(resolve, subagentCancelWaitMs);
        }),
      ]);
      clearTimeout(timer);
      if (!result) return finish({ ok: true, data: { subagent_id: id, cancelled: true, settled: false } });
      const [released] = result;
      if (released.status === "rejected") {
        return finish({ ok: false, error: `failed: ${cap(released.reason?.message ?? released.reason, 200)}` });
      }
      if (released.value !== true) {
        return finish({ ok: false, error: `subagent_not_running: ${id} ended before the cancel` });
      }
      return finish({ ok: true, data: { subagent_id: id, cancelled: true } });
    })();
    return rest;
  }

  // `name` is the only session command that 18.4.4 lets an extension run without driving the
  // editor (session changes need a command handler, and an editor submit can reach a picker).
  function runCommand(args: any, finish: any, refuse: any, rest: any) {
    const name = args.name;
    const sub = args.args && typeof args.args === "object" ? args.args : {};
    if (name !== "name") return refuse(`unsupported_op: command ${cap(name, 40)}`);
    const title = sub.title;
    if (!isOneLine(title, 200)) {
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

  // Removes herdr blocks for this runtime from the editor text; returns whether it found any.
  function scrubLeakedBlocks(ctx: any): boolean {
    const ui = ctx?.ui;
    const text = ui?.getEditorText?.();
    if (typeof text !== "string" || !text.includes("herdr-")) return false;
    let kept = "";
    let at = 0;
    let found = false;
    const header =
      /herdr-(action:v1|instruction:v[34]|key:v1):([0-9a-f]{32}):(\d+):(\d+):([0-9A-Za-z-]{1,64})(?::[0-9a-f]{32})?\n/g;
    for (let match = header.exec(text); match; match = header.exec(text)) {
      const [whole, kind, id, , third, token] = match;
      const ours =
        kind === "key:v1"
          ? answerNonces.includes(token)
          : isBlockToken(token) || isStaleBlockToken(token);
      if (!ours) continue;
      found = true;
      const bodyStart = match.index + whole.length;
      let end = bodyStart;
      if (kind === "key:v1") {
        // A key's body is one key code (a custom answer has no control characters).
        const newline = text.indexOf("\n", bodyStart);
        end = newline < 0 ? text.length : newline;
      } else {
        const marker = text.indexOf(blockEnd(id), bodyStart);
        if (marker >= 0) {
          // Cut up to the end line: exact, whatever OMP did to the body.
          end = marker + blockEnd(id).length;
        } else {
          // A v3 block (a herdr from before end lines). Its byte length counts the text herdr
          // wrote; OMP's paste handling turns a tab into 3 spaces and can shorten text to NFC. Cut
          // the body only when it is plain ASCII, where counting can only fall short (a tab), so
          // the person's own text is never cut. Otherwise cut only the header with the token.
          const bytes = Number(third);
          let used = 0;
          let candidate = bodyStart;
          for (const point of text.slice(bodyStart)) {
            if (used >= bytes) break;
            used += Buffer.byteLength(point, "utf8");
            candidate += point.length;
          }
          if (/^[\x20-\x7e\n]*$/.test(text.slice(bodyStart, candidate))) end = candidate;
        }
      }
      kept += text.slice(at, match.index);
      at = end;
      header.lastIndex = end;
      if (kind === "action:v1") {
        void ackAction(id, { ok: false, error: "failed: the block reached OMP's editor during a session change; nothing ran" });
      } else if (kind.startsWith("instruction")) {
        ackInstruction(id, "dropped");
      }
    }
    if (!found) return false;
    ui.setEditorText?.(kept + text.slice(at));
    return true;
  }


  function activateRootSession(ctx: any, sessionStartSource = "startup"): boolean {
    if (ctx?.hasUI !== true) {
      return false;
    }
    activations += 1;
    rootSession = true;
    // A new or changed session has no compaction of its own running.
    compacting = false;
    // A new or changed session has no running tool or open dialog of its own.
    runningTools.clear();
    detailRefused = false;
    dialogs = [];
    detailCtx = ctx;
    // A block herdr wrote while OMP had no listener (it drops them all when a session change
    // starts) reached the editor as a paste. Take it out, tell herdr nothing ran, and stop using
    // the token it showed. Every registration gets a new token anyway.
    rotateBlockToken(!scrubLeakedBlocks(ctx));
    clearDroppedText(ctx?.ui);
    // `/move` and `/wt` rename the session file and emit no event: look every few seconds, so
    // herdr learns the new path (for `expected_session` and for resume) without waiting for a
    // write or a turn.
    if (!moveWatch) {
      moveWatch = setInterval(() => {
        if (rootSession && instructionListener && detailCtx) followSessionMove(detailCtx);
      }, moveWatchMs);
      moveWatch.unref?.();
    }
    onRegistered = (lost) => {
      if (lost) publishState(true);
      if (instructionListener) scheduleDetail(undefined, true);
    };
    // Subagent changes show in the roster: registry events (start, status, end) and the tool
    // events of subagent bindings, at most one report a second.
    onSubagentChange = () => {
      if (rootSession) scheduleDetail();
    };
    // Run counting needs to know when OMP's executor finished a run (`runFinished`).
    void loadLifecycleManager(pi.pi?.AgentRegistry);
    if (!unsubscribeRegistry) {
      try {
        const off = agentRegistry()?.onChange?.((event: any) => {
          if (event?.ref?.kind === "sub" && typeof event.ref.id === "string") observeRun(event.ref);
          onSubagentChange?.();
        });
        if (typeof off === "function") unsubscribeRegistry = off;
      } catch {}
    }
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

  // A subagent's binding of this module adds what OMP's registry lacks: the agent type and the
  // tool the subagent runs. Returns whether `ctx` is a subagent's.
  function noteSubagent(event: string, data: any, ctx: any): boolean {
    const agent = ctx?.agent;
    if (agent?.kind !== "sub" || typeof agent.id !== "string") return false;
    const info = subagentEntry(agent.id);
    if (typeof agent.name === "string" && agent.name) info.type = agent.name;
    if (event === "message_start") {
      const message = data?.message;
      if (message?.role !== "user" || message.attribution === "agent") return true;
      const text = messageText(message.content);
      const steer = text === undefined ? -1 : info.steers.indexOf(text);
      if (steer >= 0) info.steers.splice(steer, 1);
      else info.person = true;
    } else if (event === "tool_execution_start") {
      info.tool = { name: cap(data?.toolName || "tool", 120), call_id: data?.toolCallId, started_ms: Date.now() };
    } else if (event === "tool_execution_end" && info.tool?.call_id === data?.toolCallId) {
      info.tool = undefined;
    }
    onSubagentChange?.();
    return true;
  }

  pi.on("session_start", (_event, ctx) => {
    if (noteSubagent("session_start", _event, ctx)) return;
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

  pi.on("message_start", (event, ctx) => {
    if (noteSubagent("message_start", event, ctx)) return;
    const message = event?.message;
    if (!rootSession || awaitingAdmission.length === 0 || message?.role !== "user") {
      return;
    }
    const text = messageText(message.content);
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
    if (noteSubagent("tool_execution_start", event, ctx)) return;
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
    if (noteSubagent("tool_execution_end", event, ctx)) return;
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

  // See the comment at the `session_before_*` handlers.
  let withdrawalWatch: ReturnType<typeof setInterval> | undefined;
  function watchWithdrawal(ctx: any, activation: number, sessionKey: string | undefined) {
    // A newer withdrawal supersedes the previous one's watch.
    clearInterval(withdrawalWatch);
    let stableSince = Date.now();
    let last = sessionKey;
    const deadline = Date.now() + 10 * 60_000;
    const watch = setInterval(() => {
      if (activation !== activations || !rootSession || Date.now() > deadline) {
        clearInterval(watch);
        return;
      }
      const now = readSessionIdKey(ctx);
      if (now !== last) {
        last = now;
        stableSince = Date.now();
        return;
      }
      if (now !== sessionKey || Date.now() - stableSince < switchWaitMs) return;
      clearInterval(watch);
      registerInstructionListener(ctx);
      if (!followSessionMove(ctx)) void reportSession();
    }, switchPollMs);
    watch.unref?.();
    withdrawalWatch = watch;
  }

  // OMP drops every input listener when a session change starts (`/new`, `/resume`, fork,
  // branch), before these events, and registers none until the change ends. Tell herdr at once,
  // so it refuses writes instead of putting a block into the editor. `session_switch` or
  // `session_branch` registers the listener again.
  // A change can still fail or be cancelled after these events (a failed fork, another
  // extension's cancel, a `/btw` branch that throws), and then no session event follows. OMP 18.4.4
  // gives extensions no way to see that a change has ended, so the integration watches: once a
  // second, it reads the session OMP runs (without storing it), by id, so a `/move` or `/wt` does
  // not count. It registers the listener again only after OMP has run the session it ran before
  // the event, unchanged, for `switchWaitMs` (40 s, above OMP's 30 s cap per handler) with no
  // activation in between. A change that is still
  // applying (other extensions' handlers, a flush, an advisor drain) keeps it withdrawn; any change
  // of the session OMP runs (a swap, a rollback) starts the wait over. Residual: two other
  // handlers that each hit the cap take longer than the wait; a block that then arrives after
  // OMP changed its session is still refused at take time (`liveSessionMoved`). The withdrawal
  // report gets one attempt of at most 1 s, so a herdr that accepts but does not answer delays the
  // person's switch by that much.
  for (const event of ["session_before_switch", "session_before_branch"]) {
    pi.on(event, async (_event, ctx) => {
      if (!rootSession) return undefined;
      unsubscribeInstructions?.();
      unsubscribeInstructions = undefined;
      instructionListener = false;
      // The session OMP runs now, which is the one before the event (OMP swaps later), by id: a
      // `/move` before or during the wait keeps it.
      watchWithdrawal(ctx, activations, readSessionIdKey(ctx) ?? currentSessionKey());
      const sessionRef = currentSessionRef();
      if (sessionRef) {
        await sendRequestAttempt(sessionReport("startup", sessionRef), shutdownReportTimeoutMs);
      }
      return undefined;
    });
  }

  pi.on("session_shutdown", async () => {
    if (!rootSession) {
      return;
    }
    activations += 1;
    clearInterval(moveWatch);
    moveWatch = undefined;
    unsubscribeRegistry?.();
    unsubscribeRegistry = undefined;
    onSubagentChange = undefined;
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
