# PRODUCT.md — Hotkey preview readiness

UX research and product definition for sekio's first-run and capability-setup
experience. Written as the source input for a later design pass; it defines the
experience and the evidence behind it, not the implementation.

---

## 1. Overview

sekio's headline capability is a *quick look*: press a global hotkey and the
file your file manager currently has selected appears instantly. That single
gesture depends on a chain of system capabilities — a resident daemon, a
registered global hotkey, a resolvable selection, a clipboard helper, and
optionally a tray host — each of which can fail independently, for reasons the
user did not cause and cannot see.

Today sekio diagnoses all of this exceptionally well and tells almost nobody.
The entire report lives in `sekio-gui --doctor`, a command-line tool that a
person who installed a graphical file previewer will never run. The result is
the product's worst failure mode: the user presses the hotkey and **nothing
happens at all** — no window, no error, no explanation.

This document defines **Hotkey preview readiness**: an in-application way to
set up the hotkey feature, understand which parts of it are working, repair the
parts that are not, and *verify with a live test* that the whole chain actually
functions end to end. It is deliberately optional and non-blocking — sekio must
remain instantly usable by someone who only ever drags a file onto it.

---

## 2. Existing Product Context

### 2.1 What sekio already is

A quick-view tool for any filetype. One core library turns a path into a
frontend-neutral representation; thin frontends paint it. The GUI
(`sekio-gui`) is one of three frontends, alongside a CLI and a TUI. Linux and
Windows are both first-class targets; macOS is explicitly out of scope.

### 2.2 Entry points into the GUI

| Entry | What it produces |
|---|---|
| `sekio-gui <path>` | A one-shot preview popup over whatever the user was doing |
| `sekio-gui` (no path) | An application window, opening on the home screen |
| `sekio-gui --daemon` | A resident background process serving a socket, plus a tray icon |
| Global hotkey | The daemon previews the file currently selected in the file manager |
| File-manager keybinding | A spacebar (or similar) binding invoking `sekio-gui <path>` |
| Tray menu | Open, Recent, Hotkey, Quit |

### 2.3 The governing concept: Mode

Everything about dismissal hangs on one three-valued idea, and **any new
surface must respect it**:

- **Popup** — launched with a path. Dismissing is the whole point; it exits.
- **App** — launched deliberately with no path. It must not vanish on Escape.
- **Daemon** — resident. Dismissing hides the window, keeping the process warm.

A Popup is *promoted* to an App the moment the user opens something through
sekio itself — a dialog, the browser, a drop, a recent entry. At that point
they are no longer using a transient popup; they are using a viewer.

### 2.4 Current screens and states

| State | Content |
|---|---|
| Home | Wordmark and version, "Open file…" and "Browse files", drop hint, Recent list, Keys legend |
| Loading | Spinner in the header |
| Ready | Header (filename, `n / m` sibling position, `truncated` badge, Open…, Browse, ⚙) plus content |
| Failed | An error message in place of content |
| Browser panel | Built-in file browser (Ctrl+B), needs no portal |
| Open dialog | Native file dialog (Ctrl+O), falls back to the browser panel |
| Settings menu | ⚙ → Theme (Follow the desktop / Light / Dark), and where config lives |
| Tray menu | Open, Recent, Hotkey (five presets), Quit |
| Drop overlay | Drag-over hint |

The home screen is a **centred column capped at 720px**, vertically
scrollable. Recent holds up to ten entries. This is a launcher a user passes
through in about a second — not a dashboard they dwell on.

### 2.5 The capability chain behind one hotkey press

1. **Daemon resident** — without it there is nothing to summon, and every popup
   pays for a fresh process instead of a ~5 ms socket handoff.
2. **Hotkey registered** — needs X11 or XWayland. On a Wayland-only or headless
   session it cannot be grabbed at all. Another application may already own the
   combination.
3. **Selection resolves** — on Linux, best-effort only: PRIMARY selection,
   then clipboard, then a bare filename matched against open folders.
4. **Clipboard helper present** — one of `wl-paste`, `xclip`, `xsel`.
5. **Tray host present** — optional. Stock GNOME needs the AppIndicator
   extension. Its absence does not stop the daemon or the hotkey.
6. **Desktop portal** — optional, for the native Open dialog only.
7. **Writable config directory** — otherwise a hotkey chosen in the tray cannot
   be remembered.

On **Windows** the picture is much simpler: Explorer answers directly over COM,
so steps 3 and 4 need nothing installed.

### 2.6 Linux selection coverage — partial by design

No Linux file manager exposes its selection over a public API. The product is
already honest about this:

| Desktop | Behaviour |
|---|---|
| KDE / Dolphin | Selecting fills PRIMARY — the hotkey works with no copying |
| GNOME / Nautilus | Publishes nothing — the user must press Ctrl+C first |
| XFCE, Nemo, Caja, PCManFM | Copy-then-hotkey works; live selection varies |
| Anywhere | A path or `file://` URI copied from a terminal, editor or browser |

