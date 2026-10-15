//! Headless demo client for the sealed-bid auction template.
//!
//! Run `sealed-bid-demo help` for the list of commands. The demo drives the
//! full auction lifecycle on the Esmeralda testnet: faucet, template
//! publish, lot mint, auction creation, sealed bids, reveals, settlement.

use std::{path::PathBuf, str::FromStr};

use blake2::digest::{Update, VariableOutput};
use blake2::Blake2bVar;
use ootle_rs::builtin_templates::{
    account::IAccount,
    component::IComponent,
    faucet::IFaucet,
    UnsignedTransactionBuilder,
};
use ootle_rs::claim_burn::{ClaimBurn, MinotariBurnClaimProof};
use ootle_rs::keys::OotleSecretKey;
use ootle_rs::provider::{IndexerProvider, PendingTransaction, ProviderBuilder, Provider};
use ootle_byte_type::ToByteType;
use ootle_rs::stealth::{Output, StealthTransfer};
use ootle_rs::template_types::UtxoAddress;
use ootle_rs::wallet::NetworkWallet;
use ootle_rs::template_types::{
    constants::{TARI_TOKEN, XTR_FAUCET_COMPONENT_ADDRESS},
    Amount, ComponentAddress, EncryptedData, Hash32,
};
use ootle_rs::wallet::OotleWallet;
use ootle_rs::{default_indexer_url, ToAccountAddress, TransactionRequest, Network};
use serde::{Deserialize, Serialize};
use tari_utilities::{byte_array::ByteArray, hex::Hex};
use tari_crypto::{
    keys::PublicKey,
    ristretto::{RistrettoPublicKey, RistrettoSecretKey},
};
use ootle_rs::builtin_templates::component::TransactionBuildable;
use ootle_rs::provider::WalletProvider;
use tari_ootle_transaction::{args, Epoch, Transaction, UnsignedTransaction};

/// DEMO_NETWORK=localnet runs the demo against a local tari_swarm_daemon
/// network; the default is the public Esmeralda testnet.
fn network() -> Network {
    match std::env::var("DEMO_NETWORK").as_deref() {
        Ok("localnet") => Network::LocalNet,
        _ => Network::Esmeralda,
    }
}

/// DEMO_INDEXER_URL overrides the indexer endpoint (e.g. a local swarm).
fn indexer_url() -> String {
    std::env::var("DEMO_INDEXER_URL")
        .unwrap_or_else(|_| default_indexer_url(network()).to_string())
}
/// 1 TARI = 1,000,000 µT.
const TARI: u64 = 1_000_000;

/// Demo defaults; keep small so the faucet covers everything.
const DEMO_BOND_TARI: u64 = 2;
const DEMO_BIDS_TARI: [u64; 2] = [10, 20];
const GHOST_BID_TARI: u64 = 99;

#[derive(Serialize, Deserialize, Clone)]
struct ActorState {
    name: String,
    account_secret_hex: String,
    view_secret_hex: String,
}

impl ActorState {
    fn random(name: &str) -> Self {
        let secret = OotleSecretKey::random(network());
        Self {
            name: name.to_string(),
            account_secret_hex: secret.account_secret().to_hex(),
            view_secret_hex: secret.view_only_secret().to_hex(),
        }
    }

    fn to_wallet(&self) -> OotleWallet {
        let account = RistrettoSecretKey::from_hex(&self.account_secret_hex).expect("account hex");
        let view = RistrettoSecretKey::from_hex(&self.view_secret_hex).expect("view hex");
        OotleWallet::from(PrivateKeyProvider::new(OotleSecretKey::new(
            network(), account, view,
        )))
    }

    fn public_key(&self) -> RistrettoPublicKey {
        let account = RistrettoSecretKey::from_hex(&self.account_secret_hex).expect("account hex");
        RistrettoPublicKey::from_secret_key(&account)
    }
}

type DemoProvider = IndexerProvider<OotleWallet>;

use ootle_rs::key_provider::PrivateKeyProvider;

async fn actor_provider(actor: &ActorState) -> DemoProvider {
    ProviderBuilder::new()
        .wallet(actor.to_wallet())
        .connect_with_transaction_timeout(
            indexer_url(),
            std::time::Duration::from_secs(600),
        )
        .await
        .expect("connect to indexer")
}

#[derive(Serialize, Deserialize, Default)]
struct DemoState {
    template_address: Option<String>,
    auction_address: Option<String>,
    bond_tari: u64,
    actors: Vec<ActorState>,
    /// actor name -> (blinding hex, amount µT) for bids made through this CLI
    bot_bids: std::collections::BTreeMap<String, (String, u64)>,
}

impl DemoState {
    fn load() -> Self {
        match std::fs::read_to_string(state_path()) {
            Ok(text) => serde_json::from_str(&text).expect("parse demo-state.json"),
            Err(_) => Self {
                bond_tari: DEMO_BOND_TARI,
                ..Default::default()
            },
        }
    }

    fn save(&self) {
        std::fs::write(state_path(), serde_json::to_string_pretty(self).unwrap())
            .expect("write demo-state.json");
    }

    fn auction(&self) -> ComponentAddress {
        let raw = self.auction_address.as_deref().expect("run setup first");
        let normalized = raw.strip_prefix("component_").unwrap_or(raw);
        ComponentAddress::from_str(&format!("component_{normalized}")).expect("auction address")
    }

    fn actor(&self, name: &str) -> &ActorState {
        self.actors
            .iter()
            .find(|a| a.name == name)
            .unwrap_or_else(|| {
                panic!(
                    "unknown actor '{name}' (known: {:?})",
                    self.actors.iter().map(|a| &a.name).collect::<Vec<_>>()
                )
            })
    }
}

fn state_path() -> PathBuf {
    let suffix = match network() {
        Network::LocalNet => "localnet",
        _ => "esmeralda",
    };
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("demo-state.{suffix}.json"))
}

fn wasm_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("target/wasm32-unknown-unknown/release/sealed_bid.wasm")
}

/// The commitment the template expects: blake2b-256(pk || blinding || amount_le).
fn commitment_for(public_key: &RistrettoPublicKey, blinding: &[u8], amount: Amount) -> Hash32 {
    let mut hasher = Blake2bVar::new(32).expect("32-byte output");
    hasher.update(public_key.as_bytes());
    hasher.update(blinding);
    hasher.update(&amount.to_le_bytes());
    let mut digest = [0u8; 32];
    hasher.finalize_variable(&mut digest).expect("infallible");
    Hash32::try_from_slice(&digest).expect("32 bytes")
}

