# ARCADE as the home chain

Martin, 2026-11-15: "make arcade test and later arcade main the home chain and make pepecoin test net another shared node."
This carries out the decision in ARCADECHAIN.md §15.1 ("Yes, arcade should become the home chain", 2026-11-14).

## Goal

- **ARCADE testnet becomes the home chain.** @tags, profiles, follows, posts, messages, linked wallets, the operator's
  handle proof, sign-in and the App Store's default all live there.
- **Pepecoin testnet becomes a shared coin, like Dogecoin.** It is an optional install. Most arcades borrow it
  (coreshare) from the few that keep it whole.
- **At mainnet launch (2027-01-01) every arcade moves to ARCADE main.** ARCADE main is home from its block 0, which
  carries the Pepecoin mainnet ledger (§15 step 19). Pepecoin mainnet is shared, and ARCADE testnet becomes an
  optional shared test coin.

## What "home" is in the code today

The home chain is `node.params`, set once in `main.rs` to `PEPECOIN_TEST`. It has 237 uses in 60 files, and they do
three different jobs:

| Role | Where | After the switch |
|---|---|---|
| **The ledger:** tags, claims, profiles, follows, feed, messages, links, handle proofs, account tags, App Store and explorer defaults, the page chain | claims, msgsvc, follows, feedsvc, links, handlegate, accountsvc, appsvc, http, explorer, sitesvc | **ARCADE** |
| **Coin machinery:** P2P sync, the testnet miner, mempool, resends, the test-PEP welcome faucet, Core copies, lending | chainsvc, miner, mempoolshare, resend, faucet, coreboot, v1import, coreshare | **Pepecoin**, as one coin among many |
| **The original arcades:** the home tip in `nets`, the `wl` welcome | meshsvc | **Pepecoin**, kept for them; ARCADE has its own `ac` frames |

So the change is **a split, not a swap**:
- `node.ledger_home()` is the chain the ledger is written to and read from first.
- `node.params` stays a coin chain, and in time only where a coin chain is meant.

## Decisions

| Question | Decision |
|---|---|
| @tags claimed on Pepecoin testnet after the snapshot (block 1,569,080) | **Claimed again on ARCADE** (Martin, 2026-11-15). They are test coins, and the ARCADE faucet covers the fee. Reads find them on Pepecoin testnet until phase 3, not after. |
| The mainnet move | **Every arcade moves to ARCADE main** at launch (Martin, 2026-11-15). The launch release switches every arcade's home to ARCADE main, and ARCADE testnet becomes an optional shared test coin. |
| Tokens and the market | **They move to ARCADE in phase 3** with the rest of the ledger, priced in ARCADE (Martin, 2026-11-16). The Pepecoin testnet book stays readable as history. |
| The public ARCADE arcade (phase 0) | **Ashburn** (Martin, 2026-11-15). It is already a seed and the TURN relay. Check its disk first: ARCADE testnet is about 1.7 GB today. |

## Phases

Each phase ships as a release, and each has a pass test like §15.2's.

### 0. An ARCADE network that is always up and reachable

- **Ashburn installs ARCADE testnet** (`chains.add arctest`), keeps it whole and lends it. **Done 2026-11-16:**
  installed on v0.1.84, synced to block 451 within minutes, about 890 MB of RAM (1.3 GB still free) and 129 MB of disk.
  - **Today:** only home-network arcades and one other address hold the chain. A firewalled arcade sat at block 0
    because none of the arcades it could reach ran the chain (2026-11-16).
  - **Seeds:** Ashburn is in SEEDS already. Once it runs the chain, the seeds give every arcade an ARCADE source.
- **ARCADE testnet is not reset from here on.** Its ledger becomes everyone's identity. A change of rules goes out as
  a rules version, not as a new block 0.
- **Pass:** a fresh arcade on an outside network syncs ARCADE through the seeds alone and stays on the GX10's tip.

### 1. The split, with no change in behavior

