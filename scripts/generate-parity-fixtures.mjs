// Generates tests/fixtures/parity.json by running the TypeScript whoop-ai-mcp
// server against a deterministic synthetic WHOOP dataset with a frozen clock.
//
// Usage: TZ=UTC node scripts/generate-parity-fixtures.mjs <path-to-whoop-mcp-checkout>
// (the checkout must be built with `npm run build`).

import { writeFileSync } from "node:fs";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";

const repo = resolve(process.argv[2] ?? "../whoop-mcp");
const load = (p) => import(pathToFileURL(resolve(repo, p)).href);
const { createWhoopServer } = await load("dist/server.js");
const { WhoopApiError } = await load("dist/api/client.js");
const { InMemoryTransport } = await load("node_modules/@modelcontextprotocol/sdk/dist/esm/inMemory.js");

// ---------------------------------------------------------------------------
// Frozen clock
// ---------------------------------------------------------------------------
const NOW = Date.parse("2026-09-30T12:00:00.000Z");
const RealDate = Date;
class FrozenDate extends RealDate {
  constructor(...args) {
    if (args.length === 0) super(NOW);
    else super(...args);
  }
  static now() {
    return NOW;
  }
}
globalThis.Date = FrozenDate;

// ---------------------------------------------------------------------------
// Deterministic dataset
// ---------------------------------------------------------------------------
let seed = 42;
const rnd = () => {
  seed = (seed * 1103515245 + 12345) % 2147483648;
  return seed / 2147483648;
};
const round = (x, digits) => Math.round(x * 10 ** digits) / 10 ** digits;
const iso = (ms) => new RealDate(ms).toISOString();
const HOUR = 3_600_000;
const DAY = 86_400_000;
const OFFSETS = ["+00:00", "+05:30", "-07:00", "+01:00"];
const SPORTS = ["Running", "Cycling", "Weightlifting", "Yoga"];

