"use client";

// The drawer shell and its three panels. "play" directs the scene (choosing the camera's next
// stop, writing a word, dropping a light, how the streams arrive, the stats overlay). "about"
// is what the page is (title, sentence, counters, recent actions). "isolation" is how a square
// is isolated. Each header button opens its own panel in the same shell, so they share the
// close button, Escape and focus handling, and each opens at its top. On a phone the header has
// no room for a third icon, so About carries the isolation section too (`isolationInAbout`). Every label comes from
// the view model; every action goes through WallClient.act, which holds it until the Turnstile
// check passes when the coordinator asks for one.

import { useEffect, useRef } from "react";
import { wordArgs, type WallView } from "../../core/index.ts";
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

export type DrawerPanel = "play" | "about" | "isolation";

export function PlayDrawer({ client, view, onClose, panel, isolationInAbout, word, setWord }: { client: WallClient; view: WallView; onClose: () => void; panel: DrawerPanel; isolationInAbout: boolean; word: string; setWord: (word: string) => void }) {
  const p = view.play!;
  const shell = useRef<HTMLElement | null>(null);
  const title = panel === "about" ? view.title : panel === "isolation" ? view.isolation.title : p.title;

  useEffect(() => {
    // Every panel opens at its top: focus without scrolling, then pin the scroll to 0.
    const target = panel === "play" ? shell.current?.querySelector<HTMLElement>("button, input") : shell.current?.querySelector<HTMLElement>(`#${panel}-title`);
    target?.focus({ preventScroll: true });
    if (shell.current) shell.current.scrollTop = 0;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose, panel]);

  return (
    <aside ref={shell} className="drawer" role="dialog" aria-modal="false" aria-labelledby={`${panel}-title`} data-testid="drawer" data-panel={panel}>
      <header>
        <h2 id={`${panel}-title`} tabIndex={panel === "play" ? undefined : -1}>
          {title}
        </h2>
        <button type="button" className="btn" onClick={onClose}>
          {p.close}
        </button>
      </header>
      {/* In the shell, so a queued action's check is there whichever panel is open. */}
      <VerifyBox client={client} view={view} />
      {panel === "about" ? <AboutSection view={view} /> : null}
      {panel === "about" && isolationInAbout ? <IsolationSection view={view} heading /> : null}
      {panel === "isolation" ? <IsolationSection view={view} /> : null}
      {panel === "play" ? <PlaySections client={client} view={view} word={word} setWord={setWord} /> : null}
    </aside>
  );
}

function PlaySections({ client, view, word, setWord }: { client: WallClient; view: WallView; word: string; setWord: (word: string) => void }) {
  const p = view.play!;
  return (
    <>
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
            client.act("word", wordArgs(text, client.getState().ui.textStyle));
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
        <fieldset className="styles" data-testid="text-style">
          <legend>{p.word.style.legend}</legend>
          {p.word.style.options.map((o) => (
            <label key={o.value} className={`style-option${o.selected ? " selected" : ""}`} data-testid={`text-style-${o.value}`}>
              <input
                type="radio"
                name="text-style"
                value={o.value}
                checked={o.selected}
                onChange={() => client.ui({ textStyle: o.value })}
              />
              <img src={o.thumb} alt={o.alt} width={96} height={54} />
              <span>{o.label}</span>
            </label>
          ))}
        </fieldset>
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
        <p className="note" data-testid="family">
          {p.stats.family}
        </p>
      </section>
    </>
  );
}

function AboutSection({ view }: { view: WallView }) {
  return (
    <section className="about" data-testid="about">
      <p data-testid="sentence">{view.sentence}</p>
        {view.counter ? (
          <p className="counter" data-testid="counter">
            {view.counter.label}:{" "}
            {/* A new key per total restarts the highlight, so the count visibly ticks. */}
            <b key={view.counter.total} className="tick" data-testid="counter-value">
              {view.counter.value}
            </b>
            <span className="sub">{view.counter.lastHour}</span>
            {view.counter.wake ? (
              <span className="sub" data-testid="wake-stat">
                {view.counter.wake}
              </span>
            ) : null}
            {view.counter.scrubs ? (
              <span className="sub" data-testid="counter-scrubs">
                {view.counter.scrubs}
              </span>
            ) : null}
          </p>
        ) : null}
        {view.feed.length > 0 ? (
          <ol className="feed" aria-label="Recent actions" data-testid="feed">
            {view.feed.map((x) => (
              <li key={x.key}>
                {x.text} <span className="when">{x.when}</span>
              </li>
            ))}
          </ol>
        ) : null}
    </section>
  );
}

function IsolationSection({ view, heading = false }: { view: WallView; heading?: boolean }) {
  const iso = view.isolation;
  return (
    <section className="isolation" data-testid="isolation">
      {heading ? <h3>{iso.title}</h3> : null}
      {iso.sentences.map((t) => (
        <p key={t}>{t}</p>
      ))}
      <a href={iso.link.href} target="_blank" rel="noopener noreferrer" data-testid="isolation-repo">
        {iso.link.label}
      </a>
    </section>
  );
}
