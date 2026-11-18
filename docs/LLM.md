# Shared models (LLM inference for ARCADE)

Status: **MOVED OUT OF ARCADED** (2026-11-16, Martin: "we're supposed to have the AI marketplace as its own app only
using the arcade mesh"). It is now the Models app (inscribed by @strawberry on arctest) and its helper program
`arcade-models` (`@strawberry/arcade-models` on ARCADE), on arcaded's general service channel (docs/SVC.md). This file
is kept as the design history: the pricing, paying and checking rules below are what the helper does. Was LIVE in
v0.1.98 (chat, PR #239) and v0.1.100 (speech, §7) inside arcaded. Martin 2026-11-16: "add open AI API to arcade V1 so that someone with a local model can
share their inference to the network in exchange. They should earn arcade coin." Decided: **buyers pay sellers**
(a market, as the original arcade's LLM service); no minted reward, no change to the ARCADE coin's rules.

## 1. What people get

- **Seller** (anyone running vLLM, llama.cpp, Ollama, LM Studio, ... on their own computer): Models tab -> *Share
  your model*: the server's URL (and key if it has one), the models found there ticked, a price in ARCADE per million
  tokens in and out for each model (blank or 0 = free), how many requests at once, the longest reply. Their arcade
  announces the offer to the mesh, serves requests that arrive over mesh links, and is paid in ARCADE to its own
  @tag's ARCADE address.
- **Buyer**: a *Models* tab lists every offer on the mesh (model, seller @tag, price, busy/free, speed and track
  record). Buy credit with one press, then use it:
  - from the desktop/web UI (a plain chat box per model, for trying it);
  - from app pages through `arcade.llm` (/r/llm.js, the original API, APP_API.md B4);
  - from any OpenAI program (coding tools, chat UIs, scripts): base URL `http://127.0.0.1:8430/openai/v1`, key
    `sk-arcade-...` from the Models tab. `/v1` stays dproto's, so the OpenAI door is `/openai/v1`.

## 2. Moving the request

- Requests go seller-ward over a **mesh link** with the request/answer channel already used by serving checks
  (`crvia`: `Via::open(peer)`, several `a` answers per id = streaming). A buyer with no link to the seller opens one
  the way arcserve does (dial from the offer), else through a relay arcade (`relaysvc`, end to end sealed). New
  frame kind **`lm`** (older arcades ignore it):
  - `offer {o}`: on link start and on change; the offer is also flooded once signed (`canon::sign_json`, like
    shares) so arcades without a link see it. Offers older than 3 h drop.
  - `load {busy, at_once, waiting}`: every 15 s on links that asked.
  - `q {id, what, seq, end}` + a piece of the request (zlib, 48 KiB pieces): `what` = chat | balance | bill.
  - `a {id, seq, end, err?}` + one event: each OpenAI stream chunk as it comes, `{"arcade": {usage, cost, left}}`
    last.
- The seller's arcade calls its own model server (only ever the URL its operator set; never a URL from a request),
  passes a fixed list of OpenAI fields (messages, max_tokens, temperature, top_p, stop, tools, tool_choice,
  response_format, stream, seed, ...), caps max_tokens to what the buyer's credit covers and to the seller's longest
  reply, and reads usage from the server's own answer (`stream_options.include_usage`).

## 3. Paying (same pattern as the relay fee, relaysvc.rs)

- Credit is **prepaid, per buyer per seller** (one credit for all of that seller's models), and kept by the seller's arcade. The most a buyer can lose
  to a bad seller is the credit they put there; the seller never serves on trust.
- **Buyer identity** = the buyer account's ARCADE address. The account signs a grant (Core-style `signmessage`)
  naming the seller's node id, its address and an expiry; the buyer's arcade carries it on each request. The seller
  serves an address only with a grant that address signed for that seller, so an arcade can't spend someone
  else's credit.
- **Top-up**: the buyer's arcade pays the seller's `pay_to` with the memo `arcade-llm <grant hash>`
  (`arcledger::pay_with_memo`) and tells the seller the txid. The seller credits it **once it is in a block** (one
  ARCADE confirmation, ~2.5 min; never from the pool, a waiting tx can be replaced), once per txid, only to the
  address the memo names.
- **Charging**: tokens in x price in + tokens out x price out, from the seller's model server's count; reserved
  before the request (prompt guess + capped reply), settled after. A reply that errors costs nothing. Kept in
  millionths of an ARCADE.
- **Auto top-up** (buyer's choice, off by default): when a credit falls below a floor, the buyer's arcade buys the
  set amount again, within a spending cap per day the buyer sets. This makes the OpenAI door work unattended.
- **Checking the seller**: the buyer's arcade counts the reply's tokens itself (rough) and keeps each seller's
  record: requests, failures, first-token time, tokens/s, and billed vs counted tokens. The Models tab shows it, so a
  seller who over-counts is visible. Unused credit can be asked back (`refund`): the seller's arcade pays it back
  automatically unless its operator turned refunds off (shown on the offer).
- The operator's own accounts use their own model free.

## 4. Pieces in arcaded

| piece | file | notes |
|---|---|---|
| settings + model server client | `llmsvc.rs` | META `llm` (url, key, models, prices, at_once, max_out, refunds); `/models` scan; streaming chat |
| books | `llm-books/` JSON (atomic) or META rows | credits, used txids, usage log, keys (hash only), grants |
| mesh | `llmshare.rs` | `lm` frames, offers, crvia ask/answer, load |
| payments | in `llmsvc.rs` | memo, credit after a block, refunds, auto top-up |
| OpenAI door | `http.rs` `/openai/v1/models`, `/openai/v1/chat/completions` (SSE like `realtime_door`) | `sk-arcade-` keys: local or LAN with a key, body cap 4 MiB |
| commands | `commands.rs` / `api.rs` | `llm.status`, `llm.set` (Admin), `llm.offers`, `llm.chat`, `llm.topup`, `llm.balance`, `llm.keys`, `llm.key_new`, `llm.key_drop`, `llm.refund`, `llm.usage` |
| page API | `shims/llm.js`, appwindow.js `llm` door | the original `arcade.llm` (offers/chat), account only, per-page yes |
| UI | desktop dist: *Models* tab, Options -> *Share a model* | trying a model, credit, keys + "how to connect" text |

Pictures (the original's ComfyUI image sharing) come after text works, as a second step.

As built: `llmsvc.rs` holds all of it (settings in META `llm`, seller books in `<home>/llm-books.json`, the buyer's
seller records in `<home>/llm-sellers.json`, keys in META `llm_key:<id>`, grants in META `llm_grant:<seller>`,
auto top-up in META `llm_auto`); `lm` frames go through crvia (meshsvc dispatch); offers are flooded through
`mesh_outbox` every 10 min and on each settings change; the OpenAI door is in http.rs (`openai_door`), this computer
only (as `/api/v1`), with an `sk-arcade-` key; app pages get `arcade.llm` through the appwindow `llm` door (one yes per
window). A buyer with no link to a seller tries every way there at once and waits up to 25 s: the seller's public
name from its offer (dialed), a relay that lists the seller (asked for a relayed link whichever id is lower,
`relaysvc::want`), and a UDP introduction through its links (`udpsvc::want`). Not reaching a seller is the mesh's
fault, not the seller's: it isn't counted in the seller's record (ARCADE #19). A mesh link that hears nothing for
120 s ends (pings go both ways every 15 s), so a relayed link whose far end is gone no longer stays "linked" with
nothing going through.

## 5. Tests before it goes live

1. Unit: price math, grant/top-up checks, memo parsing, reserve/settle, field filtering.
2. Local net (two arcades in `unshare -Urn`, a fake OpenAI server): offer seen, chat streamed, charged right,
   top-up credited only after a block, a grant for another seller refused, error = free, refund.
   Done: scratchpad llmnet.py, 22/22 PASS (offer seen, no-credit refusal, top-up credited after a block, 0.009
   ARCADE charged exactly, server error free, door whole + streamed, own use free, refund paid).
3. Live on arctest: GX10 sells its vLLM (`qwen3.8-flash-next` on :18300) at a small price; a buyer on another arcade
   buys credit and uses it from the Models tab and from an OpenAI client pointed at `/openai/v1`.

## 6. Open

- Whether credits can also be paid in other coins (pep, doge ...) later; this plan is ARCADE only.
- Pictures (step 2).

## 7. Speech (voices)

Martin 2026-11-16: "build the sharing of the conversational speech model" (the CSM voice server, `csm-voice`, an
OpenAI `/v1/audio/speech` server whose `/v1/models` lists `csm-1b` with its `voices`).

- **Several model servers.** A seller sets `servers: [{url, key}]` (the old single `url` + `key` is read as the first),
  so one arcade can share its chat server and its voice server together. A scan of every server records where each
  model lives and what it is (`info: {model: {server, kind, voices}}`); requests go only to that server. Each server
  has its own slots, so a voice and a chat don't wait for each other.
- **A speech model** is one whose list entry has `voices`, or whose name says tts/csm/speech/kokoro/orpheus. Its offer
  entry carries `kind: "speech"` and the voices; older arcades list it as a chat model and get a clear refusal.
- **Price**: `in` = ARCADE per million **characters** of input (OpenAI prices speech the same way); `out` is unused.
  The cost is known before: it is reserved in full, refused when the credit doesn't cover it, and charged only when
  audio came (an error costs nothing). Input 1 to 4096 characters. The buyer counts the characters itself, so its
  record shows any over-billing exactly.
- **Wire**: `q {what: "speech", body: {model, input, voice?, response_format?, speed?, instructions?}}` (nothing else
  is passed to the voice server). Answers carry the audio in the frames' blobs (48 KiB pieces, the first with
  `ev {type}`), keepalives `ev {wait: true}` every 20 s while the voice server is still making audio (CSM makes a
  whole wav before sending), then `{"arcade": {usage: {characters, bytes}, cost, left, ...}}`. A speech request may
  take 20 minutes; the seller waits while the link's queue is full rather than dropping audio.
- **Buyer**: `POST /openai/v1/audio/speech` streams the audio as it arrives (`Content-Type` from the seller's server;
  402 when the credit is short, 502 on a seller error, OpenAI-style). `GET /openai/v1/models` marks voices with
  `kind: "speech"` and `voices`. `llm.speak {model|seller, input, voice?, response_format?}` returns base64 for the
  Models tab, whose try-it box becomes *Speak* with a voice picker and a player when a voice is picked. App pages'
  `arcade.llm` lists chat models only.
- Tested: scratchpad `llmspeech.py` on a closed two-arcade net with a fake voice server (byte-exact audio, 0.002
  ARCADE for 40 characters at 50/M, failure free, door 5.8 MB wav, 45 s silent start, own use free, chat beside it).
