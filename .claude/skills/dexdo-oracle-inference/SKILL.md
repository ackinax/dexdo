---
name: dexdo-oracle-inference
description: Deploy a DEX.DO oracle on Acki Nacki Shellnet and bind it to an inference market — register a numeric RANGE event whose outcome is decided by an InferenceOrderBook's weekly-median reference price (spec §6.2). This is how an oracle "supports" inference markets: a prediction market resolves from a model's on-chain price. All on-chain via the dodex-sdk libraries (deploy_oracle + the dexdo CLI) — NO REST API. Load when the user wants to deploy an oracle for inference, make a prediction market resolve from an inference order book, add/resolve a range event, or wire an oracle to a model's weekly-median price.
---

# DEX.DO — Oracle for Inference Markets (Shellnet)

Deploys an **Oracle** (+ its `OracleEventList`) and binds a **range event** to an
**`InferenceOrderBook`**, so a prediction market's outcome is decided by that
model's **weekly-median reference price** (spec §6.2 "range events").

**Everything here is on-chain via the dodex-sdk libraries — no REST API.** The
tools are the `deploy_oracle` and `dexdo` SDK binaries; the REST backend has no
oracle endpoints and is never touched.

## What an oracle has to do with inference

Oracles belong to the **prediction-market** side (`RootOracle → Oracle →
OracleEventList`). An inference market (`InferenceOrderBook` + `TokenContract`)
does **not** use an oracle to run — it is a CLOB settled by probe-tick escrow.

The link is one specific thing: a prediction market can be a **numeric range
event** whose outcome is read from an inference model's price. The
`OracleEventList` stores `bounds[]` + the bound `InferenceOrderBook` address
(`ob`); at resolution it calls `InferenceOrderBook.requestWeeklyMedian(...)`, the
book pushes its weekly VWAP median back via `onWeeklyMedian`, and the median is
bucketed into an outcome. So the **inference market is the price source**, the
oracle is the consumer. "Deploying an oracle for inference" = deploy the oracle,
then `add-range-event` bound to the book.

```
deploy_oracle            → Oracle + OracleEventList@0 (+ oracle keypair)
dexdo add-range-event    → range event on the list, bound to ob=<InferenceOrderBook>, bounds[], outcomes[]
   (a PrivateNote deploys a PMP referencing this event; the oracle confirmEvents it — prediction-market flow)
dexdo resolve-range      → after the deadline: pull the book's weekly median → onWeeklyMedian buckets it → PMP resolves
```

## Scope

- **Network:** Shellnet (public testnet — `deploy_oracle` tops `RootOracle` up
  from the public giver). Mainnet is out of scope (no public giver).
- **In scope:** deploy an Oracle; register a range event bound to an
  InferenceOrderBook; resolve it after the deadline.
- **Out of scope:** deploying the inference market itself (the
  `InferenceOrderBook` is an INPUT — deploy it from a note via
  `deployInferenceOrderBook`, see the inference deposit/trading flows); deploying
  the PMP/prediction market and staking/trading on it (that is the note owner's
  side — `dexdo-trading`).

## Inputs

- **The InferenceOrderBook address** (`0:<hex>`) — the model's on-chain order
  book whose weekly median resolves the event. Provided by the user (or found via
  the inference market listing). This is the `--ob` argument.
- **Range bounds + outcome labels** — the price buckets. `N` strictly-increasing
  upper bounds (raw `uint256` price units, the same units the book quotes) yield
  `N+1` outcomes; you supply `N+1` dense labels `0..N`.
- **Deadline** — a unix timestamp; must be at least `MIN_RESULT_GAP` in the
  future (it doubles as the PMP result-start).

## Setup

Uses the `deploy_oracle` and `dexdo` SDK binaries. Build once (release):

```sh
export WORKSPACE="${WORKSPACE:-$HOME/dexdo-workspace}"
cd "$WORKSPACE/dexdo/sdk"
cargo build --release --bin deploy_oracle --bin dexdo
DEPLOY_ORACLE="$WORKSPACE/dexdo/sdk/target/release/deploy_oracle"
DEXDO="$WORKSPACE/dexdo/sdk/target/release/dexdo"
ENDPOINT="shellnet.ackinacki.org"
```

If the repo/workspace is not set up yet, do the clone + toolchain setup from
`dexdo-deposit-shellnet` §Setup first (same `$WORKSPACE/dexdo`). `jq` is used
below to parse the deploy output.

## Step 1 — Deploy the oracle

`deploy_oracle` generates a fresh oracle keypair, tops up `RootOracle` from the
giver, calls `RootOracle.deployOracle`, and waits for the `Oracle` and its
`OracleEventList@0` to go Active. It prints a JSON object on stdout:

```sh
mkdir -p "$WORKSPACE/oracle"
"$DEPLOY_ORACLE" --endpoint "$ENDPOINT" --name-prefix dodex-oracle \
  > "$WORKSPACE/oracle/oracle.json"
cat "$WORKSPACE/oracle/oracle.json"
# → { "address": "0:<oracle>", "pubkey_hex": "...", "secret_hex": "...<SECRET>",
#     "event_list_address": "0:<OracleEventList@0>" }

# Pull the pieces the next steps need.
EVENT_LIST=$(jq -r .event_list_address "$WORKSPACE/oracle/oracle.json")
ORACLE_PUB=$(jq -r .pubkey_hex        "$WORKSPACE/oracle/oracle.json")
ORACLE_SEC=$(jq -r .secret_hex        "$WORKSPACE/oracle/oracle.json")
chmod 600 "$WORKSPACE/oracle/oracle.json"
echo "oracle EventList = $EVENT_LIST"
```