async fn wait_for_tx(pending: &PendingTransaction) {
    // On quiet public testnets a transaction may only finalize when the next
    // L2 block is produced, which can take a whole epoch. Watch, then poll
    // the receipt directly for up to ~35 minutes.
    if let Ok(outcome) = pending.watch().await {
        println!("    => tx {} finalized: {:?}", pending.tx_id(), outcome);
        if outcome.is_reject() {
            panic!("transaction rejected: {:?}", outcome);
        }
        return;
    }
    println!("    => tx {} slow, polling the result...", pending.tx_id());
    for _ in 0..35 {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        if let Ok(receipt) = pending.get_receipt().await {
            match receipt.outcome() {
                tari_engine_types::transaction_receipt::FinalizeOutcome::Commit => {
                    println!("    => tx {} committed (polled)", pending.tx_id());
                    return;
                },
                tari_engine_types::transaction_receipt::FinalizeOutcome::FeeIntentCommit => {
                    panic!("transaction fee-intent-only (polled): {:?}", receipt.outcome());
                },
            }
        }
    }
    panic!("transaction never finalized within the polling window");
}

async fn max_epoch(provider: &DemoProvider) -> Epoch {
    Epoch(provider.get_epoch().await.expect("get epoch").as_u64() + 40)
}

async fn submit(provider: &mut DemoProvider, unsigned: UnsignedTransaction) -> PendingTransaction {
    let tx = TransactionRequest::default()
        .with_transaction(unsigned)
        .build(provider.wallet())
        .await
        .expect("build transaction");
    provider.send_transaction(tx).await.expect("send transaction")
}

/// The public faucet pays a fixed 1000 TARI per claim and can run low;
/// retry for a while in case it gets refilled.
async fn faucet(provider: &mut DemoProvider) {
    const ATTEMPTS: usize = 5;
    for attempt in 1..=ATTEMPTS {
        let epoch = max_epoch(provider).await;
        let unsigned = IFaucet::new(provider, epoch)
            .take_faucet_funds()
            .pay_fee(2_000u64)
            .prepare()
            .await
            .expect("prepare faucet tx");
        let pending = submit(provider, unsigned).await;
        let outcome = pending.watch().await.expect("watch faucet tx");
        println!("    => faucet attempt {attempt}/{ATTEMPTS}: {outcome:?}");
        if !outcome.is_reject() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }
    panic!("faucet claims kept failing; the public faucet may be empty, retry later");
}

