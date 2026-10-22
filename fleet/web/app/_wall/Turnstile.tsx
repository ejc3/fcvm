"use client";

// Cloudflare Turnstile, loaded only when the coordinator asks for it. The
// widget's token goes to the coordinator on /live; the actions pressed before
// it are sent once the coordinator says the connection is verified.

import { useEffect, useRef } from "react";

const SCRIPT = "https://challenges.cloudflare.com/turnstile/v0/api.js?render=explicit";

interface TurnstileApi {
  render(el: HTMLElement, opts: { sitekey: string; callback: (token: string) => void; "error-callback"?: () => void; theme?: string }): string | undefined;
  remove?(id: string): void;
}

declare global {
  interface Window {
    turnstile?: TurnstileApi;
  }
}

let loading: Promise<TurnstileApi> | null = null;

function loadTurnstile(): Promise<TurnstileApi> {
  if (window.turnstile) return Promise.resolve(window.turnstile);
  loading ??= new Promise((resolve, reject) => {
    const s = document.createElement("script");
    s.src = SCRIPT;
    s.async = true;
    s.onload = () => (window.turnstile ? resolve(window.turnstile) : reject(new Error("Turnstile did not load")));
    s.onerror = () => {
      loading = null;
      reject(new Error("Turnstile did not load"));
    };
    document.head.appendChild(s);
  });
  return loading;
}

export function TurnstileWidget({ sitekey, onToken, onError }: { sitekey: string; onToken: (token: string) => void; onError: (e: string) => void }) {
  const el = useRef<HTMLDivElement | null>(null);
  const cb = useRef({ onToken, onError });
  cb.current = { onToken, onError };
  useEffect(() => {
    let id: string | undefined;
    let api: TurnstileApi | null = null;
    let gone = false;
    loadTurnstile()
      .then((t) => {
        if (gone || !el.current) return;
        api = t;
        id = t.render(el.current, {
          sitekey,
          theme: "dark",
          callback: (token) => cb.current.onToken(token),
          "error-callback": () => cb.current.onError("the check did not load; try again"),
        });
      })
      .catch((e: unknown) => cb.current.onError(e instanceof Error ? e.message : "the check did not load"));
    return () => {
      gone = true;
      if (api && id !== undefined) api.remove?.(id);
    };
  }, [sitekey]);
  return <div ref={el} className="turnstile" data-testid="turnstile" />;
}