This is the single most important fact in this document: **the feature's
quality is environment-dependent, and the environment is not the user's
fault.** Every piece of copy must reflect that.

### 2.7 The diagnosis that already exists

`sekio-gui --doctor` reports: console attachment (Windows only), selection
strategy with a **live probe** (what it can see right now, from where, and in
how many milliseconds), hotkey (spec, which of three config layers it came
from, parsed form, display server, and a **real grab-and-release test**), open
dialog availability, daemon socket and running state, tray hosting (an actual
spawn probe), and configuration. Every failing row is followed by a concrete
"→ do this next". It always exits 0 — it is a report, not a test.

This work is done and is the raw material for the entire experience below. The
problem is purely one of surfacing.

### 2.8 Terminology already in use

"preview", "Recent", "Keys", "truncated", "Follow the desktop", "Quick preview
for any file", "daemon", "tray". New surfaces should extend this vocabulary
rather than introduce a competing one.

### 2.9 Constraints that must not be broken

- **A hotkey press that resolves nothing must do nothing visible.** Returning
  no selection is a normal outcome, not an error.
- **Registration failure is never fatal.** The daemon still serves its socket
  and `sekio-gui <path>` keeps working.
- **No tray host is an ordinary outcome**, not a failure.
- **Core previewing is never gated behind setup.**
- Mode semantics (§2.3) decide what dismissal means, everywhere.

---

## 3. Target User Experience

**Clarity.** A user who wants the hotkey should be able to learn, in one
glance, whether it will work — and if not, which single link in the chain is
broken. "It doesn't work" must become "the clipboard helper isn't installed".

**Confidence.** Because the daemon is invisible by nature, the user needs
verifiable evidence that it is running: not a claim, but a state with
supporting detail they can check.

**Effort.** Setup is optional and interruptible. Nothing about it may slow the
path from launching sekio to looking at a file. A user who never wants the
hotkey should be able to ignore and permanently dismiss the whole thing.

**Progression.** Readiness is a small number of discrete checks with a visible
count. The remaining work is always bounded and legible — "1 step left", not
an open-ended "configure the hotkey".

**Feedback.** The decisive moment is a live end-to-end test the user watches
happen: they select a file, press the key, and sekio reports what it actually
resolved, from where, and how fast. Configured and *working* are collapsed into
one observed event.

**Recovery.** Every failing check owns its own remedy. Where sekio cannot act
on the user's behalf — installing a package, enabling a shell extension,
changing session type — it says exactly what to do instead of pretending it can
fix it. A copyable report is the terminal escape hatch.

**Honesty above all.** Where support is partial (GNOME), sekio says so and
gives the workaround. Where a capability is impossible in this session
(Wayland-only), it says that too and offers the documented alternative. It
never reports success it has not observed.

---

## 4. User Journey

### Trigger

The user launches sekio as an application (no path) and lands on Home. The
readiness card is present because at least one capability check is unmet. It is
never shown in Popup mode.

Secondary triggers: ⚙ → "Hotkey preview setup", and the tray menu.

### Steps

1. On Home, below the primary actions, a single collapsed row reads:
   **"Hotkey preview — 3 of 5 ready · Set up"**.
2. The user activates it. A readiness panel opens, listing checks
   **severity-first**: unmet ones at the top, each with a plain statement of
   the consequence and one action; met ones collapsed into "3 checks passed".
3. The user works through the problems. Some sekio can act on directly (start
   the daemon, enable start-at-login, choose a different hotkey). Others it can
   only instruct (install a clipboard helper, enable the AppIndicator
   extension) — those offer copyable commands and a documentation link.
4. When the chain is plausibly complete, the panel's primary action becomes
   **"Try it now"**.
5. Activating it arms a listening state: *"Select a file in your file manager
   and press Ctrl+Shift+Space. sekio is watching."* with a visible Cancel.
6. The user leaves sekio, selects a file, presses the key.
7. sekio reports the outcome specifically: the resolved path, whether it came
   from the file manager or the clipboard, and the elapsed milliseconds. This
   becomes a durable **"Last verified"** record, not a transient toast.
8. With everything green and a successful verification, the card removes itself
   from Home.

### Decision points

- **Set up now or ignore.** Dismissal is permanent but recoverable from ⚙.
- **Which hotkey.** Default `Ctrl+Shift+Space`, plus four presets. A
  modifier-less typing key earns a warning: grabbing it globally takes it from
  every other application.
- **Start at login.** On by default from the deb/rpm/MSI installers; absent
  when built from source.
- **Skip verification.** Permitted; readiness then reads "not verified" rather
  than green.

### Exit conditions

- All checks pass and a live test succeeded → card disappears; readiness is
  reachable only from ⚙.
- User dismisses the card → gone from Home, still in ⚙.
- User abandons midway → state persists exactly as left; no wizard restarts.

