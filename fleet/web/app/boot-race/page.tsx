import type { Metadata } from "next";
import { laneTime, parseBootRaceSummary, utc } from "../../core/boot-race.ts";
import raw from "./summary.json";
import "./boot-race.css";

// Recorded results from one boot-race campaign (bench/boot-race), never live. The summary is checked when the page is
// built: a missing summary or a lane without its configuration fails the build (core/boot-race.ts).
export const dynamic = "force-static";

const summary = parseBootRaceSummary(raw);

export const metadata: Metadata = {
  title: "Boot race (recorded results)",
  description: `Recorded results of boot-race campaign ${summary.campaign}: time from asking for compute to a workload answering, fcvm restores against EC2, ECS and Lambda.`,
};

const REPO = "https://github.com/ejc3/fcvm";

export default function BootRacePage() {
  const s = summary;
  return (
    <main className="br-page" data-testid="boot-race">
      <div className="br-wrap">
        <p className="br-nav">
          <a href="/">fcvm</a> <span aria-hidden="true">/</span> boot race
        </p>
        <h1>Time to a running workload</h1>
        <p className="br-recorded" data-testid="boot-race-recorded">
          <strong>Recorded results, not live.</strong> These numbers come from one campaign, {s.campaign}, run{" "}
          {utc(s.window_utc.first_sample)} to {utc(s.window_utc.last_sample)}. Nothing on this page is measured when you
          load it.
        </p>
        <p className="br-lead">
          Each time runs from the call that asks for compute until a GET from the campaign host has read the
          workload&apos;s complete answer. Every lane runs the same small program.
        </p>

        <dl className="br-facts">
          <div><dt>Campaign</dt><dd>{s.campaign}, {s.rounds} rounds, every lane once per round</dd></div>
          <div><dt>Window (UTC)</dt><dd>{utc(s.window_utc.first_sample)} to {utc(s.window_utc.last_sample)}</dd></div>
          <div><dt>fcvm commit</dt><dd><a href={`${REPO}/commit/${s.fcvm_commit}`}><code>{s.fcvm_commit}</code></a></dd></div>
          <div><dt>Region</dt><dd>{s.region}</dd></div>
          <div><dt>Campaign host</dt><dd>{s.campaign_host}{s.host_load ? `; ${s.host_load}` : ""}</dd></div>
          <div><dt>Records</dt><dd><a href={s.records.url}>{s.records.repository} at <code>{s.records.commit.slice(0, 8)}</code></a></dd></div>
          <div><dt>Full page</dt><dd><a href={s.page_url}>the campaign page, with every sample</a></dd></div>
        </dl>

        {s.not_run.map((l) => (
          <p className="br-notrun" key={l.id} data-testid={`boot-race-notrun-${l.id}`}>
            <strong>Not run: {l.name}.</strong> {l.why}
          </p>
        ))}

        <h2>Results</h2>
        <p className="br-muted">Fastest median first. p95 and p99 are nearest-rank; with {s.rounds} samples p99 is the slowest one.</p>
        <ol className="br-lanes">
          {s.lanes.map((l) => (
            <li key={l.id} className={`br-lane br-${l.id.split("-")[0]}`} data-testid={`boot-race-lane-${l.id}`}>
              <div className="br-lane-head">
                <h3>{l.name}</h3>
                <span className="br-median">{laneTime(l.median_ms)}</span>
              </div>
              <p className="br-stats">
                median {laneTime(l.median_ms)} · p95 {laneTime(l.p95_ms)} · p99 {laneTime(l.p99_ms)} · range{" "}
                {laneTime(l.min_ms)} to {laneTime(l.max_ms)} · n {l.n}
                {l.failures > 0 ? ` · ${l.failures} failed` : ""}
              </p>
              <p><span className="br-k">Clock starts:</span> {l.start}</p>
              <p><span className="br-k">Configuration:</span> {l.config}</p>
              {l.check ? <p className="br-muted">{l.check}</p> : null}
            </li>
          ))}
        </ol>

        <p className="br-muted br-foot">
          Source: <a href={`${REPO}/tree/main/bench/boot-race`}>bench/boot-race</a>. This page is built from{" "}
          <code>app/boot-race/summary.json</code>, which <code>bench/boot-race/summary.py</code> writes from the
          published campaign page&apos;s data.
        </p>
      </div>
    </main>
  );
}