async fn print_balances(actor: &ActorState) {
    let provider = actor_provider(actor).await;
    let account = provider.default_signer_address().to_account_address();
    if provider.get_substate(account).await.ok().flatten().is_none() {
        println!("  [{}] no account component (UTXO-only actor)", actor.name);
        return;
    }
    let balances = provider.get_account_balances(account).await.expect("balances");
    println!("  [{}] balances:", actor.name);
    for (resource, amount) in &balances {
        println!("    {resource} = {} µT", amount.to_u64_checked().unwrap_or(0));
    }
    if balances.is_empty() {
        println!("    (empty)");
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("help");
    let rest = &args[1..];

    match cmd {
        "setup" => cmd_setup().await,
        "bots-bid" => cmd_bots_bid().await,
        "bots-reveal" => cmd_bots_reveal().await,
        "settle" => cmd_settle().await,
        "info" => cmd_info().await,
        "faucet" => cmd_faucet(rest).await,
        "new-actor" => cmd_new_actor(rest).await,
        "bootstrap" => cmd_bootstrap(rest).await,
        "faucet-refill" => cmd_faucet_refill(rest).await,
        "fund-utxo" => cmd_fund_utxo(rest).await,
        "addresses" => cmd_addresses().await,
        "bid" => cmd_bid(rest).await,
        "reveal" => cmd_reveal(rest).await,
        _ => print_help(),
    }
}

fn print_help() {
    println!(
        "sealed-bid-demo — headless demo for the sealed-bid auction template (Esmeralda testnet)

USAGE:
  setup                  faucet all actors, publish the template, mint a lot,
                         create the auction, save demo-state.json
  bots-bid               the three bot actors seal their bids
  bots-reveal            the two honest bots reveal; the ghost stays silent
  settle                 settle the auction and print the outcome
  info                   dump the on-chain auction state and current epoch

  faucet <actor>         claim faucet funds for an actor (creates it if new)
  new-actor <name>       create an actor and print its L1 claim public key
  bootstrap <actor> <proof.json>
                         claim a burn proof and fund all demo actors straight
                         from the claimed UTXO (local networks have no faucet)
  faucet-refill <actor> <tari> <proof.json>
                         claim a burn and deposit the funds into the public
                         XTR faucet (topping up the testnet for everyone)
  fund-utxo <actor> <commitment-hex> <claimed-microtari>
                         fund all demo actors from an already-claimed UTXO
                         (resume after a claim that committed but timed out)
  addresses              print every actor's ootle address and account
  bid <actor> <tari>     manually seal a bid (prints the blinding factor to keep)
  reveal <actor> <tari> <blinding-hex>
                         manually reveal a sealed bid

`setup` creates the actors (seller, bidder1, bidder2, ghost) and stores their
keys in demo/demo-state.json. One epoch is ~30 minutes: bids land immediately
during the sealing window, reveals after it closes, settlement after that."
    );
}

async fn cmd_setup() {
    let mut state = DemoState::load();
    assert!(
        !state.actors.is_empty(),
        "no actors: run new-actor / bootstrap first"
    );
    state.save();

    // 1. Publish the template from the seller.
    let seller = state.actor("seller").clone();
    let mut provider = actor_provider(&seller).await;
    println!("=> publishing template");
    let wasm = std::fs::read(wasm_path()).expect("wasm not built: run `tari build` in the template crate first");
    let epoch = max_epoch(&provider).await;
    let unsigned = IAccount::new(&provider, epoch)
        .publish_template(tari_ootle_transaction::Blob::from(wasm))
        .pay_fee(3_000_000u64)
        .prepare()
        .await
        .expect("prepare publish");
    let pending = submit(&mut provider, unsigned).await;
    wait_for_tx(&pending).await;
    let receipt = pending.get_receipt().await.expect("receipt");
    let template = receipt
        .diff_summary()
        .upped
        .iter()
        .find_map(|s| s.substate_id.as_template())
        .expect("template address in receipt");
    state.template_address = Some(template.as_template_address().to_string());
    println!("   template: {}", template.as_template_address());

    // 3. Mint a lot and open the auction in one transaction.
    println!("=> minting lot + creating auction");
    let seller_account = provider.default_signer_address().to_account_address();
    let epoch = max_epoch(&provider).await;
    let unsigned = IComponent::new(&provider, epoch)
        .call_function(
            template.as_template_address(),
            "mint_lot",
            args!["Haunted Portrait #13".to_string(), "SPOOK".to_string()],
        )
        .put_last_instruction_output_on_workspace("lot")
        .call_function(
            template.as_template_address(),
            "new",
            args![
                Workspace("lot"),
                seller_account,
                Option::<Amount>::None,
                Amount::from_integer(state.bond_tari * TARI),
                1u64,
                2u64,
            ],
        )
        .pay_fee(200_000u64)
        .prepare()
        .await
        .expect("prepare create auction");
    let pending = submit(&mut provider, unsigned).await;
    wait_for_tx(&pending).await;
    let receipt = pending.get_receipt().await.expect("receipt");
    let auction = receipt
        .diff_summary()
        .upped
        .iter()
        .find_map(|s| s.substate_id.as_component_address())
        .expect("auction component in receipt");
    state.auction_address = Some(auction.to_string());
    println!("   auction:  {auction}");
    state.save();

    println!(
        "\nSetup done. The sealing window is open for ~1 epoch (about 30 min).\n\
         Run `bots-bid` now, `bots-reveal` after the window closes,\n\
         `settle` after the reveal window closes (about 2 more epochs)."
    );
}

async fn cmd_bots_bid() {
    let mut state = DemoState::load();
    let auction = state.auction();

    let plans = [
        ("bidder1", DEMO_BIDS_TARI[0]),
        ("bidder2", DEMO_BIDS_TARI[1]),
        ("ghost", GHOST_BID_TARI),
    ];
    for (name, bid_tari) in plans {
        let actor = state.actor(name).clone();
        let mut provider = actor_provider(&actor).await;
        let account = provider.default_signer_address().to_account_address();
        let amount = Amount::from_integer(bid_tari * TARI);
        let blinding: [u8; 32] = rand::random();
        let commitment = commitment_for(&actor.public_key(), &blinding, amount);
        println!(
            "=> {name} seals {bid_tari} TARI (commitment {commitment}) — the amount never leaves this machine",
        );
        let epoch = max_epoch(&provider).await;
        let unsigned = IComponent::new(&provider, epoch)
            .call_method(account, "withdraw", args![TARI_TOKEN, Amount::from_integer(state.bond_tari * TARI)])
            .put_last_instruction_output_on_workspace("bond")
            .call_method(
                auction,
                "place_bid",
                args![commitment, account, Workspace("bond")],
            )
            .pay_fee(100_000u64)
            .prepare()
            .await
            .expect("prepare place_bid");
        let pending = submit(&mut provider, unsigned).await;
        wait_for_tx(&pending).await;
        state
            .bot_bids
            .insert(name.to_string(), (hex(blinding), bid_tari * TARI));
    }
    state.save();
    println!("All bids sealed. The chain sees commitments only — no amounts.");
}

async fn cmd_bots_reveal() {
    let state = DemoState::load();
    let auction = state.auction();
    for name in ["bidder1", "bidder2"] {
        let (blinding_hex, amount_u) = state
            .bot_bids
            .get(name)
            .cloned()
            .unwrap_or_else(|| panic!("no sealed bid recorded for {name}; run bots-bid first"));
        let actor = state.actor(name).clone();
        let mut provider = actor_provider(&actor).await;
        let account = provider.default_signer_address().to_account_address();
        let amount = Amount::from_integer(amount_u);
        println!("=> {name} reveals {} µT", amount_u);
        let epoch = max_epoch(&provider).await;
        let unsigned = IComponent::new(&provider, epoch)
            .call_method(account, "withdraw", args![TARI_TOKEN, amount])
            .put_last_instruction_output_on_workspace("payment")
            .call_method(
                auction,
                "reveal",
                args![unhex(&blinding_hex), amount, Workspace("payment")],
            )
            .pay_fee(100_000u64)
            .prepare()
            .await
            .expect("prepare reveal");
        let pending = submit(&mut provider, unsigned).await;
        if let Err(err) = try_wait_tx(&pending).await {
            println!("   ({name}: {err}) — continuing with the remaining bids");
        }
    }
    println!("Honest bids are revealed. The ghost stays silent — its bond will be forfeited.");
}

async fn cmd_settle() {
    let state = DemoState::load();
    let auction = state.auction();
    let actor = state.actor("seller").clone();
    let mut provider = actor_provider(&actor).await;
    println!("=> settling the auction");
    let epoch = max_epoch(&provider).await;
    // Settlement pays out to the seller and refunds every bidder, so every
    // account component it touches must be declared as a transaction input.
    let mut builder = IComponent::new(&provider, epoch)
        .call_method(auction, "settle", args![]);
    for actor in &state.actors {
        let provider = actor_provider(actor).await;
        let account = provider.default_signer_address().to_account_address();
        // Only actors whose account component actually exists participate.
        if provider.get_substate(account).await.ok().flatten().is_some() {
            builder = UnsignedTransactionBuilder::add_input(builder, account);
            builder = TransactionBuildable::want_all_vaults(builder, account);
        }
    }
    let unsigned = builder
        .pay_fee(5_000_000u64)
        .prepare()
        .await
        .expect("prepare settle");
    let pending = submit(&mut provider, unsigned).await;
    wait_for_tx(&pending).await;
    let receipt = pending.get_receipt().await.expect("receipt");
    for event in receipt.events.iter() {
        println!("   event: {event:?}");
    }
    for actor in &state.actors {
        print_balances(actor).await;
    }
}

async fn cmd_info() {
    let state = DemoState::load();
    let auction = state.auction();
    let actor = state.actor("seller").clone();
    let provider = actor_provider(&actor).await;
    let substate = provider
        .fetch_substate(auction)
        .await
        .expect("fetch auction substate");
    println!("auction {auction}");
    println!("{}", serde_json::to_string_pretty(&substate).unwrap());
    let epoch = provider.get_epoch().await.expect("epoch");
    println!("current epoch: {}", epoch.as_u64());
}

async fn cmd_faucet(rest: &[String]) {
    let mut state = DemoState::load();
    let name = rest.first().expect("usage: faucet <actor>");
    if !state.actors.iter().any(|a| &a.name == name) {
        state.actors.push(ActorState::random(name));
        state.save();
        println!("created actor '{name}' (keys saved to demo-state.json)");
    }
    let actor = state.actor(name).clone();
    let mut provider = actor_provider(&actor).await;
    println!("=> faucet for {name}");
    faucet(&mut provider).await;
    print_balances(&actor).await;
}

async fn cmd_bid(rest: &[String]) {
    let mut state = DemoState::load();
    let name = rest.first().expect("usage: bid <actor> <tari>");
    let tari: u64 = rest
        .get(1)
        .expect("usage: bid <actor> <tari>")
        .parse()
        .expect("tari amount");
    let auction = state.auction();
    let actor = state.actor(name).clone();
    let mut provider = actor_provider(&actor).await;
    let account = provider.default_signer_address().to_account_address();
    let amount = Amount::from_integer(tari * TARI);
    let blinding: [u8; 32] = rand::random();
    let commitment = commitment_for(&actor.public_key(), &blinding, amount);
    println!(
        "=> {name} seals {tari} TARI; commitment {commitment}\n   KEEP THIS BLINDING: {}",
        hex(blinding)
    );
    let epoch = max_epoch(&provider).await;
    let unsigned = IComponent::new(&provider, epoch)
        .call_method(account, "withdraw", args![TARI_TOKEN, Amount::from_integer(state.bond_tari * TARI)])
        .put_last_instruction_output_on_workspace("bond")
        .call_method(
            auction,
            "place_bid",
            args![commitment, account, Workspace("bond")],
        )
        .pay_fee(100_000u64)
        .prepare()
        .await
        .expect("prepare place_bid");
    let pending = submit(&mut provider, unsigned).await;
    wait_for_tx(&pending).await;
    state
        .bot_bids
        .insert(name.to_string(), (hex(blinding), tari * TARI));
    state.save();
}

async fn cmd_reveal(rest: &[String]) {
    let state = DemoState::load();
    let name = rest.first().expect("usage: reveal <actor> <tari> <blinding-hex>");
    let tari: u64 = rest
        .get(1)
        .expect("usage: reveal <actor> <tari> <blinding-hex>")
        .parse()
        .expect("tari amount");
    let blinding_hex = rest.get(2).expect("usage: reveal <actor> <tari> <blinding-hex>");
    let auction = state.auction();
    let actor = state.actor(name).clone();
    let mut provider = actor_provider(&actor).await;
    let account = provider.default_signer_address().to_account_address();
    let amount = Amount::from_integer(tari * TARI);
    let epoch = max_epoch(&provider).await;
    let unsigned = IComponent::new(&provider, epoch)
        .call_method(account, "withdraw", args![TARI_TOKEN, amount])
        .put_last_instruction_output_on_workspace("payment")
        .call_method(
            auction,
            "reveal",
            args![unhex(blinding_hex), amount, Workspace("payment")],
        )
        .pay_fee(100_000u64)
        .prepare()
        .await
        .expect("prepare reveal");
    let pending = submit(&mut provider, unsigned).await;
    wait_for_tx(&pending).await;
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).expect("hex byte"))
        .collect()
}

