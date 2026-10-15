# Sealed Bids at Midnight

A sealed-bid auction template for [Tari Ootle](https://www.tari.com/) where **bid amounts do not exist on-chain until their owners reveal them**.

Built for the October 2026 "Spooky Secrets" community build contest — a demonstration of Tari's programmable privacy.

## The problem

On a transparent chain, submitting a bid puts it in a public mempool and state, where anyone can front-run it or bid one unit over it. Commit-reveal schemes fix the bidding phase, and they can be built on any chain; what makes this template native to Tari is how little machinery it needs — one WASM template, no external contracts, bonds and escrow as ordinary vaults, and identity straight from the transaction signer.

While the auction is sealed, privacy here is structural:

| | While bidding is open | After midnight (reveal window) |
|---|---|---|
| The chain sees | your public key, a Blake2b commitment, a bond identical for every bidder | the amounts, proven against the seals |
| Your bid amount | **never leaves your machine — it does not exist on-chain** | revealed by you, only after the sealing window has closed |

Honest limits (found the hard way, see the security review below): a bidder holding several sealed bids can choose which to open after seeing others' reveals, so the reveal window weakens true sealed-bid incentives unless the bond is large; and during the reveal window all opened amounts are public. The sealing phase itself remains opaque: commitments are domain-separated and bound to both the auction and the signer, and no amount exists on-chain until its owner reveals it.

## How it works

```
        epoch 0                    bidding_end                reveal_end
  ────────┬────────────────────────────┬──────────────────────────┬─────────
   seller creates auction         midnight: bids are        settlement: the lot
   bidders seal commitments       frozen; honest bidders    goes to the highest
   + equal anti-spam bonds        reveal + pay up front     bid, losers are
   (ghosts: bids that are never   ; ghosts stay silent      refunded, ghosts
   revealed forfeit their bonds)  and forfeit their bonds   forfeit to the seller
```

- **`place_bid(commitment, refund_account, bond)`** — the client computes `commitment = blake2b(pk ‖ blinding ‖ amount_le)`. The bond is the same for every bidder, so it leaks nothing.
- **`reveal(blinding, amount, payment)`** — the template recomputes the hash from the unforgeable signer public key and pays the exact amount up front. The bond is returned immediately to honest bidders.
- **`settle()`** — highest revealed bid at or above the reserve wins (earliest reveal breaks exact ties). Settlement is **pull-based**: `settle()` only records the winner; the lot, the proceeds and every refund are then claimed individually (`claim_lot`, `claim_proceeds`, `claim_refund`), so no failed deposit can ever lock someone else's funds. **Ghost bids — sealed, never revealed — have their bonds locked in the component forever**: paid to no one, not even the seller, so shill bids that stay sealed are pure cost.

The official Ootle `nft_marketplace` starter ships an English auction and leaves blind auctions as a TODO: *"requires cryptography support"*. This template closes that gap with Ootle's native primitives: hashing intrinsics, `CallerContext` identity, consensus epochs for phases, and vaults for escrow.

## Repo layout

- `src/lib.rs` — the `SealedBidAuction` template (WASM, `tari_template_lib` 0.34)
- `tests/test.rs` — 15 integration tests via `tari_template_test_tooling`: full lifecycle, tie-breaking in both seal orders, reserve semantics, cross-auction commitment binding, capacity cap, constructor and window rejections, claim authorization and replay, and a 20-bidder scale run
- `demo/` — a headless CLI client (`ootle-rs`) that drives the whole auction lifecycle on a network
- `demo/bridge.sh` — L1 → L2 bridge helper: burns tTARI on the L1 console wallet and funds the demo actors from the claimed UTXO

## Live on the public Esmeralda testnet (October 2026)

- Template: `21205337687900cb38b22d6f988de25fa9e1f828faac4be7460a78fb135bda0a`
- Auction: `component_e467ca6064eb4f94784d033f481f8abdff9953efae6f2e4e1aea3141af99888a`
- Settled: winner paid 20 TARI for a minted SPOOK lot, the losing bid was refunded, and one ghost forfeited its bond. Every event is on the public indexer, e.g.
  `https://ootle-indexer-a.tari.com/substates/component_e467ca6064eb4f94784d033f481f8abdff9953efae6f2e4e1aea3141af99888a?local_search_only=false`
- The actors were funded by mining L1 test Tari solo (difficulty ~33), burning it and claiming on L2 — the same bridge the demo client automates.

## Quick start on a local network

Prerequisites: Rust with the `wasm32-unknown-unknown` target, and a local Ootle swarm running (`tari_swarm_daemon start --no-compile --binaries-root <binaries>`; its web UI is on `127.0.0.1:8080`, the indexer REST on `127.0.0.1:12007`).

```bash
cargo test                          # 8/8 template tests, no network needed
tari build                          # produce the WASM

cd demo
export DEMO_NETWORK=localnet DEMO_INDEXER_URL=http://127.0.0.1:12007
DEMO=../target/debug/sealed-bid-demo

$DEMO new-actor faucet-hero         # prints the L1 claim public key
./bridge.sh 15000 <claim-pubkey>    # burn on L1, claim on L2, fund all actors
$DEMO setup                         # publish the template, mint a lot, open the auction
$DEMO bots-bid                      # three sealed bids (10, 20 and a 99 TARI ghost)
#   ... advance a couple of epochs (swarm UI: Mine, or POST {"method":"mine"}) ...
$DEMO bots-reveal                   # the two honest bids open; the ghost stays silent
#   ... advance past the reveal window ...
$DEMO settle                        # settle and print every actor's balances
```

You can play manually at any point: `bid <actor> <tari>` (prints the blinding factor to keep), `reveal <actor> <tari> <blinding-hex>`, `info` dumps the on-chain auction state.

### A real run (localnet, October 2026)

```
=> bidder1 seals 10 TARI (commitment bac01bff79b4f04e...bbac213d) — the amount never leaves this machine
=> bidder2 seals 20 TARI (commitment 1c5ec22c93e23e0...99bc7b392)
=> ghost   seals 99 TARI (commitment 40ad3acbfe199421...893a5cc9)
All bids sealed. The chain sees commitments only — no amounts.
...
=> settling the auction
   event: SealedBidAuction.AuctionSettled { winner: bidder2, winning_bid: "20.000000", ghost_bonds_forfeited: "1" }

  [seller]   TARI = 518902248 µT   (+20 winning bid +2 forfeited ghost bond)
  [bidder1]  TARI = 499992404 µT   (losing bid refunded in full)
  [bidder2]  TARI = 479992001 µT + 1 × SPOOK "Haunted Portrait #13"   (the winner)
  [ghost]    TARI = 497996326 µT   (bond forfeited: sealed and vanished)
```

## Security review

Before submission the project was reviewed by an independent auditor (Claude Opus, adversarial pass). All accepted findings were fixed in the v2 template and client, and regression tests were added for each:

- **Funds-locking settlement (critical):** `settle()` used to pay everyone in one transaction; one hostile account or bid spam could lock every bidder's funds. Settlement is now pull-based with per-party claims.
- **Seller traps:** overflowing epoch arithmetic could open settlement instantly — now checked; the component now has `OwnerRule::None`, so its rules can never be rewritten once escrow is held.
- **Shill-bond incentive:** forfeited bonds used to go to the seller, making sealed shill bids free. They are now locked in the component forever.
- **Commitment binding:** commitments are now domain-separated (`"sealed_bid/v1"`) and bound to the auction's component address with a mandatory 32-byte blinding, so they cannot leak across auctions or be brute-forced with short blindings.
- **Reserve fairness:** a valid reveal below the reserve used to reject and forfeit the bond; it is now accepted (bond returned) and simply cannot win.
- **Client:** bid blindings are persisted before the transaction is submitted, and bridge/refill statements carry explicit change outputs.

## Engineering notes

Platform constraints discovered while building this (each cost us a rejected transaction):

- Template method signatures only accept paths and tuples — `[u8; N]` arrays are rejected by the ABI, so the blinding factor travels as `Vec<u8>`.
- `tari_template_lib::intrinsics` (including hashing) only run inside the WASM engine; native tests recompute commitments with the `blake2` crate (same Blake2b-256 over the concatenated parts).
- Tari (the stealth TARI resource) cannot be burned by templates (`native.resource.Burn` is denied), so the original "ghost bonds burn at midnight" became a forfeiture to the seller — economically equivalent, and fairer to the seller.
- A template that cross-calls another component (`account.deposit` during payouts) requires that component *and its vaults* to be declared as transaction inputs.
- An L1 burn claim pays its own fee out of the claimed UTXO, so the spendable amount is the proof value minus the receipt's `total_fees_paid`; and the claim is only valid once the L2 validators have synced past the epoch containing the burn block.
- Historical: the public Esmeralda faucet was underfunded by exactly its own claim size (paid a fixed 1000 TARI, held 999.65). We worked around it by mining L1 test Tari, burning it and claiming on L2 (see "Live on the public Esmeralda testnet" above); the same bridge funds the demo on any network.

## License

[MIT](LICENSE)
