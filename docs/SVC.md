# Services over the mesh (`svc`)

A general channel for programs on this computer to offer a service to other arcades and to ask theirs. Arcaded only
carries it; it knows nothing about what a service does. The first user will be the AI marketplace's helper
(arcade-models), moving the marketplace out of arcaded. Code: `crates/arcaded/src/svcmesh.rs`.

## A helper (the service's side)

Open the API socket and send `{"cmd": "svc.attach", "service": "<name>"}`. The name is 1-32 characters of a-z, 0-9
and `-`. The connection then stays open:

- **arcaded writes** `{"ok": true, "result": {service, node, pay_to, tag}}` first (this arcade's node id, its ARCADE
    address and @tag), then:
  - `{"in": {id, from, who, tag, body}}` for each request. `from` is the asking arcade's node id. `who` is the
    asker's ARCADE address, proved by a grant its wallet signed for this service at this arcade (null when it has
    no ARCADE wallet), and `tag` is that address's @tag.
  - `{"cancel": {id, why}}` when the asker's link goes down.
  - `{"offered": bool}` or `{"error": ...}` for the helper's own lines.
- **the helper writes:**
  - `{"out": {id, ev?, blob?, end?, err?}}`, any number per request. `blob` is base64 bytes, split into 48 KiB
    frames on the wire. The last answer carries `end: true` or `err`.
  - `{"offer": {...}}`: its offer, at most 16 KiB. This arcade signs it with its node key and spreads it over the
    mesh, again every 10 minutes while the helper stays attached. `{"offer": null}` withdraws it.

There is one helper per service name. When the helper's connection closes, its open requests get "the service
stopped", and its offer is withdrawn at once.

## Commands

| command | perm | what |
|---|---|---|
| `svc.offers {service?}` | Read | every arcade's offer: service, arcade, tag, public name, pay_to, linked, offer |
| `svc.services` | Read | the helpers attached here |
| `svc.ask {to, service, body, timeout?, stream?}` | Local | ask a service at an arcade (this one too). Returns `{last, answers: [{ev, blob?}]}`. With `"stream": true` on the socket, one line per answer as it comes, then `{ok, result: {last}}`. |
| `svc.pay {to, amount, memo}` | Wallet | ARCADE to an address, with a memo of up to 80 bytes |
| `svc.paid {txid, memo?}` | Read | whether it is in a block (`in_block`), what it `paid` this arcade's address, and whether `memo_found` |

**Reaching an arcade:** an ask reaches it by its public name, a relay, or a UDP introduction, waiting up to 25 s.
It may take up to 20 minutes, with at most 3 minutes between answers.

## Wire

Frames are `{"t": "sv", ...}`; older arcades ignore the kind.

- `offer {s, o, at, tag, name, pay_to, body, sig}`, flooded and kept 30 minutes.
- `q {id, seq, end}` plus a piece of the request (zlib JSON `{service, body, grant}`, 48 KiB pieces), over crvia.
- `a {id, seq, end, ev?, err?}` plus a piece of answer bytes.

A grant is a Core-style signmessage, by the asker's ARCADE address, of
`arcade-svc grant <service> <arcade> <address> <expires>`. It lasts a year at most.

## Spending allowances (2026-11-21)

An app may be given an allowance for a service: up to N ARCADE a day that it spends without a card, so it can keep
its own credit topped up while it runs unattended. Any app, any service.

- `arcade.svc.askAllowance(service, perDay, reason?)` asks the person on a card ("<app> asks to spend up to N ARCADE
  a day on AI models (reason)"); `arcade.svc.allowance(service)` reads `{per_day, spent_today, left, payments}`.
- A payment the app asks for with `arcade.svc.local(service, {what: "buy", amount, ...})` that fits what is left today
  (UTC) is counted at once (`apps.allowance_use`) and goes without a card; one beyond it asks as before. A payment that
  fails is given back.
- With an allowance for a service, the app's other uses of it (asks, chats, the helper's reads) need no
  once-a-window card either. A new key still asks.
- The person sees and changes it in the app's details (Apps → Details → Allowances; 0 takes it away), or with
  `apps.perms {id, set: {allowances: {<service>: N}}}`.