- **`Node::ledger_home()`** returns the chain the ledger writes to. Until phase 3 it returns `node.params`.
- **Every use of `node.params` is sorted** into ledger, coin or original-arcade:
  - **Ledger uses** go through `ledger_home()`.
  - **Coin uses** take the chain they mean (`chains::find`, or the coin's own params).
- **Pass:** the whole test suite passes, and the three arcades behave as before.
- **Done (2026-11-16):** `Node::ledger_home()` is in, with 66 uses routed through it:
  - **Whole modules:** claims, follows, feedsvc, links, handlegate, accountsvc and msgsvc.
  - **Mixed modules, ledger reads only:** tag lookups in explorer and realtime, the App Store's feed acts and
    creator tags, `msg.stats`, and arcledger's ledger chains.
- **Left on `params` (184 uses):** these mean Pepecoin testnet as a coin chain, or are defaults phase 3 decides:
  - **Coin machinery:** sync, the miner, the mempool, resends, the welcome faucet, Core copies, lending, its own
    ledger index (`ledger.rs`), and the "from home" profile (the old Pepecoin profile shown beside an ARCADE one).
  - **Market, trades and swaps** (marketsvc, tradesvc, swaps): their tokens and order book are Pepecoin testnet's
    ledger today. **They move to ARCADE in phase 3** (Martin, 2026-11-16: tokens and the market move with the rest).
    The snapshot already carried 121 tokens and 462 balances there, and the Pepecoin testnet book stays readable as
    history.
  - **Defaults:** the explorer's and site API's chain when none is named (explorer, sitesvc, http, pagechain), and
    the wallet's send chain (walletsvc).

### 2. Reads ask ARCADE first

- **Lookups ask ARCADE first, then Pepecoin testnet.** This covers tags, profiles, follows, feed, messages, links and
  the inbox. The App Store and pagechain already read across chains (`ledger_chains`).
- **The handle proof has an ARCADE form.** It is signed with the tag's key on ARCADE (no Pepecoin message magic) and
  checked against the ARCADE ledger. Peers take either form.
- **Pass:** a tag held only on Pepecoin testnet and one held only on ARCADE both resolve on every arcade, and each
  arcade's handle proof is accepted.
- **Built (2026-11-16):** `crate::hometag` answers who holds a tag and which tag a key holds, ARCADE first and then
  the ledger home, by key hash: one key is one wallet on both chains.
  - **Where it's used:** handle proofs (checked on ARCADE as well, and made by an operator whose tag is on ARCADE
    only), account and member tags, explorer and follow rows, @tag message recipients (`msg.who`), and home claims:
    a tag held on ARCADE is taken.
  - **Why the claim check matters:** v0.1.84 would let a tag held on ARCADE, such as @claude_bugs, be claimed again
    on Pepecoin testnet by someone else.
  - **Checked** on a copy of the GX10's data (closed network):
    - **@claude_bugs** (ARCADE only): "taken on ARCADE testnet".
    - **@strawberry:** still its Pepecoin owner.
    - **A free name:** still free.
    - **`msg.who @claude_bugs`:** found; no messaging key is published yet.
  - **The handle proof needs no new format:** it is signed by the home key, and the check reads ARCADE by that key's
    hash.
  - **Left for phase 3:** the account vault lookup (`find_vault`), whose vaults are keyed by the home address.

### 3. Writes go to ARCADE: the switch

**Step 1 built (2026-11-16): new @handles on ARCADE.**
- **Where claims go:** with no chain named, `tag.claim` and `tag.check` use ARCADE when it runs here
  (`claims::claim_home`). Asking for Pepecoin testnet by name still works.
- **Funding:** a claim waiting for coins asks the ARCADE faucet (#175) for its address.
- **The start screen:** `start.status` follows the claim's chain (ARCADE balance, faucet state and heights, a `chain`
  field), so the screen says "Waiting for 0.01 ARCADE".