### Failure / recovery

Every unmet check carries its own remedy. Where a capability is impossible in
this session, the panel says so plainly and points at the documented
alternative — binding a desktop shortcut to `sekio-gui <path>`. A **Copy
report** action reproduces the `--doctor` content for a bug report.

---

## 5. Alternate Flows & Edge Cases

**Wayland-only or headless session.** The hotkey cannot be grabbed at all. The
hotkey row is shown *read-only* with one plain banner stating why and what to
do instead. It is never hidden — hiding it makes the feature undiscoverable and
turns a known limitation into a mystery.

**Hotkey already owned by another application.** The OS reveals this only at
registration, so the message is necessarily *post-hoc*: "couldn't register —
another application may already own this combination". sekio cannot name the
owner and must not pretend to. Offer the presets.

**Daemon not running.** Readiness shows it as the first unmet check, because
everything downstream depends on it. Offer to start it, and offer start-at-login.

**Daemon running but no tray host.** Explicitly **not an error**. Overall
status stays green; the tray appears as a degraded *component* row —
"Tray icon: unavailable — nothing in this session hosts one" — with a "Why?"
link. The daemon serves its socket and answers its hotkey regardless.

**No clipboard helper installed (Linux).** Selection cannot be read. A single
check with copyable install commands for `wl-paste` / `xclip` / `xsel`.

**GNOME / Nautilus.** Not a failure — a *partial* state. The check reads
something like "works, but press Ctrl+C first", with the reason. Flattening
this to a red cross would be wrong and would misrepresent a working setup.

**No writable config directory.** A hotkey chosen in the tray cannot be
remembered. Surfaced where the hotkey is chosen, at the moment it matters.

**Capability regression.** A previously-working setup can break outside sekio —
a helper uninstalled, a session switched from X11 to Wayland. Checks are
re-evaluated when the readiness surface is opened, not cached from first run.

**An unprompted hotkey press that resolves nothing.** Per §2.9 this stays
**silent**. See §8, Pattern F — this is the one place the research is overruled.

**Popup mode.** No setup surface, ever. A popup was asked for as a transient
preview; putting configuration in it would violate the Mode contract.

**Windows.** Steps 3 and 4 collapse — Explorer answers directly. The readiness
list is correspondingly shorter and must not show Linux-only rows.

**First run with everything already working.** Common for deb/rpm/MSI installs,
where the installer enabled autostart. No card appears. Nothing is celebrated;
there is nothing to fix.

---

## 6. Screen & State Inventory

### Screen: Readiness card (Home)

**Purpose** — A single, low-cost signal that the hotkey feature is not fully
set up, plus the way in.

**Entry conditions** — Home is showing; Mode is App or Daemon; at least one
check is unmet; the card has not been dismissed.

**Primary actions** — Open the readiness panel; dismiss.

**Exit / transition** — Opens the panel; or dismisses and remains reachable
from ⚙. Self-removes when all checks pass and verification has succeeded.

---

### Screen: Readiness panel

**Purpose** — The full picture: what is working, what is not, what to do about
each, and how to test the whole thing.

**Entry conditions** — From the readiness card, from ⚙, or from the tray.

**Primary actions** — Act on an unmet check; expand the passing group; change
the hotkey; toggle start at login; "Try it now"; copy report.

**Exit / transition** — Closes back to whatever was showing. Escape follows
existing Mode semantics and must not exit an App.

---

### State: Check row — unmet

**Purpose** — Name one broken link, its consequence, and its remedy.

**Entry conditions** — That capability probe failed.

**Primary actions** — The remedy: a real action where sekio can act, copyable
instructions where it cannot.

**Exit / transition** — Re-probe on demand; the row moves into the passing
group when satisfied.

---

### State: Check row — partial

**Purpose** — Represent "works, with a caveat" without lying in either
direction. GNOME selection is the motivating case.

**Entry conditions** — The capability resolves, but only under a condition the
user must know.

**Primary actions** — Read the caveat; open the coverage documentation.

**Exit / transition** — Stays partial; this is a stable, acceptable end state.

---

### State: Check group — passing (collapsed)

**Purpose** — Reassurance without a wall of rows. "4 checks passed."

**Entry conditions** — At least one check passes.

**Primary actions** — Expand to audit.

**Exit / transition** — Collapses again.

---

### Screen: Hotkey setting

**Purpose** — Show the current binding, where it came from, and whether this
session will actually hand the key over.

**Entry conditions** — From the readiness panel or ⚙.

**Primary actions** — Choose a preset; see the registration result; read the
modifier-less warning; read the read-only banner when no display server exists.

**Exit / transition** — Persists to config where a config directory exists;
says so plainly where it does not.

---

### State: Verification — armed

**Purpose** — The listening state. sekio is watching for a real press.

**Entry conditions** — "Try it now" activated.

**Primary actions** — Perform the action in another application; Cancel.

