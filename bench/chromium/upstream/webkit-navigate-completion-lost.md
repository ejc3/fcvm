# WebKitGTK: navigate never completes, and its 300 s timeout never fires

## LEADING HYPOTHESIS (traced in Source/, supported by a prediction; the patch is not built or tested)

`WebPageProxy::didFinishLoadForFrame` (WebPageProxy.cpp:7996) gates the
automation notification on the navigation still being registered:

```cpp
RefPtr<API::Navigation> navigation;
if (frame->isMainFrame() && navigationID && m_navigationState->hasNavigation(*navigationID))
    navigation = m_navigationState->navigation(*navigationID);

bool isMainFrame = frame->isMainFrame();
if (!isMainFrame || !navigationID || navigation) {          // <-- gate
    ...
    automationSession->navigationOccurredForFrame(*frame);
    automationSession->loadCompletedForFrame(*frame, navigationID, WallTime::now());
}
```

The sibling path does NOT gate it. `didFinishDocumentLoadForFrame`
(WebPageProxy.cpp:7832) notifies automation unconditionally, before it even
looks the navigation up. That asymmetry is the candidate defect.

Two IPC messages from the WebProcess race:

* `DidDestroyNavigation`, sent by
  `WebLocalFrameLoaderClient::documentLoaderDetached`
  (WebProcess/WebCoreSupport/WebLocalFrameLoaderClient.cpp:218)
* `DidFinishLoadForFrame`

If the former is processed first the `API::Navigation` is already retired,
`hasNavigation()` is false, the gate skips the whole block, and the WebDriver
callback is left in `m_pendingNormalNavigationInBrowsingContextCallbacksPerPage`
forever.

**Supporting prediction.** WebDriver's `normal` strategy resolves via the GATED
`navigationOccurredForFrame`; `eager` resolves via the UNGATED
`documentLoadedForFrame`. So `eager` should never stall. Measured on 2.52.5,
same page, same protocol, fresh container per trial:

| pageLoadStrategy | stalled |
|---|---|
| normal | 13/31 (42%) |
| **eager** | **0/8** |

Proposed patch: `webkit-didFinishLoadForFrame-automation.patch` in this
directory moves the automation notification out of the gate, matching
`didFinishDocumentLoadForFrame`. NOT YET BUILT OR TESTED against a WebKit build;
the eager/normal split is consistent with the gate being the cause but does not
isolate it (eager also changes when the wait resolves), so the asymmetry stays
a hypothesis until the patched `normal` path is measured to recover.


Two defects, filed together because the second is what makes the first fatal.
Reproduced on **WebKitGTK 2.52.5** (current upstream stable) and **2.50.6**,
aarch64, Debian trixie/bookworm, `WebKitWebDriver --port=9515 --host=all`,
MiniBrowser `--automation` under Xvfb.

## 1. POST /session/<id>/url never returns, though the page has finished

**Rate: 13 of 31 fresh sessions (42%, 95% CI [26%, 59%]).** When it does not
stall it returns in 1.4 s.

It is not a hang. Probed during a confirmed stall, navigate still outstanding
at t=25 s:

```
document.readyState                     -> "complete"
page's own marker element               -> "done layout_ms=1145.0 canvas_ms=160.0 checksum=65030"
document.querySelectorAll("tr").length  -> 1200
GET /status, /url, /title               -> 200, all under 3 ms
```

The page loaded and its script completed in ~1.3 s. JavaScript still executes.
`WebKitWebProcess` instantaneous CPU during the stall is 0.3% of one core,
having consumed ~8 CPU-seconds all within the first ~6 s. Only the WebDriver
command fails to complete.

Chromium 151.0.7922.137 renders the identical page in 1387.6 ms with no stalls.

## 2. The pageLoad timeout does not fire, so the stall is unrecoverable

`GET /session/<id>/timeouts` reports `pageLoad: 300000`, and
`WebAutomationSession.cpp:92` has `defaultPageLoadTimeout = 300_s`. One
observed navigate ran **390 s with no response at all** — the client's own
timeout, not a WebDriver `timeout` error.

Nothing else can rescue it: `Source/WebDriver/Session.cpp` `Session::go` arms no
timer of its own, it sends `navigateBrowsingContext` and waits on the backend
indefinitely. So a client that waits on this command waits forever.

Structural note offered without a claim to have proven it fires here:
`WebAutomationSession` keeps a **single shared `m_loadTimer`** for four separate
pending-callback maps (per-page normal/eager, per-frame normal/eager), and every
resolving wait calls `m_loadTimer.stop()` (lines 962, 968, 993, 1002).
`loadTimerFired()` then responds to all four maps. One wait resolving therefore
cancels every other pending wait's deadline.

## Reproducer

Self-contained, 3302 bytes, no network subresources beyond a `data:` favicon,
no rAF/setInterval/Worker/fetch/WebGL. One synchronous script before the load
event:

- build a 1200-row x 6-column table (7200 cells), calling `appendChild` and
  reading `table.offsetHeight` every 100 rows — ~12 forced layouts of a growing
  table
- then canvas 800x600: `createLinearGradient`, `fillRect`, arcs, text
- then `getImageData` to force raster

Full fixture: `bench/chromium/pages/heavy.html` in this repository.

```
POST /session/<id>/url  {"url": "http://127.0.0.1:8000/heavy.html"}
```

Repeat on fresh sessions; roughly 2 in 5 never return.

## What we ruled out, with the measurement

| hypothesis | ruled out by |
|---|---|
| slow layout / still computing | CPU idle at 0.3% during the stall; page marker shows it finished |
| crash | session answers /status, /url, /title in under 3 ms |
| old version | reproduces on 2.52.5, current upstream stable |
| accelerated-canvas GL fence with no timeout | `WEBKIT_DISABLE_COMPOSITING_MODE=1` gave 3/10 vs 6/10, Fisher p = 0.370 — not significant. NOTE: we did not verify the env var actually disabled the accelerated path, so this negative is only as strong as the intervention. |
| concurrency | reproduces at N=1 |

## Client-side workaround (what we shipped)

`pageLoadStrategy: "none"` plus polling `document.readyState` over
`execute/sync`, with a sentinel global planted on the previous document so a
stale `"complete"` cannot be mistaken for the new one.
`waitForNavigationToCompleteOnPage` returns on its first branch when the
strategy is None, so the broken completion path is never entered.

Result: **0 failures in 30 fresh sessions**, against 13/31 before.

This is a workaround, not a fix: any WebDriver client using the default
`normal` page load strategy is exposed, and defect 2 means it has no timeout to
fall back on.