function buildDataset() {
  const cycles = [];
  const sleeps = [];
  const recoveries = [];
  const workouts = [];
  const midnight = Date.UTC(2026, 8, 30);
  for (let d = 0; d < 120; d++) {
    const dayStart = midnight - d * DAY;
    const offset = OFFSETS[d % OFFSETS.length];
    const wake = dayStart + 5 * HOUR + Math.floor(rnd() * 2 * HOUR);
    const sleepStart = wake - Math.floor((6 + rnd() * 3) * HOUR);
    const cycleId = 5000 + d;
    const sleepId = `sleep-${String(d).padStart(3, "0")}`;
    const state = d % 23 === 7 ? "PENDING_SCORE" : d % 31 === 11 ? "UNSCORABLE" : "SCORED";
    const light = Math.floor((2.5 + rnd() * 1.5) * HOUR);
    const deep = Math.floor((1 + rnd()) * HOUR);
    const rem = Math.floor((1 + rnd() * 1.2) * HOUR);
    const awake = Math.floor(rnd() * 0.8 * HOUR);
    sleeps.push({
      id: sleepId,
      cycle_id: cycleId,
      v1_id: 90000 + d,
      user_id: 7,
      created_at: iso(wake + 10 * 60_000),
      updated_at: iso(wake + 20 * 60_000),
      start: iso(sleepStart),
      end: iso(wake),
      timezone_offset: offset,
      nap: false,
      score_state: state,
      score:
        state === "SCORED"
          ? {
              stage_summary: {
                total_in_bed_time_milli: light + deep + rem + awake,
                total_awake_time_milli: awake,
                total_no_data_time_milli: 0,
                total_light_sleep_time_milli: light,
                total_slow_wave_sleep_time_milli: deep,
                total_rem_sleep_time_milli: rem,
                sleep_cycle_count: 3 + Math.floor(rnd() * 3),
                disturbance_count: Math.floor(rnd() * 10),
              },
              sleep_needed: {
                baseline_milli: 8 * HOUR,
                need_from_sleep_debt_milli: Math.floor(rnd() * HOUR),
                need_from_recent_strain_milli: Math.floor(rnd() * 0.5 * HOUR),
                need_from_recent_nap_milli: d % 5 === 0 ? -Math.floor(0.3 * HOUR) : 0,
              },
              respiratory_rate: d % 9 === 4 ? null : round(14 + rnd() * 3, 3),
              sleep_performance_percentage: d % 13 === 3 ? null : Math.floor(60 + rnd() * 40),
              sleep_consistency_percentage: Math.floor(50 + rnd() * 50),
              sleep_efficiency_percentage: round(80 + rnd() * 19, 4),
            }
          : undefined,
    });
    if (d % 5 === 2) {
      const napStart = dayStart + 14 * HOUR;
      sleeps.push({
        id: `nap-${d}`,
        cycle_id: cycleId,
        user_id: 7,
        created_at: iso(napStart + HOUR),
        updated_at: iso(napStart + HOUR),
        start: iso(napStart),
        end: iso(napStart + 0.5 * HOUR),
        timezone_offset: offset,
        nap: true,
        score_state: "SCORED",
        score: sleeps[sleeps.length - 1].score ?? null,
      });
    }
    if (d % 11 === 6) {
      sleeps.push({ ...sleeps.find((s) => s.id === sleepId), id: `${sleepId}-dup`, start: iso(sleepStart + HOUR) });
    }
    cycles.push({
      id: cycleId,
      user_id: 7,
      created_at: iso(sleepStart + 60_000),
      updated_at: iso(wake + HOUR),
      start: iso(sleepStart),
      end: d === 0 ? null : iso(sleepStart + DAY + Math.floor(rnd() * HOUR)),
      timezone_offset: offset,
      score_state: d % 29 === 13 ? "PENDING_SCORE" : "SCORED",
      score:
        d % 29 === 13
          ? null
          : {
              strain: round(4 + rnd() * 15, 6),
              kilojoule: round(6000 + rnd() * 8000, 3),
              average_heart_rate: 55 + Math.floor(rnd() * 30),
              max_heart_rate: 140 + Math.floor(rnd() * 50),
            },
    });
    const recoveryState = state === "SCORED" ? (d % 19 === 3 ? "PENDING_SCORE" : "SCORED") : state;
    recoveries.push({
      cycle_id: cycleId,
      sleep_id: sleepId,
      user_id: 7,
      created_at: iso(wake + 15 * 60_000),
      updated_at: iso(wake + 25 * 60_000),
      score_state: recoveryState,
      score:
        recoveryState === "SCORED"
          ? {
              user_calibrating: d > 110,
              recovery_score: Math.floor(10 + rnd() * 89),
              resting_heart_rate: Math.floor(45 + rnd() * 20),
              hrv_rmssd_milli: round(30 + rnd() * 90, 5),
              spo2_percentage: d % 6 === 1 ? null : round(94 + rnd() * 5, 3),
              skin_temp_celsius: round(32 + rnd() * 2, 4),
            }
          : null,
    });
    if (d % 2 === 0) {
      const workoutStart = dayStart + 17 * HOUR - (d === 0 ? DAY : 0);
      workouts.push({
        id: `workout-${d}`,
        v1_id: 70000 + d,
        user_id: 7,
        created_at: iso(workoutStart + 2 * HOUR),
        updated_at: iso(workoutStart + 2 * HOUR),
        start: iso(workoutStart),
        end: iso(workoutStart + HOUR + Math.floor(rnd() * HOUR)),
        timezone_offset: offset,
        sport_name: SPORTS[(d / 2) % SPORTS.length],
        sport_id: d % 4,
        score_state: d % 14 === 8 ? "UNSCORABLE" : "SCORED",
        score:
          d % 14 === 8
            ? null
            : {
                strain: round(3 + rnd() * 14, 6),
                average_heart_rate: 110 + Math.floor(rnd() * 40),
                max_heart_rate: 150 + Math.floor(rnd() * 40),
                kilojoule: round(800 + rnd() * 2000, 3),
                percent_recorded: 100,
                distance_meter: d % 4 === 0 ? round(3000 + rnd() * 10000, 2) : null,
                altitude_gain_meter: null,
                altitude_change_meter: null,
                zone_durations: {
                  zone_zero_milli: 60_000,
                  zone_one_milli: 600_000,
                  zone_two_milli: 900_000,
                  zone_three_milli: 1_200_000,
                  zone_four_milli: 300_000,
                  zone_five_milli: 60_000,
                },
              },
      });
    }
  }
  return {
    profile: { user_id: 7, email: "athlete@example.com", first_name: "Ada", last_name: "Lovelace" },
    body: { height_meter: 1.75, weight_kilogram: 70.5, max_heart_rate: 190 },
    collections: {
      "/v2/cycle": cycles,
      "/v2/recovery": recoveries,
      "/v2/activity/sleep": JSON.parse(JSON.stringify(sleeps)),
      "/v2/activity/workout": workouts,
    },
    errors: {},
  };
}

