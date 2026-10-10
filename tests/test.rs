use tari_utilities::byte_array::ByteArray;
use tari_template_lib::types::constants::{NFT_FAUCET_COMPONENT_ADDRESS, TARI_TOKEN};
use tari_template_lib::types::{Amount, ComponentAddress, Hash32, NonFungibleAddress, ResourceAddress};
use tari_template_test_tooling::crypto::{PublicKey, RistrettoPublicKey, RistrettoSecretKey};
use tari_template_test_tooling::engine_types::virtual_substate::{VirtualSubstate, VirtualSubstateId};
use tari_template_test_tooling::support::assert_error::assert_reject_reason;
use tari_template_test_tooling::transaction::args;
use tari_template_test_tooling::TemplateTest;

fn bond() -> Amount { Amount::from_integer(10u64) }
const BIDDING_EPOCHS: u64 = 10;
const REVEAL_EPOCHS: u64 = 5;

#[derive(Clone)]
struct Bidder {
    component: ComponentAddress,
    owner_token: NonFungibleAddress,
    key: RistrettoSecretKey,
    pk: RistrettoPublicKey,
}

fn bidder(test: &mut TemplateTest) -> Bidder {
    let (component, owner_token, key) = test.create_funded_account();
    let pk = RistrettoPublicKey::from_secret_key(&key);
    Bidder {
        component,
        owner_token,
        key,
        pk,
    }
}

/// The commitment the template expects: blake2b-256(pk || blinding || amount_le).
/// Computed locally: the template's intrinsics only run inside the WASM engine.
fn commitment_for(pk: &RistrettoPublicKey, blinding: &[u8], amount: Amount) -> Hash32 {
    use blake2::digest::{Update, VariableOutput};
    use blake2::Blake2bVar;

    let mut hasher = Blake2bVar::new(32).expect("32-byte Blake2b output");
    hasher.update(pk.as_bytes());
    hasher.update(blinding);
    hasher.update(&amount.to_le_bytes());
    let mut digest = [0u8; 32];
    hasher.finalize_variable(&mut digest).expect("infallible");
    Hash32::try_from_slice(&digest).expect("32 bytes")
}

fn set_epoch(test: &mut TemplateTest, epoch: u64) {
    test.set_virtual_substate(
        VirtualSubstateId::CurrentEpoch,
        VirtualSubstate::CurrentEpoch(epoch),
    );
}

fn create_auction(test: &mut TemplateTest, seller: &Bidder, min_bid: Option<Amount>) -> (ComponentAddress, ResourceAddress) {
    let template = test.get_template_address("SealedBidAuction");

    let result = test.execute_expect_success(
        test.transaction()
            .call_method(
                NFT_FAUCET_COMPONENT_ADDRESS,
                "mint",
                args![Amount::from_integer(1u64), tari_bor::Value::Null],
            )
            .put_last_instruction_output_on_workspace("lot")
            .call_function(
                template,
                "new",
                args![
                    Workspace("lot"),
                    seller.component,
                    min_bid,
                    bond(),
                    BIDDING_EPOCHS,
                    REVEAL_EPOCHS,
                ],
            )
            .build_and_seal(&seller.key),
        vec![seller.owner_token.clone()],
    );
    // [0] = mint, [1] = put_last_instruction_output_on_workspace, [2] = new.
    let auction = result.finalize.execution_results[2]
        .decode::<ComponentAddress>()
        .unwrap();

    // Ask the auction itself which resource the lot is.
    let result = test.execute_expect_success(
        test.transaction()
            .call_method(auction, "lot_resource", args![])
            .build_and_seal(&seller.key),
        vec![seller.owner_token.clone()],
    );
    let lot_resource = result.finalize.execution_results[0]
        .decode::<ResourceAddress>()
        .unwrap();
    (auction, lot_resource)
}

fn place_bid(test: &mut TemplateTest, auction: ComponentAddress, who: &Bidder, commitment: Hash32) {
    test.execute_expect_success(
        test.transaction()
            .call_method(who.component, "withdraw", args![TARI_TOKEN, bond()])
            .put_last_instruction_output_on_workspace("bond")
            .call_method(
                auction,
                "place_bid",
                args![commitment, who.component, Workspace("bond")],
            )
            .build_and_seal(&who.key),
        vec![who.owner_token.clone()],
    );
}