async fn cmd_new_actor(rest: &[String]) {
    let mut state = DemoState::load();
    let name = rest.first().expect("usage: new-actor <name>");
    if state.actors.iter().any(|a| &a.name == name) {
        panic!("actor '{name}' already exists");
    }
    let actor = ActorState::random(name);
    let wallet = actor.to_wallet();
    println!("network:           {:?}", network());
    println!("actor:             {name}");
    println!("ootle address:     {}", wallet.default_address());
    println!("L1 claim public key (hex): {}", actor.public_key().to_hex());
    state.actors.push(actor);
    state.save();
    println!("(saved to demo-state.json)");
}

/// The JSON file the L1 console wallet writes into burn_proofs/.
type BurnProofFile = tari_sidechain::CompleteClaimBurnProof;

// Adapted from tari-project/tari-ootle applications/tari_ootle_app_utilities
// src/burn_claim_proof.rs (BSD-3-Clause): converts the L1 proof into the
// engine-side claim proof.
fn claim_proof_from_l1(proof: &tari_sidechain::BurnClaimProof) -> Result<MinotariBurnClaimProof, String> {
    use ootle_byte_type::ToByteType;
    use tari_common_types::burn_proof::{BurnOutputProof, MmrInclusionProof};
    use tari_engine_types::confidential::{
        BurnOutput,
        BurnOutputFeatures,
        BurnOutputInclusionProof,
        BurnSidechainId,
        MmrInclusionProof as ClaimMmrInclusionProof,
    };
    use tari_transaction_components::transaction_components::{OutputFeatures, OutputType, SideChainFeatureData};
    use ootle_rs::template_types::Hash32;

    fn bytes<const N: usize>(field: &str, bytes: Vec<u8>) -> Result<bounded_vec::BoundedVec<u8, 1, N>, String> {
        let len = bytes.len();
        bounded_vec::BoundedVec::<u8, 1, N>::from_vec(bytes)
            .map_err(|_| format!("burn output {field} length {len} is out of range"))
    }

    fn mmr_proof(proof: &MmrInclusionProof) -> ClaimMmrInclusionProof {
        let hashes = |hashes: &[tari_common_types::types::FixedHash]| {
            hashes
                .iter()
                .map(|hash| Hash32::from_array(hash.into_array()))
                .collect()
        };
        ClaimMmrInclusionProof {
            leaf_index: proof.leaf_index,
            mmr_size: proof.mmr_size,
            path: hashes(&proof.path),
            peaks: hashes(&proof.peaks),
        }
    }

    let BurnOutputProof {
        block_hash,
        output,
        normal_output_proof,
        normal_output_mr,
        block_output_proof,
        ..
    } = &proof.output_proof;

    let features: OutputFeatures =
        borsh::from_slice(&output.features).map_err(|e| format!("invalid burn output features: {e}"))?;
    if features.output_type != OutputType::Burn {
        return Err(format!("the output is a {} output, not a burn", features.output_type));
    }
    let sidechain_feature = features
        .sidechain_feature
        .as_ref()
        .ok_or("the burn output has no sidechain feature, so it names no claimant")?;
    let SideChainFeatureData::ConfidentialOutput(confidential_output) = &sidechain_feature.data else {
        return Err("the burn output's sidechain feature is not a confidential output".to_string());
    };

    Ok(MinotariBurnClaimProof {
        commitment: output.commitment.to_byte_type(),
        ownership_proof: proof.ownership_proof.to_byte_type(),
        value: proof.value,
        output: BurnOutput {
            version: output.version,
            features: BurnOutputFeatures {
                version: features.version.as_u8(),
                maturity: features.maturity,
                claim_public_key: confidential_output.claim_public_key.to_byte_type(),
                sidechain_id: sidechain_feature.sidechain_id().map(|id| BurnSidechainId {
                    public_key: id.public_key().to_byte_type(),
                    knowledge_proof: id.knowledge_proof().to_byte_type(),
                }),
                range_proof_type: features.range_proof_type.as_byte(),
            },
            rangeproof_hash: Hash32::from_array(output.rangeproof_hash.into_array()),
            script: bytes("script", output.script.clone())?,
            sender_offset_public_key: output.sender_offset_public_key.to_byte_type(),
            metadata_signature: bytes(
                "metadata signature",
                borsh::to_vec(&output.metadata_signature).map_err(|e| e.to_string())?,
            )?,
            covenant: bytes("covenant", output.covenant.clone())?,
            encrypted_data: bytes("encrypted data", output.encrypted_data.clone())?,
            minimum_value_promise: output.minimum_value_promise,
        },
        inclusion_proof: BurnOutputInclusionProof {
            block_hash: Hash32::from_array(block_hash.into_array()),
            normal_output_proof: mmr_proof(normal_output_proof),
            normal_output_mr: Hash32::from_array(normal_output_mr.into_array()),
            block_output_proof: mmr_proof(block_output_proof),
        },
    })
}

