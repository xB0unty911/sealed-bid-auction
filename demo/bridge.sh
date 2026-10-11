#!/usr/bin/env bash
# Bridges tTARI from the L1 console wallet to the L2 public faucet via a
# burn + claim + faucet deposit. Requires a running tari_swarm_daemon localnet.
#
# Usage: ./bridge.sh <amount_tari> <claim_pubkey_hex>
set -euo pipefail

AMOUNT_TARI="${1:?usage: bridge.sh <amount_tari> <claim_pubkey_hex>}"
CLAIM_PUBKEY="${2:?usage: bridge.sh <amount_tari> <claim_pubkey_hex>}"
AMOUNT_UT=$((AMOUNT_TARI * 1000000))

GRPCURL="${GRPCURL:-$HOME/tari/.tools/bin/grpcurl}"
PROTOS="${PROTOS:-$HOME/tari/.tools/protos}"
WALLET_ADDR="${WALLET_ADDR:-127.0.0.1:12004}"
SWARM_ADDR="${SWARM_ADDR:-127.0.0.1:8080}"
BURN_PROOFS="$HOME/.local/share/tari/localnet/burn_proofs"
DEMO="${DEMO:-$HOME/tari/sealed_bid/target/debug/sealed-bid-demo}"

BEFORE=$(ls -A "$BURN_PROOFS" 2>/dev/null | wc -l)

CLAIM_B64=$(python3 -c "import base64,sys;print(base64.b64encode(bytes.fromhex(sys.argv[1])).decode())" "$CLAIM_PUBKEY")
"$GRPCURL" -plaintext -import-path "$PROTOS" -proto wallet.proto \
  -d "{\"amount\":\"$AMOUNT_UT\",\"feePerGram\":\"10\",\"claimPublicKey\":\"$CLAIM_B64\"}" \
  "$WALLET_ADDR" tari.rpc.Wallet/CreateBurnTransaction | grep -q '"isSuccess": true'

echo "burn transaction submitted; mining until the proof file appears..."
for _ in $(seq 1 10); do
  curl -s --max-time 30 -X POST "http://$SWARM_ADDR/json_rpc" \
    -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","method":"mine","params":{"num_blocks":8},"id":1}' > /dev/null
  sleep 20
  NOW=$(ls -A "$BURN_PROOFS" | wc -l)
  if [ "$NOW" -gt "$BEFORE" ]; then
    break
  fi
done

PROOF=$(ls -t "$BURN_PROOFS"/*.json | head -1)
echo "proof: $PROOF"
DEMO_NETWORK=localnet DEMO_INDEXER_URL=http://127.0.0.1:12007 \
  "$DEMO" bootstrap faucet-hero "$PROOF"