fn reveal(test: &mut TemplateTest, auction: ComponentAddress, who: &Bidder, blinding: Vec<u8>, amount: Amount) {
    test.execute_expect_success(
        test.transaction()
            .call_method(who.component, "withdraw", args![TARI_TOKEN, amount])
            .put_last_instruction_output_on_workspace("payment")
            .call_method(
                auction,
                "reveal",
                args![blinding.clone(), amount, Workspace("payment")],
            )
            .build_and_seal(&who.key),
        vec![who.owner_token.clone()],
    );
}

fn settle(test: &mut TemplateTest, auction: ComponentAddress, who: &Bidder) {
    test.execute_expect_success(
        test.transaction()
            .call_method(auction, "settle", args![])
            .build_and_seal(&who.key),
        vec![who.owner_token.clone()],
    );
}

fn balance(test: &mut TemplateTest, who: &Bidder, resource: &ResourceAddress) -> Amount {
    let result = test.execute_expect_success(
        test.transaction()
            .call_method(who.component, "balance", args![resource])
            .build_and_seal(&who.key),
        vec![who.owner_token.clone()],
    );
    result.finalize.execution_results[0]
        .decode::<Amount>()
        .unwrap()
}

fn tari_balance(test: &mut TemplateTest, who: &Bidder) -> Amount {
    balance(test, who, &TARI_TOKEN)
}

fn sealed_bid_count(test: &mut TemplateTest, auction: ComponentAddress, who: &Bidder) -> u64 {
    let result = test.execute_expect_success(
        test.transaction()
            .call_method(auction, "sealed_bid_count", args![])
            .build_and_seal(&who.key),
        vec![who.owner_token.clone()],
    );
    result.finalize.execution_results[0].decode::<u64>().unwrap()
}

fn winning_bid(test: &mut TemplateTest, auction: ComponentAddress, who: &Bidder) -> Option<Amount> {
    let result = test.execute_expect_success(
        test.transaction()
            .call_method(auction, "winning_bid", args![])
            .build_and_seal(&who.key),
        vec![who.owner_token.clone()],
    );
    result.finalize.execution_results[0]
        .decode::<Option<Amount>>()
        .unwrap()
}

#[test]
fn full_flow_three_bidders_one_ghost() {
    let mut test = TemplateTest::my_crate();
    let seller = bidder(&mut test);
    let (auction, lot_resource) = create_auction(&mut test, &seller, None);

    let b1 = bidder(&mut test);
    let b2 = bidder(&mut test);
    let ghost = bidder(&mut test);

    let b1_blinding = vec![1u8; 32];
    let b2_blinding = vec![2u8; 32];
    let ghost_blinding = vec![3u8; 32];

    let b1_start = tari_balance(&mut test, &b1);
    let _b2_start = tari_balance(&mut test, &b2);
    let ghost_start = tari_balance(&mut test, &ghost);
    let seller_start = tari_balance(&mut test, &seller);

    // Sealing phase: three sealed bids, no amounts anywhere on-chain.
    place_bid(
        &mut test,
        auction,
        &b1,
        commitment_for(&b1.pk, &b1_blinding, Amount::from_integer(100u64)),
    );
    place_bid(
        &mut test,
        auction,
        &b2,
        commitment_for(&b2.pk, &b2_blinding, Amount::from_integer(200u64)),
    );
    place_bid(
        &mut test,
        auction,
        &ghost,
        commitment_for(&ghost.pk, &ghost_blinding, Amount::from_integer(999u64)),
    );

    assert_eq!(sealed_bid_count(&mut test, auction, &b1), 3);
    // Before settlement the winning bid does not exist.
    assert_eq!(winning_bid(&mut test, auction, &b1), None);

    // Midnight passes, the reveal window opens.
    set_epoch(&mut test, BIDDING_EPOCHS + 2);
    reveal(&mut test, auction, &b1, b1_blinding.clone(), Amount::from_integer(100u64));
    reveal(&mut test, auction, &b2, b2_blinding.clone(), Amount::from_integer(200u64));
    // The ghost stays silent.

    // The reveal window closes.
    set_epoch(&mut test, BIDDING_EPOCHS + REVEAL_EPOCHS + 1);
    settle(&mut test, auction, &seller);

    // b2 wins and gets the lot.
    assert_eq!(balance(&mut test, &b2, &lot_resource), Amount::from_integer(1u64));
    // The seller is paid the winning bid plus the ghost's forfeited bond.
    assert_eq!(
        tari_balance(&mut test, &seller),
        seller_start + Amount::from_integer(200u64) + bond()
    );
    // The losing bid is refunded.
    assert_eq!(tari_balance(&mut test, &b1), b1_start - Amount::from_integer(100u64) + Amount::from_integer(100u64) - bond() + bond());
    // The ghost's bond was forfeited to the seller.
    assert_eq!(tari_balance(&mut test, &ghost), ghost_start - bond());
    // The winning bid is now public.
    assert_eq!(winning_bid(&mut test, auction, &b1), Some(Amount::from_integer(200u64)));
}