**Exit / transition** — Resolves to success or no-resolve. Must survive sekio
losing focus, since the user has to leave the window to complete the test.

---

### State: Verification — result

**Purpose** — Report exactly what happened: resolved path, origin (file manager
or clipboard), elapsed milliseconds. On failure, name probable causes keyed to
the detected desktop.

**Entry conditions** — An armed test resolved, or the user cancelled.

**Primary actions** — Retry; act on a named cause; copy report.

**Exit / transition** — Becomes the durable "Last verified" record.

---

### State: Daemon status

**Purpose** — Make an invisible process trustworthy through evidence, not
assertion.

**Entry conditions** — Present in the readiness panel.

**Primary actions** — Start or stop; toggle start at login.

**Exit / transition** — Updates in place.

---

### Screen: Report (details)

**Purpose** — The technical tier: socket paths, timings, parsed hotkey, config
layers. Deliberately one level down, and the bug-report path.

**Entry conditions** — "Details" or "Copy report".

**Primary actions** — Read; copy to clipboard.

**Exit / transition** — Returns to the panel.

---

## 7. Interaction Requirements

- **Progressive disclosure, two tiers.** Consumer view answers "is it broken,
  and where?"; a details tier holds paths, timings and parsed values. The
  default view must never read as a developer console.
- **Severity ordering.** Unmet checks first; passing checks collapsed behind a
  count. Ordering is by severity, not by the order the code probes them.
- **Checks are machine-diagnosed, not user-ticked.** They can complete *and
  regress* without any user action, so the panel re-evaluates on open rather
  than storing a "step completed" flag.
- **Three states, not two.** Met / partial / unmet. Collapsing partial into
  either neighbour misrepresents the most common Linux configuration.
- **Consequence framing.** Each row states what the *user* loses, not what the
  system lacks: "the hotkey won't find your selection", not "xclip absent".
- **Post-hoc registration reporting.** Conflicts are only knowable after a grab
  attempt, so the hotkey UI reports failure after the fact and cannot warn
  before a choice.
- **Armed state survives backgrounding.** The verification test requires the
  user to act in another application; the armed state and its result must be
  intact and legible on return.
- **Durable verification record.** The outcome persists as "Last verified" with
  a timestamp. A toast alone is insufficient — it disappears exactly when the
  user is looking elsewhere.
- **Non-blocking and dismissible.** No modal, no overlay, nothing that could
  intercept a file drop or an Open click on Home.
- **Mode-aware.** Setup surfaces exist in App and Daemon modes only. Escape
  keeps its existing meaning per Mode.
- **Silence preserved.** An unprompted press that resolves nothing produces no
  window and no sound. Feedback is earned only inside an armed test.
- **Platform-shaped lists.** Windows shows a shorter chain; Linux-only rows must
  not appear there.

---

## 8. UX Patterns & Research Findings

### Pattern A — Capability inventory with per-row consequence and repair

**Research finding.** Apps that depend on capabilities they cannot grant
themselves converge on a persistent, app-owned inventory: one row per
capability, a state chip, a one-sentence consequence, and a repair action.
Amazon notably carries a *partial* state ("Enabled for 27 features") rather
than flattening to on/off; Pillow links each permission to what breaks without
it; Polarsteps shows a mock-up of the exact rows to change.

**Decision.** ADAPT.