/// Claims burned L1 funds for the actor and deposits everything but a small
/// fee reserve into the public faucet. Only needed on local networks where
/// the faucet starts underfunded.
async fn cmd_bootstrap(rest: &[String]) {
    let state = DemoState::load();
    let name = rest.first().expect("usage: faucet-topup <actor> <proof.json>");
    let proof_path = rest.get(1).expect("usage: faucet-topup <actor> <proof.json>");
    let actor = state.actor(name).clone();
    let proof: BurnProofFile =
        serde_json::from_str(&std::fs::read_to_string(proof_path).expect("read proof file"))
            .expect("parse burn proof file");
    let claim_proof = claim_proof_from_l1(&proof.claim_proof).expect("convert L1 burn proof");
    let encrypted_data = EncryptedData::try_from(proof.encrypted_data.clone()).expect("encrypted data");

    let provider = actor_provider(&actor).await;
    let epoch = max_epoch(&provider).await;
    let (unsigned, sealer) = ClaimBurn::new(&provider, claim_proof, encrypted_data, epoch)
        .with_max_fee(2_000_000u64)
        .with_memo_message("faucet top-up")
        .prepare()
        .await
        .expect("prepare burn claim");
    let tx = TransactionRequest::default()
        .with_transaction(unsigned)
        .build(&sealer)
        .await
        .expect("seal claim transaction");
    let mut provider = actor_provider(&actor).await;
    let pending = provider.send_transaction(tx).await.expect("send claim");
    wait_for_tx(&pending).await;

    // The claimed funds live in a stealth UTXO. The receipt may up several
    // UTXOs (e.g. the revealed fee change); collect them all and spend the
    // one that still exists and balances.
    let receipt = pending.get_receipt().await.expect("claim receipt");
    let claim_utxos: Vec<_> = receipt
        .diff_summary()
        .upped
        .iter()
        .filter_map(|s| s.substate_id.as_utxo_address())
        .collect();
    // The claim paid its fee out of the claimed output, so the UTXO holds
    // the value minus the fees actually charged.
    let fees_paid = receipt.fee_receipt().total_fees_paid();
    drop(receipt);
    let claimed = proof
        .claim_proof
        .value
        .checked_sub(fees_paid)
        .expect("fees exceed the claimed value");
    println!(
        "=> claimed {} µT (fee {}) into {} upped UTXO(s)",
        claimed, fees_paid, claim_utxos.len()
    );

    // Split the UTXO into a fee fund and the deposit, and reveal the split fee.
    const SPLIT_FEE: u64 = 200_000;
    const FEE_FUND: u64 = 1_000_000;
    let deposit_amount = claimed - SPLIT_FEE - FEE_FUND;
    let address = provider.default_signer_address().clone();

    let epoch = max_epoch(&provider).await;
    let build_split = |utxo: &_| {
        StealthTransfer::new(TARI_TOKEN, &provider)
            .spend_stealth_input(address.clone(), utxo)
            .to_revealed_output(SPLIT_FEE)
            .to_stealth_output(Output::new(address.clone(), TARI_TOKEN, std::num::NonZeroU64::new(FEE_FUND).expect("non-zero")))
            .to_stealth_output(Output::new(address.clone(), TARI_TOKEN, std::num::NonZeroU64::new(deposit_amount).expect("non-zero")))
            .prepare()
    };
    let mut prepared = None;
    for utxo in &claim_utxos {
        match build_split(utxo).await {
            Ok(p) => {
                prepared = Some((p.0, p.1, utxo.clone()));
                break;
            },
            Err(err) => println!("   (skipping a claim-receipt UTXO: {err})"),
        }
    }
    let (split, split_signers, spent_utxo) = prepared.expect("no spendable claim UTXO");
    let outputs = split.stealth_outputs().to_vec();
    let unsigned = Transaction::builder(network(), epoch)
        .with_fee_instructions_builder(|b| {
            b.stealth_transfer(TARI_TOKEN, split)
                .put_last_instruction_output_on_workspace("fee")
                .pay_fee_from_bucket("fee")
        })
        .add_input(spent_utxo)
        .build_unsigned();
    let authorizer = provider.wallet().stealth_authorizer(split_signers);
    let tx = TransactionRequest::default()
        .with_transaction(unsigned)
        .build(&authorizer)
        .await
        .expect("seal split");
    let pending = provider.send_transaction(tx).await.expect("send split");
    wait_for_tx(&pending).await;
    println!("=> split into fee fund + deposit fund");

    // Local networks have no builtin faucet, so fund the demo actors
    // straight from the claimed UTXO: carve a fund per actor, then create
    // each account and deposit the fund into it in one transaction.
    const FUND_TARI: u64 = 500;
    const TX_FEE: u64 = 200_000;
    let fund = FUND_TARI * TARI;
    let mut rest_amount = deposit_amount;
    let mut rest_utxo = outputs[1].commitment().clone();
    let mut fee_bank = outputs[0].commitment().clone();
    let mut fee_bank_amount = FEE_FUND;

    for actor in state.actors.clone() {
        if &actor.name == name {
            // The bridging actor itself is the source of funds, not a target.
            continue;
        }
        // Carve: pay the fee and split off the actor's fund.
        let epoch = max_epoch(&provider).await;
        let (carve, carve_signers) = StealthTransfer::new(TARI_TOKEN, &provider)
            .spend_stealth_input(address.clone(), &rest_utxo)
            .to_revealed_output(TX_FEE)
            .to_stealth_output(Output::new(address.clone(), TARI_TOKEN, std::num::NonZeroU64::new(fund).expect("non-zero")))
            .to_stealth_output(Output::new(address.clone(), TARI_TOKEN, std::num::NonZeroU64::new(rest_amount - fund - TX_FEE).expect("non-zero")))
            .prepare()
            .await
            .expect("prepare carve");
        let carve_outs = carve.stealth_outputs().to_vec();
        let unsigned = Transaction::builder(network(), epoch)
            .with_fee_instructions_builder(|b| {
                b.stealth_transfer(TARI_TOKEN, carve)
                    .put_last_instruction_output_on_workspace("fee")
                    .pay_fee_from_bucket("fee")
            })
            .add_input(UtxoAddress::new(TARI_TOKEN, rest_utxo.clone().into()))
            .build_unsigned();
        let authorizer = provider.wallet().stealth_authorizer(carve_signers);
        let tx = TransactionRequest::default()
            .with_transaction(unsigned)
            .build(&authorizer)
            .await
            .expect("seal carve");
        let pending = provider.send_transaction(tx).await.expect("send carve");
        wait_for_tx(&pending).await;
        rest_amount -= fund + TX_FEE;
        rest_utxo = carve_outs[1].commitment().clone();
        let fund_utxo = carve_outs[0].commitment().clone();

        // Fund: create the actor's account and deposit the fund into it.
        let epoch = max_epoch(&provider).await;
        let (fee_stmt, fee_signers) = StealthTransfer::new(TARI_TOKEN, &provider)
            .spend_stealth_input(address.clone(), &fee_bank)
            .to_revealed_output(TX_FEE)
            .to_stealth_output(Output::new(address.clone(), TARI_TOKEN, std::num::NonZeroU64::new(fee_bank_amount - TX_FEE).expect("non-zero")))
            .prepare()
            .await
            .expect("prepare fund fee statement");
        let fee_outs = fee_stmt.stealth_outputs().to_vec();
        let (fund_stmt, fund_signers) = StealthTransfer::new(TARI_TOKEN, &provider)
            .spend_stealth_input(address.clone(), &fund_utxo)
            .to_revealed_output(fund)
            .prepare()
            .await
            .expect("prepare fund statement");
        let pk: ootle_rs::template_types::crypto::RistrettoPublicKeyBytes = actor.public_key().to_byte_type();
        let unsigned = Transaction::builder(network(), epoch)
            .with_fee_instructions_builder(|b| {
                b.stealth_transfer(TARI_TOKEN, fee_stmt)
                    .put_last_instruction_output_on_workspace("txfee")
                    .pay_fee_from_bucket("txfee")
            })
            .stealth_transfer(TARI_TOKEN, fund_stmt)
            .put_last_instruction_output_on_workspace("pot")
            .create_account(pk)
            .put_last_instruction_output_on_workspace("acc")
            .call_method("acc", "deposit", args![Workspace("pot")])
            .add_input(UtxoAddress::new(TARI_TOKEN, fee_bank.into()))
            .add_input(UtxoAddress::new(TARI_TOKEN, fund_utxo.into()))
            .build_unsigned();
        let authorizer = provider.wallet().stealth_authorizer(fee_signers.merge(fund_signers));
        let tx = TransactionRequest::default()
            .with_transaction(unsigned)
            .build(&authorizer)
            .await
            .expect("seal fund");
        let pending = provider.send_transaction(tx).await.expect("send fund");
        wait_for_tx(&pending).await;
        fee_bank = fee_outs[0].commitment().clone();
        fee_bank_amount -= TX_FEE;
        println!("=> funded {} with {} µT", actor.name, fund);
    }
    println!("All actors funded. Run `setup` to publish the template and open the auction.");
}