#[test]
fn exact_tie_is_won_by_earlier_reveal() {
    let mut test = TemplateTest::my_crate();
    let seller = bidder(&mut test);
    let (auction, lot_resource) = create_auction(&mut test, &seller, None);

    let early = bidder(&mut test);
    let late = bidder(&mut test);

    place_bid(
        &mut test,
        auction,
        &early,
        commitment_for(&early.pk, &[7u8; 32], Amount::from_integer(500u64)),
    );
    place_bid(
        &mut test,
        auction,
        &late,
        commitment_for(&late.pk, &[8u8; 32], Amount::from_integer(500u64)),
    );

    set_epoch(&mut test, BIDDING_EPOCHS + 1);
    reveal(&mut test, auction, &early, vec![7u8; 32], Amount::from_integer(500u64));
    reveal(&mut test, auction, &late, vec![8u8; 32], Amount::from_integer(500u64));

    set_epoch(&mut test, BIDDING_EPOCHS + REVEAL_EPOCHS + 1);
    settle(&mut test, auction, &seller);

    assert_eq!(balance(&mut test, &early, &lot_resource), Amount::from_integer(1u64));
    assert_eq!(balance(&mut test, &late, &lot_resource), Amount::from_integer(0u64));
}

#[test]
fn auction_with_only_ghosts_returns_lot_and_burns_all_bonds() {
    let mut test = TemplateTest::my_crate();
    let seller = bidder(&mut test);
    let (auction, lot_resource) = create_auction(&mut test, &seller, None);

    let ghost = bidder(&mut test);
    let ghost_start = tari_balance(&mut test, &ghost);
    place_bid(
        &mut test,
        auction,
        &ghost,
        commitment_for(&ghost.pk, &[9u8; 32], Amount::from_integer(123u64)),
    );

    set_epoch(&mut test, BIDDING_EPOCHS + REVEAL_EPOCHS + 1);
    settle(&mut test, auction, &seller);

    assert_eq!(balance(&mut test, &seller, &lot_resource), Amount::from_integer(1u64));
    assert_eq!(tari_balance(&mut test, &ghost), ghost_start - bond());
}

#[test]
fn rejects_reveal_that_does_not_match_the_seal() {
    let mut test = TemplateTest::my_crate();
    let seller = bidder(&mut test);
    let (auction, _) = create_auction(&mut test, &seller, None);

    let b1 = bidder(&mut test);
    place_bid(
        &mut test,
        auction,
        &b1,
        commitment_for(&b1.pk, &[1u8; 32], Amount::from_integer(100u64)),
    );

    set_epoch(&mut test, BIDDING_EPOCHS + 1);
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(b1.component, "withdraw", args![TARI_TOKEN, Amount::from_integer(100u64)])
            .put_last_instruction_output_on_workspace("payment")
            .call_method(
                auction,
                "reveal",
                args![vec![42u8; 32], Amount::from_integer(100u64), Workspace("payment")],
            )
            .build_and_seal(&b1.key),
        vec![b1.owner_token.clone()],
    );
    assert_reject_reason(reason, "The reveal does not match the seal");
}

