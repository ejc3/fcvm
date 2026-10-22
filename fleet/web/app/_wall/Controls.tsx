"use client";

// The controls bar: Relaunch all, the shared auto-relaunch setting, the stats
// switch, dropping a light, writing a word, the time of day. Every label comes
// from the view model.

import { useRef, useState } from "react";
import { autoFromValue, type WallView } from "../../core/index.ts";
import type { WallClient } from "./wall-client.ts";

function RelaunchIcon() {
  return (
    <svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d="M3 12a9 9 0 0 1 15.3-6.4L21 8" />
      <path d="M21 3v5h-5" />
      <path d="M21 12a9 9 0 0 1-15.3 6.4L3 16" />
      <path d="M3 21v-5h5" />
    </svg>
  );
}

function AutoSelect({ client, view, id }: { client: WallClient; view: WallView; id: string }) {
  const a = view.controls!.auto;
  return (
    <select
      id={id}
      value={a.selected}
      onChange={(e) => client.send("set_auto_relaunch", { secs: autoFromValue(e.target.value) })}
      aria-describedby={`${id}-note`}
    >
      {a.options.map((o) => (
        <option key={o.value} value={o.value}>
          {o.label}
        </option>
      ))}
    </select>
  );
}

export function Controls({ client, view, compact }: { client: WallClient; view: WallView; compact: boolean }) {
  const c = view.controls!;
  const [word, setWord] = useState("");
  const todTimer = useRef<{ last: number; pending: number | null; timer: ReturnType<typeof setTimeout> | null }>({ last: 0, pending: null, timer: null });

  const sendTod = (v: number) => {
    const s = todTimer.current;
    s.pending = v;
    const go = () => {
      if (s.pending === null) return;
      client.send("time_of_day", { v: s.pending });
      s.pending = null;
      s.last = performance.now();
      s.timer = null;
    };
    if (performance.now() - s.last > 150) go();
    else if (!s.timer) s.timer = setTimeout(go, 150);
  };

  const wordForm = (
    <form
      className="field word-form"
      onSubmit={(e) => {
        e.preventDefault();
        const text = word.trim();
        if (text === "") return;
        client.send("word", { text });
        setWord("");
      }}
    >
      <label htmlFor={compact ? "word-p" : "word"} className={compact ? undefined : "sr-only"}>
        {c.word.label}
      </label>
      <input
        id={compact ? "word-p" : "word"}
        type="text"
        value={word}
        maxLength={c.word.maxLength}
        placeholder={c.word.placeholder}
        autoComplete="off"
        spellCheck={false}
        onChange={(e) => setWord(e.target.value)}
      />
      <button type="submit" className="btn">
        {c.word.submit}
      </button>
      {compact ? (
        <span className={`word-status ${c.word.statusKind}`} role="status" data-testid="word-status">
          {c.word.status}
        </span>
      ) : null}
    </form>
  );

  const tod = (
    <div className="field">
      <label htmlFor={compact ? "tod-p" : "tod"}>{c.timeOfDay.label}</label>
      <input
        id={compact ? "tod-p" : "tod"}
        type="range"
        min={c.timeOfDay.min}
        max={c.timeOfDay.max}
        step={c.timeOfDay.step}
        value={c.timeOfDay.value}
        aria-valuetext={c.timeOfDay.valueText}
        onChange={(e) => sendTod(Number(e.target.value))}
      />
      <span className="value">{c.timeOfDay.valueText}</span>
    </div>
  );

  const mode = (
    <div className="field" role="group" aria-labelledby={compact ? "mode-label-p" : "mode-label"}>
      <span className="label" id={compact ? "mode-label-p" : "mode-label"}>
        {c.mode.label}
      </span>
      <span className="segmented">
        {c.mode.options.map((o) => (
          <button
            key={o.value}
            type="button"
            aria-pressed={c.mode.selected === o.value}
            disabled={o.disabled}
            data-testid={`mode-${o.value}`}
            onClick={() => client.ui({ mode: o.value, modeChosen: true })}
          >
            {o.label}
          </button>
        ))}
      </span>
      <span className="note" data-testid="mode-note" title={`${c.mode.detail}. ${c.mode.suggestion.text}`}>
        {c.mode.note}
      </span>
    </div>
  );

  const toggles = (
    <>
      <button type="button" className="btn" role="switch" aria-checked={c.stats.on} onClick={() => client.ui({ stats: !c.stats.on })} data-testid="stats-switch">
        <span className="switch" aria-hidden="true" />
        {c.stats.label}
      </button>
      <button type="button" className="btn" aria-pressed={c.light.on} onClick={() => client.ui({ lightMode: !c.light.on })}>
        {c.light.label}
      </button>
    </>
  );

  if (compact) {
    return (
      <div>
        {mode}
        {toggles}
        {wordForm}
        {tod}
      </div>
    );
  }

  return (
    <div role="toolbar" aria-label="Wall controls" className="bar">
      <span className="site">{c.site}</span>
      <button type="button" className="btn primary" disabled={c.relaunchAll.disabled} onClick={() => client.send("relaunch_all")} data-testid="relaunch-all">
        <RelaunchIcon />
        {c.relaunchAll.label}
        {c.relaunchAll.note ? <span style={{ fontWeight: 400 }}>· {c.relaunchAll.note}</span> : null}
      </button>
      <div className="field">
        <label htmlFor="auto">{c.auto.label}</label>
        <AutoSelect client={client} view={view} id="auto" />
        <span className="note" id="auto-note" data-testid="auto-note">
          {c.auto.note}
        </span>
      </div>
      {mode}
      {toggles}
      {wordForm}
      {tod}
      <span className="meta">
        <span data-testid="viewers">{c.viewers}</span>
      </span>
      {view.notices.length > 0 ? (
        <div className="notices" style={{ bottom: "calc(100% + 10px)" }} role="status">
          {view.notices.map((n) => (
            <div key={n.key} className="notice">
              {n.text}
            </div>
          ))}
        </div>
      ) : null}
    </div>
  );
}

export function PhoneDock({ client, view }: { client: WallClient; view: WallView }) {
  const c = view.controls!;
  return (
    <div className="dock">
      <button type="button" className="btn primary" disabled={c.relaunchAll.disabled} onClick={() => client.send("relaunch_all")}>
        {c.relaunchAll.disabled ? c.relaunchAll.label : "Relaunch all"}
      </button>
      <label htmlFor="auto-p" className="sr-only">
        {c.auto.label}
      </label>
      <AutoSelect client={client} view={view} id="auto-p" />
      <span className="sr-only" id="auto-p-note">
        {c.auto.note}
      </span>
    </div>
  );
}
