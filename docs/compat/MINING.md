# Pepecoin testnet: mining without Core (from Pepecoin Core 4fb5a0cd93, via the Arcade v1.0 session, 2026-11-07)

Params are height-specific (GetConsensus(nHeight)):
| heights | fPowAllowMinDifficultyBlocks | Digishield | T/spacing | maturity | legacy blocks |
|---|---|---|---|---|---|
| 0-999 | true | true | 60/60 | 30 | yes |
| 1000-1249 | **false** | true | 60/60 | 240 | yes |
| 1250-41999 | true | true | 60/60 | 240 | yes |
| 42000+ | true | true | 60/60 | 240 | **no** (AuxPoW allowed) |
powLimit compact 0x1e0fffff. DifficultyAdjustmentInterval = 1.

## Next work (pow.cpp + pepecoin.cpp)
```
GetNextWorkRequired(last, block):
  if last == NULL: return powLimit
  if AllowDigishieldMinDifficultyForBlock(last, block): return powLimit
      # = fPowAllowMinDifficultyBlocks(h) && last.height >= 1250 && block.time > last.time + 120
  interval = 1  (from last.height >= 1000; also 1 before)
  # every block retargets; the Bitcoin-testnet walk-back branch is dead code from 1000
  first = ancestor(last.height - 1)            # blockstogoback = 1 (0 for the very first retarget)
  return CalculatePepecoinNextWorkRequired(last, first.time)

CalculatePepecoinNextWorkRequired(last, first_time):   # Digishield, T = 60
  actual = last.time - first_time
  mod = 60 + (actual - 60) / 8                  # C++ int64 division, truncates toward zero
  clamp mod to [45, 90]
  new = target(last.bits) * mod / 60; cap at powLimit; return compact(new)
```
After a min-difficulty block the next target derives from that block's own bits, so the testnet tip sits at 1e0fffff.

## Subsidy (fSimplifiedRewards on testnet: no random era)
halvings = h / 100000; h < 600000: (500000 COIN) >> halvings; else 10000 COIN.
Coinbase out <= subsidy + fees ("bad-cb-amount"); paying less is allowed.

## Block rules
1. h >= 42000: refuse legacy (version 1, or 2 with chain id 0). h < 42000: refuse AuxPoW.
2. bits == GetNextWorkRequired.
3. time > median of last 11; time <= adjusted now + 2 h.
4. base version >= 4 from 1000. **Use 0x003f0004** (chain id 0x3f, no AuxPoW flag), as every live block does.
5. BIP34 from 1000: coinbase scriptSig starts with minimal CScriptNum push of the height (e.g. 03 b3 d3 17).
6. No SegWit (no commitment, no witness). CSV failed: lock-time cutoff = block time.
7. Size 1,000,000 base / weight 4,000,000; sigops cost 80,000; coinbase scriptSig 2-100 bytes; no mutated merkle.
8. No required dev/fund outputs. Live tip coinbase: v1, scriptSig = height push only, one P2PKH output, locktime 0.

## How the old arcade built blocks (blockminer.py / stratum.py)
- Coinbase scriptSig = push(height minimal LE) | 08 extranonce u64 LE | 10 "/dogecoinarcade/" (16 bytes).
  Input: null prevout, ffffffff sequence. tx version 1, locktime 0.
- Outputs: miner (reward - share); optional 10% share to the borrowed-Core arcade (>= 546); no dev output;
  no witness commitment on Pepecoin testnet.
- Header: template version/bits/curtime (V1.0 computes these itself: Digishield, min-diff after 120 s from 1250,
  0x003f0004, MTP < time <= now+2h). Merkle duplicates last. Nonce sequential; scrypt(h80,h80,1024,1,1) <= target.
- Stratum: VERSION_MASK 0x1FFFE000, extranonce1 4 bytes BE + extranonce2 4 bytes, DIFF1 0xFFFF<<224, start diff
  1024; heights 1..16 as OP_N in stratum coinbases.