#[test]
fn rejects_bids_and_reveals_outside_their_windows() {
    let mut test = TemplateTest::my_crate();
    let seller = bidder(&mut test);
    let (auction, _) = create_auction(&mut test, &seller, None);

    let b1 = bidder(&mut test);

    // Too early to reveal: the sealing phase is still open.
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(b1.component, "withdraw", args![TARI_TOKEN, Amount::from_integer(1u64)])
            .put_last_instruction_output_on_workspace("payment")
            .call_method(
                auction,
                "reveal",
                args![vec![1u8; 32], Amount::from_integer(1u64), Workspace("payment")],
            )
            .build_and_seal(&b1.key),
        vec![b1.owner_token.clone()],
    );
    assert_reject_reason(reason, "The reveal window is not open yet");

    // Too late to seal a bid.
    set_epoch(&mut test, BIDDING_EPOCHS);
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(b1.component, "withdraw", args![TARI_TOKEN, bond()])
            .put_last_instruction_output_on_workspace("bond")
            .call_method(
                auction,
                "place_bid",
                args![
                    commitment_for(&b1.pk, &[1u8; 32], Amount::from_integer(1u64)),
                    b1.component,
                    Workspace("bond")
                ],
            )
            .build_and_seal(&b1.key),
        vec![b1.owner_token.clone()],
    );
    assert_reject_reason(reason, "The sealing phase is closed");

    // Too late to reveal.
    set_epoch(&mut test, BIDDING_EPOCHS + REVEAL_EPOCHS);
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(b1.component, "withdraw", args![TARI_TOKEN, Amount::from_integer(1u64)])
            .put_last_instruction_output_on_workspace("payment")
            .call_method(
                auction,
                "reveal",
                args![vec![1u8; 32], Amount::from_integer(1u64), Workspace("payment")],
            )
            .build_and_seal(&b1.key),
        vec![b1.owner_token.clone()],
    );
    assert_reject_reason(reason, "The reveal window is closed");
}

#[test]
fn rejects_double_reveal_and_wrong_bond() {
    let mut test = TemplateTest::my_crate();
    let seller = bidder(&mut test);
    let (auction, _) = create_auction(&mut test, &seller, None);

    let b1 = bidder(&mut test);
    place_bid(
        &mut test,
        auction,
        &b1,
        commitment_for(&b1.pk, &[1u8; 32], Amount::from_integer(100u64)),
    );

    // A second bid from the same signer is rejected.
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(b1.component, "withdraw", args![TARI_TOKEN, bond()])
            .put_last_instruction_output_on_workspace("bond")
            .call_method(
                auction,
                "place_bid",
                args![
                    commitment_for(&b1.pk, &[1u8; 32], Amount::from_integer(100u64)),
                    b1.component,
                    Workspace("bond")
                ],
            )
            .build_and_seal(&b1.key),
        vec![b1.owner_token.clone()],
    );
    assert_reject_reason(reason, "This bidder already sealed a bid");

    // A bond of the wrong size is rejected.
    let b2 = bidder(&mut test);
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(b2.component, "withdraw", args![TARI_TOKEN, bond() + Amount::from_integer(1u64)])
            .put_last_instruction_output_on_workspace("bond")
            .call_method(
                auction,
                "place_bid",
                args![
                    commitment_for(&b2.pk, &[2u8; 32], Amount::from_integer(100u64)),
                    b2.component,
                    Workspace("bond")
                ],
            )
            .build_and_seal(&b2.key),
        vec![b2.owner_token.clone()],
    );
    assert_reject_reason(reason, "The bond must exactly match the required bond amount");

    // Reveal the honest bid, then try to reveal again.
    set_epoch(&mut test, BIDDING_EPOCHS + 1);
    reveal(&mut test, auction, &b1, vec![1u8; 32], Amount::from_integer(100u64));
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(b1.component, "withdraw", args![TARI_TOKEN, Amount::from_integer(100u64)])
            .put_last_instruction_output_on_workspace("payment")
            .call_method(
                auction,
                "reveal",
                args![vec![1u8; 32], Amount::from_integer(100u64), Workspace("payment")],
            )
            .build_and_seal(&b1.key),
        vec![b1.owner_token.clone()],
    );
    assert_reject_reason(reason, "This bid was already revealed");
}

#[test]
fn rejects_reveal_below_the_reserve() {
    let mut test = TemplateTest::my_crate();
    let seller = bidder(&mut test);
    let (auction, _) = create_auction(&mut test, &seller, Some(Amount::from_integer(150u64)));

    let b1 = bidder(&mut test);
    place_bid(
        &mut test,
        auction,
        &b1,
        commitment_for(&b1.pk, &[1u8; 32], Amount::from_integer(100u64)),
    );

    set_epoch(&mut test, BIDDING_EPOCHS + 1);
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(b1.component, "withdraw", args![TARI_TOKEN, Amount::from_integer(100u64)])
            .put_last_instruction_output_on_workspace("payment")
            .call_method(
                auction,
                "reveal",
                args![vec![1u8; 32], Amount::from_integer(100u64), Workspace("payment")],
            )
            .build_and_seal(&b1.key),
        vec![b1.owner_token.clone()],
    );
    assert_reject_reason(reason, "The revealed bid is below the reserve");
}