- **Restored wallets** get their @handle back from either chain.
- **Explorer:** the tag page and search find a tag held on ARCADE.
- **Pass** (scratchpad `lnet/claimnet.py`, two arcades on a closed local chain): a fresh wallet claims `@claimtest`
  with no chain named; the other arcade's faucet pays it; the claim confirms on ARCADE; the other arcade's explorer
  shows it; and on Pepecoin testnet it reads "taken on ARCADE testnet".

- **`ledger_home()` returns ARCADE.**
  - **What moves:** claims, profiles, follows, posts, messages, links and the handle proof are written there.
  - **Keys:** they come from the wallet's existing keys. The BIP44 coin type is the home chain's (§13 q6), so an
    @name's ARCADE address is its existing key hash.
- **ARCADE is borrowed until it is installed** (Martin, 2026-11-18: "all chains are only borrowed until someone
  installs them, including arcade"; this replaced "installed by default"). It is home either way: installed,
  borrowed, or installed and still catching up, when a borrowed copy stands in from 30 blocks behind its lenders
  until its own copy has caught up (`arcsvc::source`: `Own` or `Borrowed`). Balances, sends, @tag claims (checked
  with an arcade that keeps the chain), posts and messages work at once. It can be removed like any chain, and is
  borrowed again. The A:// ledger (`arcledger`: A:// profiles, follows, repos) and blob publishing still need it
  installed.
- **New accounts get their first coins from the ARCADE faucet (#175),** not test PEP.
- **Reads stop falling back to Pepecoin testnet** for tags (the decision above). Profiles and posts made there stay
  readable as history.
- **Pass:**
  - A new user signs in, gets ARCADE, claims a tag, publishes a profile and posts. Every arcade sees it, and
    Pepecoin testnet is not touched.
  - An arcade on the release before keeps reading.

### 4. Pepecoin testnet is shared

- **New arcades don't install Pepecoin testnet.** They borrow it (on by default) from the arcades that keep it whole:
  the GX10, the JB20B, and anyone else.
- **Existing installs keep their copy,** and can remove it in Chains & Coins like any coin.
- **The testnet miner and the welcome faucet run only where Pepecoin testnet is installed whole.**
- **PEP keeps working through borrowing:** sends, inscriptions, the Doginals and ord views, and apps inscribed on
  Pepecoin testnet.
- **Pass:** a fresh arcade with no Pepecoin testnet sends test PEP, opens an app inscribed on Pepecoin testnet, and
  shows the right PEP balance.

### 5. The website and the original arcades

- **app.dogecoinarcade.com** (dogecoinarcade-app-v1) moves accounts, @name claims and the faucet to ARCADE. Its
  ARCTEST params and wallet tile are already there (PR #17, #18).
- **The original arcades keep their Pepecoin view:** their feed shows only what is posted on Pepecoin.
- **Pass:** a tag a website user signs up with shows on every V1 arcade, and their profile opens there.

### 6. Mainnet (2027-01-01)

- **The launch release moves every arcade to ARCADE main.**
  - **Home:** ARCADE main, from block 0 with the Pepecoin mainnet ledger snapshot (§15 step 19).
  - **Shared:** Pepecoin mainnet is borrowed like any coin, and ARCADE testnet becomes an optional shared test coin.
  - **Lost:** tags and profiles made only on ARCADE testnet stay there. Mainnet starts from Pepecoin mainnet's ledger.
- **Pass:** the mid-December testnet rehearsal (§15 step 20) runs phases 3 and 4 end to end on a local copy of both
  ARCADE nets.

## Risks

- **The ledger is only as good as ARCADE.** A split or a reset now costs people their identity. Phase 0 comes first,
  and a split must heal (v0.1.77).
- **A mix of versions:** an arcade on an older release keeps writing to Pepecoin testnet. Phase 2's reads and both
  proof forms make that a slow degradation, not a break. Phase 3 should still go out as a release everyone updates
  to.
- **The snapshot rows:** they have to stay with every copy of the chain. They do since v0.1.84 (§15.3 step 19).
