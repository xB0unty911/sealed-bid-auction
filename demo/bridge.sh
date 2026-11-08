#!/usr/bin/env bash
# Bridges tTARI from the L1 (Minotari) console wallet to the L2 network via a
# burn + claim, then funds the demo actors.
#
# On the public esmeralda testnet the XTR faucet starts empty by design, so
# this is the documented way to fund the demo (see
# https://ootle.tari.com/guides/burn-minotari/). On a local swarm the optional
# SWARM_ADDR mine-trigger speeds up confirmation.
#
# Requires: a synced minotari_node (grpc 18152) and the console wallet gRPC
# (WALLET_ADDR). The burn proof is converted to the engine-side claim by
# ../target/debug/l1convert before the demo claims it.
#
# Usage: ./bridge.sh <amount_tari> <claim_pubkey_hex>
set -euo pipefail

AMOUNT_TARI="${1:?usage: bridge.sh <amount_tari> <claim_pubkey_hex>}"
CLAIM_PUBKEY="${2:?usage: bridge.sh <amount_tari> <claim_pubkey_hex>}"
AMOUNT_UT=$((AMOUNT_TARI * 1000000))

GRPCURL="${GRPCURL:-$HOME/tari/.tools/bin/grpcurl}"
PROTOS="${PROTOS:-$HOME/tari/.tools/protos}"
WALLET_ADDR="${WALLET_ADDR:-127.0.0.1:12004}"
SWARM_ADDR="${SWARM_ADDR:-}"          # set to a local swarm UI to force mining
BURN_PROOFS="${BURN_PROOFS:-$HOME/.local/share/tari/esmeralda/burn_proofs}"
DEMO="${DEMO:-$HOME/tari/sealed_bid/target/debug/sealed-bid-demo}"
L1CONVERT="${L1CONVERT:-$HOME/tari/sealed_bid/target/debug/l1convert}"

BEFORE=$(ls -A "$BURN_PROOFS" 2>/dev/null | wc -l)

CLAIM_B64=$(python3 -c "import base64,sys;print(base64.b64encode(bytes.fromhex(sys.argv[1])).decode())" "$CLAIM_PUBKEY")
"$GRPCURL" -plaintext -import-path "$PROTOS" -proto wallet.proto \
  -d "{\"amount\":\"$AMOUNT_UT\",\"feePerGram\":\"10\",\"claimPublicKey\":\"$CLAIM_B64\"}" \
  "$WALLET_ADDR" tari.rpc.Wallet/CreateBurnTransaction | grep -q '"isSuccess": true'

echo "burn transaction submitted; waiting for the proof file..."
for _ in $(seq 1 30); do
  if [ -n "$SWARM_ADDR" ]; then
    curl -s --max-time 30 -X POST "http://$SWARM_ADDR/json_rpc" \
      -H "Content-Type: application/json" \
      -d '{"jsonrpc":"2.0","method":"mine","params":{"num_blocks":8},"id":1}' > /dev/null
  fi
  sleep 20
  NOW=$(ls -A "$BURN_PROOFS" 2>/dev/null | wc -l)
  if [ "$NOW" -gt "$BEFORE" ]; then
    break
  fi
done

PROOF=$(ls -t "$BURN_PROOFS"/*.json | head -1)
echo "proof: $PROOF"
CONVERTED=$(mktemp -u /tmp/converted-claim-XXXXXX.json)
"$L1CONVERT" "$PROOF" "$CONVERTED"

# The claim is only valid from the L2 epoch after the one containing the burn
# block. If it is rejected as NotYetValid, rerun this line after that epoch:
echo "claim file: $CONVERTED"
"$DEMO" bootstrap faucet-hero "$CONVERTED"