> **`oracle.json` holds the oracle SECRET key** (`secret_hex`) — it is what signs
> `addRangeEvent` / `confirmEvent` / `resolveRange`. Keep it `0600`, never commit
> it, never paste it into the chat. Back it up: losing it means the oracle can no
> longer confirm or resolve its events.

## Step 2 — Bind a range event to the inference order book

Register the range event on the oracle's `OracleEventList`, pointing `--ob` at the
model's `InferenceOrderBook`. Signed with the oracle keys from Step 1.

```sh
OB="0:<inferenceOrderBookAddress>"          # the model's book (input)
DEADLINE=$(( $(date +%s) + 3600 ))          # unix; ≥ now + MIN_RESULT_GAP

"$DEXDO" add-range-event \
  --endpoint "$ENDPOINT" \
  --event-list-address "$EVENT_LIST" \
  --oracle-pubkey-hex "$ORACLE_PUB" \
  --oracle-secret-hex "$ORACLE_SEC" \
  --event-name "eth-weekly-median-$(date +%s)" \
  --describe "ETH model weekly median band" \
  --ob "$OB" \
  --deadline "$DEADLINE" \
  --bounds "1000000000,2000000000" \
  --outcomes "below-1,1-to-2,above-2" \
  --oracle-fee 0
```

Rules the contract enforces (get them right or the call reverts
`ERR_INVALID_PARAMS`):

- `--bounds` — comma-separated `uint256` **strictly increasing** upper bounds, in
  the book's raw price units. `N` bounds.
- `--outcomes` — comma-separated labels, **exactly `N+1`**, dense buckets `0..N`
  (`< bound0`, `[bound0,bound1)`, …, `≥ boundN-1`). The CLI checks the count
  before sending.
- `--deadline` — unix seconds, at least `MIN_RESULT_GAP` ahead of now (it doubles
  as the PMP result-start; a too-near deadline reverts).
- `--oracle-fee` — raw fee amount (defaults to 0); `--describe` optional.

On success the contract emits `RangeEventAdded(eventId, ob, bounds)`, where
`eventId = hash(eventName, deadline, describe, outcomeNames)`. That `eventId` is
what a `PrivateNote` uses when it `deployPMP`s the prediction market against this
event, and what `resolve-range` needs later. Read it back (library-only) from the
list's `_events` / `getRangeData` getters, or from the `RangeEventAdded` ext-out
event; the read API surfaces it as the hex `eventId` on the oracle's events if you
prefer to look it up there.

Verify the binding landed:

```sh
tvm-cli -u "$ENDPOINT" run "${EVENT_LIST#0:}::${EVENT_LIST#0:}" getRangeData \
  "{\"eventId\":\"<eventId>\"}" \
  --abi "$WORKSPACE/dexdo/contracts/dex/OracleEventList.abi.json"
# → bounds + ob == the InferenceOrderBook you bound
```

## Step 3 (later) — Resolve the range event

After `--deadline`, anyone can trigger resolution; it pulls the book's weekly
median (`requestWeeklyMedian → onWeeklyMedian`) and maps it into an outcome
bucket, resolving the confirmed PMP. Sign with the oracle keys.

```sh
"$DEXDO" resolve-range \
  --endpoint "$ENDPOINT" \
  --event-list-address "$EVENT_LIST" \
  --oracle-pubkey-hex "$ORACLE_PUB" \
  --oracle-secret-hex "$ORACLE_SEC" \
  --event-id "<eventId>" \
  --oracle-list-hash "<oracleListHash>" \
  --token-type 1
```

`--event-id`, `--oracle-list-hash`, and `--token-type` identify the **confirmed
prediction market** (a PMP is `computePMPAddressFromHash(eventId, oracleListHash,
tokenType)`): `eventId` from Step 2; `oracleListHash` and `tokenType` from the PMP
that was deployed against this event (the note-owner / market side). The book
needs enough weekly liquidity, else `_weeklyMedian()` reverts `ERR_NO_LIQUIDITY`.

## The rest of the lifecycle (context, not this skill)

For the market to actually exist and resolve, the prediction-market side must
happen too — a `PrivateNote` deploys a PMP referencing this `eventId`, and the
oracle `confirmEvent`s it (quorum), moving it from STAKING → TRADING → RESOLVING.
Those steps are the note owner's / trader's flow (`dexdo-trading`,
`dexdo-deposit-shellnet`); this skill only owns the oracle side: **deploy the
oracle and bind/resolve the range event against the inference order book.**

## What the user has after the skill

- A deployed, Active `Oracle` + `OracleEventList` under their oracle keypair
  (`$WORKSPACE/oracle/oracle.json`, secret — back it up).
- A range event on that list bound to a chosen `InferenceOrderBook`, so a
  prediction market deployed against its `eventId` resolves from the model's
  weekly-median price — and the `resolve-range` call to settle it after the
  deadline.