/// Claims a burn and deposits (almost) everything into the public XTR faucet.
async fn cmd_faucet_refill(rest: &[String]) {
    let state = DemoState::load();
    let name = rest.first().expect("usage: faucet-refill <actor> <tari> <proof.json>");
    let refill_tari: u64 = rest.get(1).expect("usage: faucet-refill <actor> <tari> <proof.json>").parse().expect("tari amount");
    let proof_path = rest.get(2).expect("usage: faucet-refill <actor> <tari> <proof.json>");
    let actor = state.actor(name).clone();
    let proof: BurnProofFile =
        serde_json::from_str(&std::fs::read_to_string(proof_path).expect("read proof file"))
            .expect("parse burn proof file");
    let claim_proof = claim_proof_from_l1(&proof.claim_proof).expect("convert L1 burn proof");
    let encrypted_data = EncryptedData::try_from(proof.encrypted_data.clone()).expect("encrypted data");

    let provider = actor_provider(&actor).await;
    let epoch = max_epoch(&provider).await;
    let (unsigned, sealer) = ClaimBurn::new(&provider, claim_proof, encrypted_data, epoch)
        .with_max_fee(2_000_000u64)
        .with_memo_message("faucet refill")
        .prepare()
        .await
        .expect("prepare burn claim");
    let tx = TransactionRequest::default()
        .with_transaction(unsigned)
        .build(&sealer)
        .await
        .expect("seal claim transaction");
    let mut provider = actor_provider(&actor).await;
    let pending = provider.send_transaction(tx).await.expect("send claim");
    wait_for_tx(&pending).await;

    let receipt = pending.get_receipt().await.expect("claim receipt");
    let fees_paid = receipt.fee_receipt().total_fees_paid();
    let claimed = proof.claim_proof.value.checked_sub(fees_paid).expect("fees exceed value");
    let claim_utxos: Vec<_> = receipt
        .diff_summary()
        .upped
        .iter()
        .filter_map(|s| s.substate_id.as_utxo_address())
        .collect();
    drop(receipt);
    println!("=> claimed {} µT into {} UTXO(s)", claimed, claim_utxos.len());

    const SPLIT_FEE: u64 = 200_000;
    const FEE_FUND: u64 = 1_000_000;
    let refill_amount = refill_tari * TARI;
    assert!(claimed >= refill_amount + SPLIT_FEE + FEE_FUND, "not enough claimed for the refill");
    let address = provider.default_signer_address().clone();

    // Split the claim into a fee fund, the refill and the remainder.
    let epoch = max_epoch(&provider).await;
    let build_split = |utxo: &_| {
        StealthTransfer::new(TARI_TOKEN, &provider)
            .spend_stealth_input(address.clone(), utxo)
            .to_revealed_output(SPLIT_FEE)
            .to_stealth_output(Output::new(address.clone(), TARI_TOKEN, std::num::NonZeroU64::new(FEE_FUND).expect("non-zero")))
            .to_stealth_output(Output::new(address.clone(), TARI_TOKEN, std::num::NonZeroU64::new(refill_amount).expect("non-zero")))
            .prepare()
    };
    let mut prepared = None;
    for utxo in &claim_utxos {
        match build_split(utxo).await {
            Ok(p) => {
                prepared = Some((p.0, p.1, utxo.clone()));
                break;
            },
            Err(err) => println!("   (skipping a claim-receipt UTXO: {err})"),
        }
    }
    let (split, split_signers, spent_utxo) = prepared.expect("no spendable claim UTXO");
    let outputs = split.stealth_outputs().to_vec();
    let unsigned = Transaction::builder(network(), epoch)
        .with_fee_instructions_builder(|b| {
            b.stealth_transfer(TARI_TOKEN, split)
                .put_last_instruction_output_on_workspace("fee")
                .pay_fee_from_bucket("fee")
        })
        .add_input(spent_utxo)
        .build_unsigned();
    let authorizer = provider.wallet().stealth_authorizer(split_signers);
    let tx = TransactionRequest::default()
        .with_transaction(unsigned)
        .build(&authorizer)
        .await
        .expect("seal split");
    let pending = provider.send_transaction(tx).await.expect("send split");
    wait_for_tx(&pending).await;
    println!("=> split ready");

    // Pay fees from the fee fund, deposit the refill into the public faucet.
    const REFILL_FEE: u64 = 200_000;
    let epoch = max_epoch(&provider).await;
    let (fee_stmt, fee_signers) = StealthTransfer::new(TARI_TOKEN, &provider)
        .spend_stealth_input(address.clone(), outputs[0].commitment())
        .to_revealed_output(REFILL_FEE)
        .to_stealth_output(Output::new(address.clone(), TARI_TOKEN, std::num::NonZeroU64::new(FEE_FUND - REFILL_FEE).expect("non-zero")))
        .prepare()
        .await
        .expect("prepare fee statement");
    let (dep_stmt, dep_signers) = StealthTransfer::new(TARI_TOKEN, &provider)
        .spend_stealth_input(address.clone(), outputs[1].commitment())
        .to_revealed_output(refill_amount)
        .prepare()
        .await
        .expect("prepare deposit statement");
    let unsigned = Transaction::builder(network(), epoch)
        .with_fee_instructions_builder(|b| {
            b.stealth_transfer(TARI_TOKEN, fee_stmt)
                .put_last_instruction_output_on_workspace("txfee")
                .pay_fee_from_bucket("txfee")
        })
        .stealth_transfer(TARI_TOKEN, dep_stmt)
        .put_last_instruction_output_on_workspace("pot")
        .call_method(XTR_FAUCET_COMPONENT_ADDRESS, "deposit", args![Workspace("pot")])
        .add_input(UtxoAddress::new(TARI_TOKEN, outputs[0].commitment().into()))
        .add_input(UtxoAddress::new(TARI_TOKEN, outputs[1].commitment().into()))
        .build_unsigned();
    let authorizer = provider.wallet().stealth_authorizer(fee_signers.merge(dep_signers));
    let tx = TransactionRequest::default()
        .with_transaction(unsigned)
        .build(&authorizer)
        .await
        .expect("seal deposit");
    let pending = provider.send_transaction(tx).await.expect("send deposit");
    wait_for_tx(&pending).await;
    println!("Public faucet refilled with {} µT.", refill_amount);
}