**Application.** This is the backbone of the readiness panel: daemon, hotkey,
selection source, clipboard helper, tray host. The partial state maps precisely
onto GNOME selection. What does not transfer is the mobile priming-then-system-
dialog choreography — there is no OS grant prompt to prime, and repairs are
shell commands and extension installs rather than a deep link.
[Grill'd](https://mobbin.com/flows/dc3724e3-44a1-4989-b20a-1f83580d8314) ·
[Amazon](https://mobbin.com/flows/009965fd-7dcd-4b6f-888a-46a42641f363) ·
[Pillow](https://mobbin.com/flows/20a862ec-f712-43ef-8c15-6ab56a8fef18) ·
[Polarsteps](https://mobbin.com/screens/6decd8ec-be62-40be-b6da-6855d526ac89)

---

### Pattern B — Severity-sorted diagnostics with a collapsed "passing" group

**Research finding.** Diagnostic views run visible checks, summarise in one
sentence, then sort by severity — failures on top with verb-phrase remedies,
passes folded away ("These are running smoothly"). A separate technical tier
(Starlink's Debug data) holds raw values behind an extra step and offers
export. Symptom-worded labels beat implementation-worded ones.

**Decision.** ADAPT.

**Application.** Renders `--doctor` as a panel rather than a console: a headline
verdict, failures as rows carrying the existing "→ do this next" text as body
copy plus a real action, and everything healthy folded into "4 checks passed".
Timings, socket paths and parsed specs move to the details tier, which doubles
as the bug-report path. What does not transfer: mobile's deep-link to OS
settings — on Linux a "fix" frequently can only *instruct*.
[Grab Driver](https://mobbin.com/flows/a668c642-5206-4d9c-937f-cc0e97187139) ·
[results screen](https://mobbin.com/screens/f21cc30a-b0e8-4c8e-9de8-3a829706255c) ·
[FotMob](https://mobbin.com/flows/7fb3b7bd-2ea6-4da2-9c74-de43c42f4e49) ·
[IKEA Home smart](https://mobbin.com/flows/3da11b05-d72c-423f-8325-36b0e68fabd4) ·
[Starlink](https://mobbin.com/flows/6324be24-9061-4d82-8a22-10a07190b3b1) ·
[Tempo](https://mobbin.com/screens/e18d7a87-0f30-4a1f-a947-9e9e1a89cfa4)

---

### Pattern C — Setup as a persistent, collapsible object

**Research finding.** Optional setup is a durable object carrying "X of Y"
state beside the working UI — never a wizard in front of it. Steps complete in
any order, the object is always dismissible, and it shrinks or vanishes at
100%. Mercedes states the *consequence* of incompleteness rather than the
mechanics.

**Decision.** ADAPT.

**Application.** The readiness card on Home, collapsed to one line with a
count, expanding into the panel, self-removing when complete. Consequence
framing is adopted directly. Two adaptations are required: sekio's home is a
launcher passed through in a second, so the floating/modal treatments (Cosmos,
PayPal) are rejected outright — they would intercept a drop or an Open click.
And sekio's steps are machine-diagnosed, so they must auto-refresh rather than
tick on user action, and can regress — which no reference handles.
[Linktree](https://mobbin.com/flows/3d38cff6-4af8-4a18-9cc1-b06e0739d797) ·
[Monarch](https://mobbin.com/screens/368e875d-7f7c-4cf2-8ede-a3e5d222f127) ·
[Mercedes-Benz](https://mobbin.com/flows/32f77d37-386d-45ea-955b-ed81eb85f46e) ·
[Cosmos](https://mobbin.com/flows/b68385e7-5136-4bbc-a972-9aaa9686e3e7) ·
[PayPal](https://mobbin.com/screens/7c30842b-02a0-474c-a75f-d54f2d5a7b75)

---

### Pattern D — Live verification with a durable, specific result

**Research finding.** The test affordance sits next to the setting it
validates: idle button → armed state with live text → a specific, timestamped
outcome → a failure path naming probable causes. Where the app cannot observe
the final hop it says so rather than claiming success. PlanetScale makes the
result durable state on the object; Brick's armed state instructs an action
performed outside the UI entirely.

**Decision.** ADAPT — the strongest and most directly applicable finding.

**Application.** "Try it now" arms a listening state; the user selects a file
elsewhere and presses the key; sekio reports resolved path, origin and elapsed
milliseconds, and keeps it as "Last verified". Failure shows causes keyed to
the detected desktop (GNOME → "press Ctrl+C first"). The honest hedge covers
the case where the grab succeeded but the selection was empty. The one thing no
reference solves: the UI being read is not the UI being acted in, so the armed
state must survive losing focus.
[Todoist](https://mobbin.com/flows/e7c64523-2be1-423a-9b1e-51a5eb73f4cf) ·
[PlanetScale](https://mobbin.com/flows/129b1a6a-ed96-4ff6-8683-aded64ba0447) ·
[Brick](https://mobbin.com/screens/1f573609-cead-40f1-8dce-027f1d23e1e2) ·
[Perplexity](https://mobbin.com/flows/f7eaed9e-5c03-4f88-b50d-c7f6224700da) ·
[incident.io](https://mobbin.com/flows/35b94543-5300-43e0-9512-3664836b3b52)

---

### Pattern E — Status object for a resident service

**Research finding.** Background services are represented ambiently: a status
object combining a state word, a colour and *verifiable evidence* (uptime,
version, component rows). One primary action toggles the service and is
labelled with the next action. Disclosures explain why the thing must keep
running. Twingate breaks a single service into per-component rows, so one
degraded component does not read as total failure.

**Decision.** ADAPT.

**Application.** A daemon status block: running state with supporting detail, a
"Start at login" toggle carrying a one-line why ("keeps the hotkey instant"),
and — critically — **the tray as a component row**. The healthy-but-invisible
case (daemon fine, no tray host) stays green overall with one degraded row,
which is exactly the product's existing position. The connect/disconnect
framing is rejected: sekio's daemon is a local process, not a network session.
Nothing in the mobile corpus models an optional second surface that may be
absent — that row is ours to invent.
[Twingate](https://mobbin.com/screens/d904c7ad-efff-4547-92b2-3db440005f69) ·
[NordVPN](https://mobbin.com/flows/8c85b6ca-c0e8-43cf-8f29-bd59fe5462a9) ·
[Opera](https://mobbin.com/flows/c542374c-7ef4-4d9d-9144-ef6a49c3d387) ·
[Lyft](https://mobbin.com/flows/ef349bee-a635-4a28-b5b3-3bfe64507616)

---

### Pattern F — In-place failure surfacing on the silent no-op

**Research finding.** Two independent streams recommended surfacing the failure
where the feature is used: Posh renders "you do not have location permissions
enabled" in the feature area itself with a settings hand-off, and the
diagnostics stream recommended auto-opening the panel after a press that
resolved nothing. On the evidence alone this is the correct fix for a silent
failure.

**Decision.** **REJECT for the unprompted case; ADAPT into the armed test.**

**Application.** This is the one place the research is deliberately overruled.
sekio has an explicit product requirement that a hotkey press resolving nothing
must do nothing visible — a global hotkey fires from anywhere, including by
accident, and a viewer that spawns a window on every stray press is worse than
one that stays quiet. The evidence is nonetheless real, so it is redirected: it
justifies the *armed* verification state (§ Pattern D), where the user has
explicitly asked to be told what happened, and it justifies keeping readiness
reachable from the tray so a puzzled user has somewhere to go. Revisiting the
unprompted case would require changing a stated product constraint — a decision
for a human, recorded in §9.
[Posh](https://mobbin.com/screens/c82a302b-0a31-4547-961f-b03235750054)

---

### Pattern G — Read-only capability banner instead of a hidden feature

**Research finding.** Discord, unable to support custom keybinds in the
browser, keeps the list visible and read-only under one banner naming the
limitation and the concrete remedy ("download the desktop application"). Height
handles conflict by *naming the owner* and asking for an explicit override
rather than silently rejecting.

**Decision.** ADAPT.

**Application.** On a Wayland-only or headless session the hotkey row is shown
read-only with a banner explaining that global hotkeys are grabbed through X11
and pointing to the desktop-shortcut alternative — never hidden. Height's
conflict shape applies with one inversion sekio cannot avoid: the OS reveals a
conflict only at registration and will not name the owner, so the message is
necessarily post-hoc and unattributed. Note the corpus here was genuinely thin
— a single conflict screen — so this pattern rests on weaker evidence than the
others.
[Discord](https://mobbin.com/screens/d747e905-e01f-488a-9edb-cfefdbd958d5) ·
[Height conflict](https://mobbin.com/screens/d0490d15-2923-4e45-b438-cf8b49d53c77) ·
[Height settings](https://mobbin.com/screens/227ea05e-2f27-4ad6-8bbf-fa8915d896f3) ·
[Zoho CRM](https://mobbin.com/screens/8291b6de-66e5-495e-91a8-c8a2b6525956) ·
[Retool](https://mobbin.com/flows/ca6eb0e2-f2ae-464e-97de-261d9c83a5d3)

---

## 9. Evidence, Assumptions & Open Questions

### Evidence-backed recommendations

- A capability inventory with per-row state, consequence and repair (Pattern A).
- Severity-sorted checks with passing ones collapsed, and a separate technical
  tier that doubles as the bug-report path (Pattern B).
- Setup as a persistent, dismissible, self-removing object rather than a wizard
  (Pattern C).
- A live "try it now" test producing a specific, durable, timestamped result
  (Pattern D).
- A daemon status object with verifiable evidence, and the tray expressed as a
  degradable component rather than a failure (Pattern E).
- A read-only row plus explanatory banner where a capability is impossible,
  rather than hiding the feature (Pattern G).
- Naming a *partial* state instead of flattening to binary — Amazon's
  "Enabled for 27 features" is the direct precedent for GNOME selection.

### Assumptions

- That the home screen is where the readiness card belongs. It is the only
  persistent surface a non-popup user reliably sees, but it is a fast launcher,
  and one extra row is a real cost against a 720px column that already carries
  Recent and Keys.
- That users want the hotkey feature at all. Many may only ever use the file
  manager binding or drag-and-drop, in which case the card is pure noise —
  which is why permanent dismissal is required rather than optional.
- That re-probing on panel open is frequent enough. Capabilities can regress
  silently; nothing here detects that in the background.
- That "3 of 5 ready" is meaningful to a user. The count is derived from
  internal probes, and the ratio may read as more precise than it is.
- That the deb/rpm/MSI majority will see no card at all, because the installer
  already enabled autostart. If that is wrong, the card is far more prominent
  in practice than intended.
- That Windows users need a materially shorter list, not a different design.

### Open questions

- **Does the unprompted silent no-op stay silent?** Recorded as Pattern F. The
  research says surface it; the product constraint says do not. Changing it is
  a human decision, and a middle path exists — a non-intrusive tray or
  home-screen record of "last press resolved nothing" that respects the "no
  window" rule while ending the total-silence problem. This is the single most
  consequential open question in the document.
- **Does the armed verification state time out, or wait indefinitely?** No
  reference resolves this; indefinite-with-Cancel is the safer default.
- **Where does readiness live when sekio is a daemon with no window and no tray
  host?** There may be genuinely no surface to put it on — arguably an argument
  for a one-time notification, which conflicts with the silence principle.
- **Should a failed verification block "setup complete", or merely annotate it?**
- **Is "Pause" a meaningful daemon action, or only Start/Stop and Quit?**
- **Does the start-at-login toggle reflect, or control, the systemd unit?**
  These can disagree — the unit is enabled globally by the installer but can be
  masked per-user, and a toggle that silently disagrees with reality would be
  worse than no toggle.
- **How is the readiness card counted on Windows**, where two of the five Linux
  checks do not exist?

---

## 10. Mobbin Reference Index

Platform note: Mobbin indexes iOS and web only. Every reference below is a
mobile or web product; none is a desktop application with a tray, a global
hotkey, or a two-application test. Each pattern in §8 states its transfer
limits.

| Reference | What to observe | Supports |
|---|---|---|
| [Grill'd — Device Permissions](https://mobbin.com/flows/dc3724e3-44a1-4989-b20a-1f83580d8314) | Per-capability cards: status label, why it's needed, deep-link button | Pattern A |
| [Amazon — Permissions dashboard](https://mobbin.com/flows/009965fd-7dcd-4b6f-888a-46a42641f363) | A genuine *partial* state, "Enabled for 27 features", beside binary rows | Pattern A, §5 GNOME |
| [Pillow — About permissions](https://mobbin.com/flows/20a862ec-f712-43ef-8c15-6ab56a8fef18) | Status list linked to per-permission explanation of what breaks | Pattern A |
| [Polarsteps — settings hand-off](https://mobbin.com/screens/6decd8ec-be62-40be-b6da-6855d526ac89) | Mock-up of the exact rows and toggle the user must change | Pattern A |
| [Posh — denied state in place](https://mobbin.com/screens/c82a302b-0a31-4547-961f-b03235750054) | Failure rendered in the feature area, with the fix named | Pattern F (rejected for unprompted case) |
| [Grab Driver — Diagnostics](https://mobbin.com/flows/a668c642-5206-4d9c-937f-cc0e97187139) | Checks run visibly, then issues-first with remedies | Pattern B |
| [Grab Driver — results](https://mobbin.com/screens/f21cc30a-b0e8-4c8e-9de8-3a829706255c) | "1 issue detected" above a collapsed "running smoothly" group | Pattern B |
| [FotMob — Network troubleshooting](https://mobbin.com/flows/7fb3b7bd-2ea6-4da2-9c74-de43c42f4e49) | Named checks ticking live; "still having an issue?" escape hatch | Pattern B |
| [IKEA Home smart — help](https://mobbin.com/flows/3da11b05-d72c-423f-8325-36b0e68fabd4) | Symptom-worded accordion, each body one plain fix | Pattern B, §7 copy |
| [Starlink — Debug data](https://mobbin.com/flows/6324be24-9061-4d82-8a22-10a07190b3b1) | A deliberately technical tier, one level down, with export | Pattern B details tier |
| [Tempo — prerequisites](https://mobbin.com/screens/e18d7a87-0f30-4a1f-a947-9e9e1a89cfa4) | Compact icon + capability + status-word rows | Pattern B |
| [Linktree — setup checklist](https://mobbin.com/flows/3d38cff6-4af8-4a18-9cc1-b06e0739d797) | Collapsed pill with "5 of 6", expanding; vanishes at 100% | Pattern C |
| [Monarch — Getting Started](https://mobbin.com/screens/368e875d-7f7c-4cf2-8ede-a3e5d222f127) | Progress bar, struck-through done rows, explicit "Hide this widget" | Pattern C |
| [Mercedes-Benz — Incomplete Setup](https://mobbin.com/flows/32f77d37-386d-45ea-955b-ed81eb85f46e) | States the consequence of not finishing, not the mechanics | Pattern C, §7 consequence framing |
| [Cosmos — Finish Setup](https://mobbin.com/flows/b68385e7-5136-4bbc-a972-9aaa9686e3e7) | Floating panel over a working feed — the treatment we reject | Pattern C (rejected form) |
| [PayPal — complete account setup](https://mobbin.com/screens/7c30842b-02a0-474c-a75f-d54f2d5a7b75) | "2 of 5" modal, unfinished on top — modal form rejected | Pattern C (rejected form) |
| [Todoist — Send test reminder](https://mobbin.com/flows/e7c64523-2be1-423a-9b1e-51a5eb73f4cf) | Test button beside the setting, live status text, colocated "not working?" list | Pattern D |
| [PlanetScale — Test webhook](https://mobbin.com/flows/129b1a6a-ed96-4ff6-8683-aded64ba0447) | Result becomes durable timestamped state, not a toast | Pattern D |
| [Brick — Ready to Scan](https://mobbin.com/screens/1f573609-cead-40f1-8dce-027f1d23e1e2) | Armed listening state for an action performed outside the UI | Pattern D armed state |
| [Perplexity — test notification](https://mobbin.com/flows/f7eaed9e-5c03-4f88-b50d-c7f6224700da) | Honest hedge about the layer the app cannot observe | Pattern D honesty |
| [incident.io — Test workflow](https://mobbin.com/flows/35b94543-5300-43e0-9512-3664836b3b52) | States what the test does *not* prove | Pattern D honesty |
| [Twingate — connector detail](https://mobbin.com/screens/d904c7ad-efff-4547-92b2-3db440005f69) | Per-component rows so one degraded part isn't total failure | Pattern E, §5 no tray host |
| [NordVPN — home state](https://mobbin.com/flows/8c85b6ca-c0e8-43cf-8f29-bd59fe5462a9) | One primary action labelled with the next action | Pattern E |
| [Opera — VPN at two altitudes](https://mobbin.com/flows/c542374c-7ef4-4d9d-9144-ef6a49c3d387) | Same state mirrored as a settings row and a detail page | Pattern E |
| [Lyft — background disclosure](https://mobbin.com/flows/ef349bee-a635-4a28-b5b3-3bfe64507616) | Explains *why* something runs in the background, and user control | Pattern E, autostart copy |
| [Discord — keybinds unavailable](https://mobbin.com/screens/d747e905-e01f-488a-9edb-cfefdbd958d5) | Read-only list under one banner naming limitation and remedy | Pattern G, §5 Wayland |
| [Height — shortcut conflict](https://mobbin.com/screens/d0490d15-2923-4e45-b438-cf8b49d53c77) | Names the conflicting owner, offers explicit override | Pattern G |
| [Height — shortcut settings](https://mobbin.com/screens/227ea05e-2f27-4ad6-8bbf-fa8915d896f3) | Binding as an editable row with the current value visible | Pattern G |
| [Zoho CRM — shortcuts](https://mobbin.com/screens/8291b6de-66e5-495e-91a8-c8a2b6525956) | Master toggle plus persistent reset | Pattern G escape hatches |
| [Retool — shortcut syntax](https://mobbin.com/flows/ca6eb0e2-f2ae-464e-97de-261d9c83a5d3) | Text-syntax binding with a persistent explainer | Pattern G fallback |

---

## 11. Risks & Dependencies

- **The silent-no-op constraint is load-bearing and contested.** Pattern F is
  unresolved, and the answer changes the shape of the experience. It should be
  settled before design begins.
- **Mobbin has no desktop corpus.** Every pattern here is adapted from mobile
  or web. Density, pointer/keyboard model, window semantics and the tray have
  no precedent in the evidence, and the shortcut-conflict corpus in particular
  was thin — one screen.
- **Verification cannot be validated in this environment.** The repository's
  own roadmap records that Wayland and X11 have never been exercised on a real
  desktop; only the no-display paths are covered. The armed-test flow is the
  part most dependent on real display-server behaviour and is therefore the
  least verifiable before someone runs it on a real machine.
- **Home screen budget.** A 720px centred column already carries wordmark,
  actions, drop hint, Recent and Keys. The card must not push Recent below the
  fold, which is a real constraint on how much it can say collapsed.
- **Mode semantics.** Any surface that ignores Popup/App/Daemon will feel
  broken — particularly Escape behaviour and the Popup promotion rule.
- **Start-at-login may disagree with reality.** The installer enables a systemd
  unit globally; a user can mask it. A toggle that misreports this is worse
  than none.
- **Platform divergence.** Windows genuinely has a shorter chain. One design
  must express both without implying missing rows are broken rows.
- **The diagnosis already exists and is good.** The main risk is rewording it
  into something less honest. The `--doctor` hints are unusually well-judged and
  should be treated as source copy, not replaced.

---

## 12. Non-goals

- **Not redesigning the preview experience.** Content rendering, zoom, sibling
  navigation and the table/image/hex views are out of scope.
- **Not redesigning the home screen.** Recent, Keys and the primary actions
  stay as they are; this adds one surface and must not restructure the rest.
- **Not solving Linux selection coverage.** No file manager publishes its
  selection; that is an upstream limitation. This work communicates the
  limitation honestly — it does not remove it.
- **Not a general preferences window.** Settings deliberately live in
  `gui.toml`, with a small ⚙ menu; a full preferences dialog that duplicates
  the file would be two places to disagree. Readiness is diagnosis and repair,
  not a settings expansion.
- **Not a Windows shell extension.** Spacebar-in-Explorer needs a shell
  extension and a Windows machine to develop it; the documented AutoHotkey
  workaround stands.
- **Not onboarding for sekio as a whole.** No tour, no feature tour, no
  welcome flow. This is scoped to the hotkey capability chain.
- **Not telemetry.** Nothing here reports capability state anywhere.
- **Not an engineering plan.** No architecture, no component breakdown, no
  visual design system, no tokens.
