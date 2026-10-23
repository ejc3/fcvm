"use client";

// The Play drawer: choosing the camera's next stop, writing a word, dropping a
// light, the time of day, how the streams arrive, and the stats overlay. Every
// label comes from the view model; every action goes through WallClient.act,
// which holds it until the Turnstile check passes when the coordinator asks for one.

import { useEffect, useRef, useState } from "react";
import type { WallView } from "../../core/index.ts";
import { TurnstileWidget } from "./Turnstile.tsx";
import type { WallClient } from "./wall-client.ts";

export function VerifyBox({ client, view }: { client: WallClient; view: WallView }) {
  const v = view.play?.verify;
  const sitekey = client.getState().turnstile.sitekey;
  if (!v || !v.needed || sitekey === null) return null;
  return (
    <div className="verify" data-testid="verify">
      <p>{v.text}</p>
      <TurnstileWidget
        sitekey={sitekey}
        onToken={(token) => client.verify(token)}
        onError={(error) => client.dispatch({ type: "turnstile-error", error })}
      />
      {v.error ? <p className="error" role="alert">{v.error}</p> : null}
    </div>
  );
}

export function PlayDrawer({ client, view, onClose, section }: { client: WallClient; view: WallView; onClose: () => void; section: string | null }) {
  const p = view.play!;
  const [word, setWord] = useState("");
  const panel = useRef<HTMLElement | null>(null);
  const iso = view.isolation;
  const todTimer = useRef<{ last: number; pending: number | null; timer: ReturnType<typeof setTimeout> | null }>({ last: 0, pending: null, timer: null });

  useEffect(() => {
    const target = section ? panel.current?.querySelector<HTMLElement>(`#${section}-title`) : null;
    if (target) {
      target.scrollIntoView({ block: "start" });
      target.focus();
    } else {
      panel.current?.querySelector<HTMLElement>("button, input")?.focus();
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose, section]);

  const sendTod = (v: number) => {
    const s = todTimer.current;
    s.pending = v;
    const go = () => {
      if (s.pending === null) return;
      client.act("time_of_day", { v: s.pending });
      s.pending = null;
      s.last = performance.now();
      s.timer = null;
    };
    if (performance.now() - s.last > 150) go();
    else if (!s.timer) s.timer = setTimeout(go, 150);
  };

  return (
    <aside ref={panel} className="drawer" role="dialog" aria-modal="false" aria-labelledby="play-title" data-testid="drawer">
      <header>
        <h2 id="play-title">{p.title}</h2>
        <button type="button" className="btn" onClick={onClose}>
          {p.close}
        </button>
      </header>
      <VerifyBox client={client} view={view} />
      {p.nudge ? (
        <section aria-labelledby="nudge-title">
          <h3 id="nudge-title">{p.nudge.title}</h3>
          <p className="note" data-testid="nudge-note">{p.nudge.note}</p>
          <div className="choices">
            {p.nudge.options.map((o) => (
              <button
                key={o.id}
                type="button"
                className="btn choice"
                aria-pressed={o.chosen}
                disabled={p.nudge!.disabled}
                data-testid={`nudge-${o.id}`}
                onClick={() => client.act("nudge", { target: o.id })}
              >
                <span>{o.label}</span>
                <span className="votes">{o.holding ? "holding now" : o.votes}</span>
              </button>
            ))}
          </div>
        </section>
      ) : null}
      <section>
        <h3 id="word-title">{p.word.label}</h3>
        <form
          className="word-form"
          onSubmit={(e) => {
            e.preventDefault();
            const text = word.trim();
            if (text === "") return;
            client.act("word", { text });
            setWord("");
          }}
        >
          <label htmlFor="word" className="sr-only">
            {p.word.label}
          </label>
          <input
            id="word"
            type="text"
            value={word}
            maxLength={p.word.maxLength}
            placeholder={p.word.placeholder}
            autoComplete="off"
            spellCheck={false}
            disabled={p.disabled}
            onChange={(e) => setWord(e.target.value)}
          />
          <button type="submit" className="btn" disabled={p.disabled}>
            {p.word.submit}
          </button>
        </form>
        <p className={`word-status ${p.word.statusKind}`} role="status" data-testid="word-status">
          {p.word.status}
        </p>
      </section>
      <section className="row">
        <button
          type="button"
          className="btn"
          aria-pressed={p.light.on}
          disabled={p.disabled}
          data-testid="light"
          onClick={() => {
            client.ui({ lightMode: !p.light.on, drawer: p.light.on });
          }}
        >
          {p.light.label}
        </button>
      </section>
      <section className="field">
        <label htmlFor="tod">{p.timeOfDay.label}</label>
        <input
          id="tod"
          type="range"
          min={p.timeOfDay.min}
          max={p.timeOfDay.max}
          step={p.timeOfDay.step}
          value={p.timeOfDay.value}
          aria-valuetext={p.timeOfDay.valueText}
          disabled={p.disabled}
          onChange={(e) => sendTod(Number(e.target.value))}
        />
        <span className="value">{p.timeOfDay.valueText}</span>
      </section>
      <section>
        <button
          type="button"
          className="btn switch-btn"
          role="switch"
          aria-checked={p.streams.on}
          disabled={p.streams.disabled}
          data-testid="streams-switch"
          onClick={() => client.ui({ mode: p.streams.on ? "aggregated" : "direct" })}
        >
          <span className="switch" aria-hidden="true" />
          {p.streams.label}
        </button>
        <p className="note" data-testid="streams-note">{p.streams.note}</p>
      </section>
      <section>
        <button
          type="button"
          className="btn switch-btn"
          role="switch"
          aria-checked={p.stats.on}
          data-testid="stats-switch"
          onClick={() => client.ui({ stats: !p.stats.on })}
        >
          <span className="switch" aria-hidden="true" />
          {p.stats.label}
        </button>
      </section>
      <section className="isolation" aria-labelledby="isolation-title" data-testid="isolation">
        <h3 id="isolation-title" tabIndex={-1}>
          {iso.title}
        </h3>
        {iso.sentences.map((t) => (
          <p key={t}>{t}</p>
        ))}
        <a href={iso.link.href} target="_blank" rel="noopener noreferrer" data-testid="isolation-repo">
          {iso.link.label}
        </a>
      </section>
    </aside>
  );
}
