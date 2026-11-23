# Content screening

An arcade shows what anyone puts on the chains it reads: inscriptions, ord content, pictures and text. Some of it can be
sexual, violent or illegal. Screening checks each picture and text with a model before the arcade's public routes serve
it, and covers or refuses what isn't safe. A new arcade is protected from its first start, with nothing to set up.

## Where it applies

`/content/<id>` (arcade-ledger inscriptions, ord inscriptions, a site's pictures, another chain's content asked by an
app), `/ord/<chain>/<id>` (the Explorer's previews) and `/app/<chain>/<id>` when it is a picture or text.

- Checked: `image/*` (pictures) and `text/*` (text; `image/svg+xml` as text). Not checked: HTML pages and scripts (apps
  are code), audio, video, fonts, token operations (`{"p": ..., "op": ...}` JSON, thousands of them) and text with
  nothing to read. ARCADE blobs (apps' parts, films) aren't screened yet.
- Verdicts: `ok` is shown; `sensitive` (nudity, sex, gore, threats) gets a cover, opened with `?show=1`; `illegal` is
  never served (451). Each verdict is kept per content (its sha256) with the prompt it answered, so each picture is
  checked once.
- Content not checked yet: the request waits up to 2.5 s for its check, then gets a "checking" cover (never cached).
- When no screener works: content not checked yet is covered for other computers and shown on this one (`unchecked:
  show_here`, the default), or covered everywhere (`unchecked: cover`).

## Who answers

`screening.set {mode}`:

- **borrow** (the default): a screening model another arcade sells on the Models marketplace (service `models`, a chat
  model whose id contains `screen`), asked straight from arcaded over the mesh (`svc.ask`, `what: chat`) and paid from
  this wallet's credit at that seller. No Models app or helper is needed on the buying side.
  - The cheapest offer (an arcade's own is free) that passes the built-in test is used.
  - Offers are looked at every 30 minutes; the one in use is tested again every 6 hours; three failed checks in a row,
    the offer going away or down, or a failed test, and the next cheapest that passes takes over.
  - At most 3 offers are tested per look (each test is 5 checks).
  - `screening.topup {amount}` pays credit to the seller (memo `arcade-models <address>`); `day_cap` caps the spending
    a day (1 ARCADE unless set). `pin: "model@<arcade id>"` borrows from one only.
- **own**: the operator's own OpenAI-compatible server with a vision model (`url`, `model`, `key`): llama.cpp with
  `--mmproj`, vLLM, Ollama, LM Studio. Encouraged: the verdicts come from the same model every day, and nothing leaves
  the computer. It runs the same test when set and every 6 hours.
- **off**: nothing is checked (the window warns first).

Every screener is asked the same question (`screening::PROMPT`, the Python arcade's words) and answers
`{"verdict": "ok"|"sensitive"|"illegal", "reason"}`. A seller of a screening model should answer with that JSON.

## The built-in test

Five harmless cases with known answers: a birthday note (ok), ordinary swearing (ok), an adult-content line
(sensitive), a carding offer (illegal), and a drawn picture (ok). All five must be right. Nothing illegal to hold is
ever used for testing.

## In the window

- **Home** shows a Content screening notice until the operator chooses (Borrow screening, Use my own model, Turn off),
  and again whenever screening stops working (with a top-up button when the seller needs credit).
- **Options → Content screening**: the warning, the mode, what is in use and what a check costs, the credit there,
  today's spending and cap, the offers with their test results, the own model's settings, and the unchecked policy.

## Not a legal shield

A model is a first filter. A public arcade should also match known child-abuse material by hash (hash lists, a
CDN's scanning tool) in front, and follow its country's reporting duties.