/// Resumes funding from an already-claimed UTXO whose commitment is known
/// (e.g. extracted from a committed claim receipt after a watch timeout).
async fn cmd_fund_utxo(rest: &[String]) {
    let state = DemoState::load();
    let name = rest.first().expect("usage: fund-utxo <actor> <commitment-hex> <claimed-microtari>");
    let commitment_hex = rest.get(1).expect("usage: fund-utxo <actor> <commitment-hex> <claimed-microtari>");
    let claimed: u64 = rest.get(2).expect("usage: fund-utxo <actor> <commitment-hex> <claimed-microtari>").parse().expect("microtari amount");
    let actor = state.actor(name).clone();
    let mut provider = actor_provider(&actor).await;
    let address = provider.default_signer_address().clone();

    let commitment = ootle_rs::template_types::crypto::PedersenCommitmentBytes::from_hex(commitment_hex)
        .expect("commitment hex");
    let utxo = UtxoAddress::new(TARI_TOKEN, commitment.into());

    const SPLIT_FEE: u64 = 200_000;
    const FEE_FUND: u64 = 1_000_000;
    let deposit_amount = claimed - SPLIT_FEE - FEE_FUND;
    let epoch = max_epoch(&provider).await;
    let (split, split_signers) = StealthTransfer::new(TARI_TOKEN, &provider)
        .spend_stealth_input(address.clone(), &utxo)
        .to_revealed_output(SPLIT_FEE)
        .to_stealth_output(Output::new(address.clone(), TARI_TOKEN, std::num::NonZeroU64::new(FEE_FUND).expect("non-zero")))
        .to_stealth_output(Output::new(address.clone(), TARI_TOKEN, std::num::NonZeroU64::new(deposit_amount).expect("non-zero")))
        .prepare()
        .await
        .expect("prepare split statement");
    let outputs = split.stealth_outputs().to_vec();
    let unsigned = Transaction::builder(network(), epoch)
        .with_fee_instructions_builder(|b| {
            b.stealth_transfer(TARI_TOKEN, split)
                .put_last_instruction_output_on_workspace("fee")
                .pay_fee_from_bucket("fee")
        })
        .add_input(utxo)
        .build_unsigned();
    let authorizer = provider.wallet().stealth_authorizer(split_signers);
    let tx = TransactionRequest::default()
        .with_transaction(unsigned)
        .build(&authorizer)
        .await
        .expect("seal split");
    let pending = provider.send_transaction(tx).await.expect("send split");
    wait_for_tx(&pending).await;
    println!("=> split into fee fund + deposit fund");

    const FUND_TARI: u64 = 500;
    const TX_FEE: u64 = 200_000;
    let fund = FUND_TARI * TARI;
    let mut rest_amount = deposit_amount;
    let mut rest_utxo = outputs[1].commitment().clone();
    let mut fee_bank = outputs[0].commitment().clone();
    let mut fee_bank_amount = FEE_FUND;

    for other in state.actors.clone() {
        if &other.name == name {
            continue;
        }
        let epoch = max_epoch(&provider).await;
        let (carve, carve_signers) = StealthTransfer::new(TARI_TOKEN, &provider)
            .spend_stealth_input(address.clone(), &rest_utxo)
            .to_revealed_output(TX_FEE)
            .to_stealth_output(Output::new(address.clone(), TARI_TOKEN, std::num::NonZeroU64::new(fund).expect("non-zero")))
            .to_stealth_output(Output::new(address.clone(), TARI_TOKEN, std::num::NonZeroU64::new(rest_amount - fund - TX_FEE).expect("non-zero")))
            .prepare()
            .await
            .expect("prepare carve");
        let carve_outs = carve.stealth_outputs().to_vec();
        let unsigned = Transaction::builder(network(), epoch)
            .with_fee_instructions_builder(|b| {
                b.stealth_transfer(TARI_TOKEN, carve)
                    .put_last_instruction_output_on_workspace("fee")
                    .pay_fee_from_bucket("fee")
            })
            .add_input(UtxoAddress::new(TARI_TOKEN, rest_utxo.clone().into()))
            .build_unsigned();
        let authorizer = provider.wallet().stealth_authorizer(carve_signers);
        let tx = TransactionRequest::default()
            .with_transaction(unsigned)
            .build(&authorizer)
            .await
            .expect("seal carve");
        let pending = provider.send_transaction(tx).await.expect("send carve");
        wait_for_tx(&pending).await;
        rest_amount -= fund + TX_FEE;
        rest_utxo = carve_outs[1].commitment().clone();
        let fund_utxo = carve_outs[0].commitment().clone();

        let epoch = max_epoch(&provider).await;
        let (fee_stmt, fee_signers) = StealthTransfer::new(TARI_TOKEN, &provider)
            .spend_stealth_input(address.clone(), &fee_bank)
            .to_revealed_output(TX_FEE)
            .to_stealth_output(Output::new(address.clone(), TARI_TOKEN, std::num::NonZeroU64::new(fee_bank_amount - TX_FEE).expect("non-zero")))
            .prepare()
            .await
            .expect("prepare fund fee statement");
        let fee_outs = fee_stmt.stealth_outputs().to_vec();
        let (fund_stmt, fund_signers) = StealthTransfer::new(TARI_TOKEN, &provider)
            .spend_stealth_input(address.clone(), &fund_utxo)
            .to_revealed_output(fund)
            .prepare()
            .await
            .expect("prepare fund statement");
        let pk: ootle_rs::template_types::crypto::RistrettoPublicKeyBytes = other.public_key().to_byte_type();
        let unsigned = Transaction::builder(network(), epoch)
            .with_fee_instructions_builder(|b| {
                b.stealth_transfer(TARI_TOKEN, fee_stmt)
                    .put_last_instruction_output_on_workspace("txfee")
                    .pay_fee_from_bucket("txfee")
            })
            .stealth_transfer(TARI_TOKEN, fund_stmt)
            .put_last_instruction_output_on_workspace("pot")
            .create_account(pk)
            .put_last_instruction_output_on_workspace("acc")
            .call_method("acc", "deposit", args![Workspace("pot")])
            .add_input(UtxoAddress::new(TARI_TOKEN, fee_bank.into()))
            .add_input(UtxoAddress::new(TARI_TOKEN, fund_utxo.into()))
            .build_unsigned();
        let authorizer = provider.wallet().stealth_authorizer(fee_signers.merge(fund_signers));
        let tx = TransactionRequest::default()
            .with_transaction(unsigned)
            .build(&authorizer)
            .await
            .expect("seal fund");
        let pending = provider.send_transaction(tx).await.expect("send fund");
        wait_for_tx(&pending).await;
        fee_bank = fee_outs[0].commitment().clone();
        fee_bank_amount -= TX_FEE;
        println!("=> funded {} with {} µT", other.name, fund);
    }
    println!("All actors funded. Run `setup` to publish the template and open the auction.");
}

/// Prints each actor's L2 address and derived account component address.
async fn cmd_addresses() {
    let state = DemoState::load();
    for actor in &state.actors {
        let provider = actor_provider(actor).await;
        let account = provider.default_signer_address().to_account_address();
        println!("{:12} {}", actor.name, provider.default_signer_address());
        println!("{:12} account: {account}", "");
    }
}

/// Like wait_for_tx, but reports failures as Err instead of panicking, so a
/// bot can continue with the remaining bids when one was already revealed.
async fn try_wait_tx(pending: &PendingTransaction) -> Result<(), String> {
    if let Ok(outcome) = pending.watch().await {
        if outcome.is_reject() {
            return Err(format!("{outcome:?}"));
        }
        return Ok(());
    }
    for _ in 0..35 {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        if let Ok(receipt) = pending.get_receipt().await {
            use tari_engine_types::transaction_receipt::FinalizeOutcome;
            match receipt.outcome() {
                FinalizeOutcome::Commit => return Ok(()),
                FinalizeOutcome::FeeIntentCommit => continue,
            }
        }
    }
    Err("never finalized within the polling window".to_string())
}