const full = buildDataset();
const EMPTY = { "/v2/cycle": [], "/v2/recovery": [], "/v2/activity/sleep": [], "/v2/activity/workout": [] };
const variants = {
  full: { errors: {}, empty: false },
  errors: { errors: { "/v2/activity/workout": 500, "/v2/recovery": 401, "/v2/user/profile/basic": 429 }, empty: false },
  empty: { errors: {}, empty: true },
  outage: {
    errors: { "/v2/cycle": 503, "/v2/recovery": 503, "/v2/activity/sleep": 503, "/v2/activity/workout": 503 },
    empty: false,
  },
};
const datasets = Object.fromEntries(
  Object.entries(variants).map(([name, v]) => [
    name,
    { ...full, errors: v.errors, collections: v.empty ? EMPTY : full.collections },
  ])
);

// ---------------------------------------------------------------------------
// Fake WHOOP API (mirrored exactly by tests/parity.rs)
// ---------------------------------------------------------------------------
const KEY_FIELD = { "/v2/recovery": "created_at" };

function fakeClient(dataset) {
  return {
    async get(path) {
      const [base, query = ""] = path.split("?");
      const params = new URLSearchParams(query);
      for (const [prefix, status] of Object.entries(dataset.errors)) {
        if (base === prefix || base.startsWith(`${prefix}/`)) throw new WhoopApiError(status, "Error", {});
      }
      if (base === "/v2/user/profile/basic") return dataset.profile;
      if (base === "/v2/user/measurement/body") return dataset.body;
      for (const [endpoint, records] of Object.entries(dataset.collections)) {
        if (base.startsWith(`${endpoint}/`)) {
          const id = base.slice(endpoint.length + 1);
          const found = records.find((r) => String(r.id) === id);
          if (!found) throw new WhoopApiError(404, "Error", {});
          return found;
        }
        if (base !== endpoint) continue;
        const key = KEY_FIELD[endpoint] ?? "start";
        const start = params.get("start");
        const end = params.get("end");
        const filtered = records
          .filter((r) => (start === null || Date.parse(r[key]) >= Date.parse(start)) && (end === null || Date.parse(r[key]) < Date.parse(end)))
          .sort((a, b) => Date.parse(b[key]) - Date.parse(a[key]));
        const limit = Math.min(Number.parseInt(params.get("limit") ?? "10", 10), 25);
        const offset = Number.parseInt(params.get("nextToken") ?? "0", 10);
        const page = filtered.slice(offset, offset + limit);
        const next = offset + limit < filtered.length ? String(offset + limit) : null;
        return { records: page, next_token: next };
      }
      throw new WhoopApiError(404, "Error", {});
    },
  };
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------
const tool = (name, args = {}, dataset = "full", privacy = "standard") => ({ kind: "tool", name, args, dataset, privacy });
const resource = (uri, dataset = "full") => ({ kind: "resource", uri, dataset, privacy: "standard" });

const cases = [
  tool("get_profile"),
  tool("get_body_measurement"),
  tool("get_recovery_collection", { start: "last 7 days", limit: 5 }),
  tool("get_sleep_collection"),
  tool("get_workout_collection", { start: "2026-09-01", end: "2026-09-10T00:00:00Z", limit: 25 }),
  tool("get_cycle_collection", { limit: 10, nextToken: "10" }),
  tool("get_cycle_collection", { start: "not a date" }),
  tool("get_sleep_by_id", { id: "sleep-004" }),
  tool("get_workout_by_id", { id: "workout-4" }),
  tool("get_cycle_by_id", { id: 5003 }),
  tool("get_weekly_summary"),
  tool("get_weekly_summary", { week_start: "last week" }),
  tool("get_weekly_summary", { week_start: "2026-09-07" }),
  tool("get_weekly_summary", { week_start: "2026-02-31" }),
  tool("compare_periods", { period_a_start: "2026-07-01", period_a_end: "2026-07-31", period_b_start: "2026-08-01", period_b_end: "2026-08-30" }),
  tool("compare_periods", { period_a_start: "2026-08-01", period_a_end: "2026-08-20", period_b_start: "2026-08-10", period_b_end: "2026-08-30" }),
  ...["recovery", "hrv", "rhr", "sleep_duration", "sleep_performance", "strain"].map((metric) => tool("get_trend", { metric, days: 30 })),
  tool("get_trend", { metric: "hrv", days: 90 }),
  tool("get_today"),
  tool("get_calendar"),
  tool("get_calendar", { days: 14 }),
  tool("get_calendar", { start: "2026-09-20", days: 30 }),
  tool("get_calendar", { start: "2027-01-01" }),
  tool("get_calendar", { days: 45 }),
  tool("get_baselines"),
  tool("get_baselines", { baseline_days: 90 }),
  tool("get_sleep_debt"),
  tool("get_sleep_debt", { days: 45 }),
  tool("get_sleep_debt", { start: "last 7 days", days: 7 }),
  tool("get_sleep_debt", { start: "2026-10-05" }),
  tool("get_weekly_summary", {}, "full", "aggregate"),
  tool("compare_periods", { period_a_start: "2026-07-01", period_a_end: "2026-07-31", period_b_start: "2026-08-01", period_b_end: "2026-08-30" }, "full", "aggregate"),
  tool("get_trend", { metric: "recovery", days: 30 }, "full", "aggregate"),
  tool("get_baselines", {}, "full", "aggregate"),
  tool("get_sleep_debt", { days: 30 }, "full", "aggregate"),
  tool("get_profile", {}, "errors"),
  tool("get_recovery_collection", {}, "errors"),
  tool("get_workout_collection", {}, "errors"),
  tool("get_weekly_summary", {}, "errors"),
  tool("get_today", {}, "errors"),
  tool("get_calendar", {}, "errors"),
  tool("get_baselines", {}, "errors"),
  tool("get_trend", { metric: "strain", days: 14 }, "errors"),
  tool("get_weekly_summary", {}, "outage"),
  tool("get_today", {}, "outage"),
  tool("get_baselines", {}, "outage"),
  tool("get_sleep_debt", {}, "outage"),
  tool("get_trend", { metric: "recovery", days: 7 }, "empty"),
  tool("get_today", {}, "empty"),
  tool("get_calendar", { days: 3 }, "empty"),
  tool("get_baselines", {}, "empty"),
  tool("get_sleep_debt", {}, "empty"),
  tool("get_weekly_summary", {}, "empty"),
  resource("whoop://v2/user/recovery/latest"),
  resource("whoop://v2/user/sleep/latest"),
  resource("whoop://v2/user/cycle/latest"),
  resource("whoop://v2/user/profile"),
  resource("whoop://v2/user/cycle/latest", "empty"),
  resource("whoop://v2/user/profile", "errors"),
];

async function run(testCase) {
  const { server } = createWhoopServer(fakeClient(datasets[testCase.dataset]), { privacyMode: testCase.privacy });
  const [clientSide, serverSide] = InMemoryTransport.createLinkedPair();
  const pending = new Map();
  clientSide.onmessage = (message) => pending.get(message.id)?.(message);
  await server.connect(serverSide);
  await clientSide.start();
  let id = 0;
  const request = (method, params) =>
    new Promise((resolvePromise) => {
      id += 1;
      pending.set(id, resolvePromise);
      void clientSide.send({ jsonrpc: "2.0", id, method, params });
    });
  await request("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "parity", version: "1" } });
  const response =
    testCase.kind === "tool"
      ? await request("tools/call", { name: testCase.name, arguments: testCase.args })
      : await request("resources/read", { uri: testCase.uri });
  await server.close();
  return response.result ?? response.error;
}

const results = [];
for (const testCase of cases) {
  const result = await run(testCase);
  results.push({ ...testCase, result });
  process.stderr.write(`${testCase.kind} ${testCase.name ?? testCase.uri} [${testCase.dataset}/${testCase.privacy}] ${result.isError ? "error" : "ok"}\n`);
}

const out = resolve(new URL("../tests/fixtures/parity.json", import.meta.url).pathname);
writeFileSync(
  out,
  `${JSON.stringify({ now: iso(NOW), base: { ...full, errors: undefined }, variants, cases: results })}\n`
);
process.stderr.write(`wrote ${results.length} cases to ${out}\n`);
