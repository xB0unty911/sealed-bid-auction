use tari_template_lib::types::constants::{NFT_FAUCET_COMPONENT_ADDRESS, TARI_TOKEN};
use tari_template_lib::types::{Amount, ComponentAddress, Hash32, NonFungibleAddress, ResourceAddress};
use tari_template_test_tooling::crypto::{PublicKey, RistrettoPublicKey, RistrettoSecretKey};
use tari_template_test_tooling::engine_types::virtual_substate::{VirtualSubstate, VirtualSubstateId};
use tari_template_test_tooling::engine_types::commit_result::RejectReason;
use tari_template_test_tooling::support::assert_error::assert_reject_reason;
use tari_template_test_tooling::transaction::args;
use tari_template_test_tooling::TemplateTest;
use tari_utilities::byte_array::ByteArray;

const BIDDING_EPOCHS: u64 = 10;
const REVEAL_EPOCHS: u64 = 5;
const MAX_BIDDERS: u64 = 16;

fn bond() -> Amount {
    Amount::from_integer(10u64)
}

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

/// The commitment the template expects:
/// blake2b-256("sealed_bid/v1" ‖ component ‖ pk ‖ blinding ‖ amount_le).
/// Computed locally: the template's intrinsics only run inside the WASM engine.
fn commitment_for(
    component: ComponentAddress,
    pk: &RistrettoPublicKey,
    blinding: &[u8],
    amount: Amount,
) -> Hash32 {
    use blake2::digest::{Update, VariableOutput};
    use blake2::Blake2bVar;

    let mut hasher = Blake2bVar::new(32).expect("32-byte output");
    hasher.update(b"sealed_bid/v1");
    hasher.update(component.as_ref());
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

fn open_auction(
    test: &mut TemplateTest,
    seller: &Bidder,
    min_bid: Option<Amount>,
    max_bidders: u64,
) -> (ComponentAddress, ResourceAddress) {
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
                    max_bidders,
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

fn create_auction(
    test: &mut TemplateTest,
    seller: &Bidder,
    min_bid: Option<Amount>,
) -> (ComponentAddress, ResourceAddress) {
    open_auction(test, seller, min_bid, MAX_BIDDERS)
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

fn reveal(
    test: &mut TemplateTest,
    auction: ComponentAddress,
    who: &Bidder,
    blinding: Vec<u8>,
    amount: Amount,
) {
    test.execute_expect_success(
        test.transaction()
            .call_method(who.component, "withdraw", args![TARI_TOKEN, amount])
            .put_last_instruction_output_on_workspace("payment")
            .call_method(
                auction,
                "reveal",
                args![blinding, amount, Workspace("payment")],
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

fn claim_lot(test: &mut TemplateTest, auction: ComponentAddress, who: &Bidder) {
    test.execute_expect_success(
        test.transaction()
            .call_method(auction, "claim_lot", args![who.component])
            .build_and_seal(&who.key),
        vec![who.owner_token.clone()],
    );
}

fn claim_proceeds(test: &mut TemplateTest, auction: ComponentAddress, who: &Bidder) {
    test.execute_expect_success(
        test.transaction()
            .call_method(auction, "claim_proceeds", args![])
            .build_and_seal(&who.key),
        vec![who.owner_token.clone()],
    );
}

fn claim_refund(test: &mut TemplateTest, auction: ComponentAddress, who: &Bidder) {
    test.execute_expect_success(
        test.transaction()
            .call_method(auction, "claim_refund", args![who.component])
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
    let b2_start = tari_balance(&mut test, &b2);
    let ghost_start = tari_balance(&mut test, &ghost);
    let seller_start = tari_balance(&mut test, &seller);

    // Sealing phase: three sealed bids, no amounts anywhere on-chain.
    place_bid(
        &mut test,
        auction,
        &b1,
        commitment_for(auction, &b1.pk, &b1_blinding, Amount::from_integer(100u64)),
    );
    place_bid(
        &mut test,
        auction,
        &b2,
        commitment_for(auction, &b2.pk, &b2_blinding, Amount::from_integer(200u64)),
    );
    place_bid(
        &mut test,
        auction,
        &ghost,
        commitment_for(auction, &ghost.pk, &ghost_blinding, Amount::from_integer(999u64)),
    );

    // Before settlement the winning bid does not exist.
    assert_eq!(
        winning_bid(&mut test, auction, &b1),
        None,
        "winning_bid must stay hidden before settlement"
    );

    // Midnight passes, the reveal window opens.
    set_epoch(&mut test, BIDDING_EPOCHS + 2);
    reveal(
        &mut test,
        auction,
        &b1,
        b1_blinding.clone(),
        Amount::from_integer(100u64),
    );
    reveal(
        &mut test,
        auction,
        &b2,
        b2_blinding.clone(),
        Amount::from_integer(200u64),
    );
    // The ghost stays silent.

    // The reveal window closes; settle only records the winner.
    set_epoch(&mut test, BIDDING_EPOCHS + REVEAL_EPOCHS + 1);
    settle(&mut test, auction, &seller);
    assert_eq!(
        winning_bid(&mut test, auction, &b1),
        Some(Amount::from_integer(200u64))
    );

    // Pull-based payouts: each party claims their own funds.
    claim_lot(&mut test, auction, &b2);
    claim_proceeds(&mut test, auction, &seller);
    claim_refund(&mut test, auction, &b1);

    assert_eq!(
        balance(&mut test, &b2, &lot_resource),
        Amount::from_integer(1u64)
    );
    // The seller gets the winning bid and NOT the ghost bond (it stays locked).
    assert_eq!(
        tari_balance(&mut test, &seller),
        seller_start + Amount::from_integer(200u64)
    );
    // The loser is refunded in full (bond already returned at reveal time).
    assert_eq!(tari_balance(&mut test, &b1), b1_start);
    assert_eq!(
        tari_balance(&mut test, &b2),
        b2_start - Amount::from_integer(200u64)
    );
    // The ghost's bond is locked in the component forever.
    assert_eq!(tari_balance(&mut test, &ghost), ghost_start - bond());
}

#[test]
fn exact_tie_is_won_by_earliest_reveal_regardless_of_seal_order() {
    for seal_order in ["early_first", "late_first"] {
        let mut test = TemplateTest::my_crate();
        let seller = bidder(&mut test);
        let (auction, lot_resource) = create_auction(&mut test, &seller, None);

        let early = bidder(&mut test);
        let late = bidder(&mut test);
        let (first, second) = if seal_order == "early_first" {
            (&early, &late)
        } else {
            (&late, &early)
        };
        // Blinding factors are bound to the role, not to the seal order.
        let first_blinding: &[u8; 32] = if seal_order == "early_first" { &[7u8; 32] } else { &[8u8; 32] };
        let second_blinding: &[u8; 32] = if seal_order == "early_first" { &[8u8; 32] } else { &[7u8; 32] };

        place_bid(
            &mut test,
            auction,
            first,
            commitment_for(auction, &first.pk, first_blinding, Amount::from_integer(500u64)),
        );
        place_bid(
            &mut test,
            auction,
            second,
            commitment_for(auction, &second.pk, second_blinding, Amount::from_integer(500u64)),
        );

        set_epoch(&mut test, BIDDING_EPOCHS + 1);
        // `early` always reveals first, whatever order the bids were sealed in.
        reveal(&mut test, auction, &early, vec![7u8; 32], Amount::from_integer(500u64));
        reveal(&mut test, auction, &late, vec![8u8; 32], Amount::from_integer(500u64));

        set_epoch(&mut test, BIDDING_EPOCHS + REVEAL_EPOCHS + 1);
        settle(&mut test, auction, &seller);
        claim_lot(&mut test, auction, &early);
        claim_proceeds(&mut test, auction, &seller);
        claim_refund(&mut test, auction, &late);

        assert_eq!(
            balance(&mut test, &early, &lot_resource),
            Amount::from_integer(1u64),
            "earliest reveal must win the tie (seal order: {seal_order})"
        );
        assert_eq!(
            balance(&mut test, &late, &lot_resource),
            Amount::from_integer(0u64)
        );
    }
}

#[test]
fn auction_with_only_ghosts_returns_lot_and_locks_bonds() {
    let mut test = TemplateTest::my_crate();
    let seller = bidder(&mut test);
    let (auction, lot_resource) = create_auction(&mut test, &seller, None);

    let ghost = bidder(&mut test);
    let ghost_start = tari_balance(&mut test, &ghost);
    let seller_start = tari_balance(&mut test, &seller);
    place_bid(
        &mut test,
        auction,
        &ghost,
        commitment_for(auction, &ghost.pk, &[9u8; 32], Amount::from_integer(123u64)),
    );

    set_epoch(&mut test, BIDDING_EPOCHS + REVEAL_EPOCHS + 1);
    settle(&mut test, auction, &seller);
    // No winner: claim_proceeds hands the lot back to the seller.
    claim_proceeds(&mut test, auction, &seller);

    assert_eq!(
        balance(&mut test, &seller, &lot_resource),
        Amount::from_integer(1u64)
    );
    assert_eq!(tari_balance(&mut test, &seller), seller_start);
    assert_eq!(tari_balance(&mut test, &ghost), ghost_start - bond());
}

#[test]
fn reveal_below_reserve_is_accepted_but_cannot_win() {
    let mut test = TemplateTest::my_crate();
    let seller = bidder(&mut test);
    let reserve = Amount::from_integer(150u64);
    let (auction, lot_resource) = create_auction(&mut test, &seller, Some(reserve));

    let low = bidder(&mut test);
    let high = bidder(&mut test);
    let low_start = tari_balance(&mut test, &low);

    place_bid(
        &mut test,
        auction,
        &low,
        commitment_for(auction, &low.pk, &[1u8; 32], Amount::from_integer(100u64)),
    );
    place_bid(
        &mut test,
        auction,
        &high,
        commitment_for(auction, &high.pk, &[2u8; 32], Amount::from_integer(200u64)),
    );

    set_epoch(&mut test, BIDDING_EPOCHS + 1);
    // The below-reserve reveal is accepted but forfeits the bond (audit N2):
    // the reserve is public during sealing, so a below-reserve bid has no
    // honest purpose.
    reveal(
        &mut test,
        auction,
        &low,
        vec![1u8; 32],
        Amount::from_integer(100u64),
    );
    assert_eq!(
        tari_balance(&mut test, &low),
        low_start - Amount::from_integer(100u64) - bond()
    );
    reveal(
        &mut test,
        auction,
        &high,
        vec![2u8; 32],
        Amount::from_integer(200u64),
    );

    set_epoch(&mut test, BIDDING_EPOCHS + REVEAL_EPOCHS + 1);
    settle(&mut test, auction, &seller);
    claim_lot(&mut test, auction, &high);
    claim_proceeds(&mut test, auction, &seller);
    claim_refund(&mut test, auction, &low);

    // The high bid wins despite the low bid being revealed first.
    assert_eq!(
        balance(&mut test, &high, &lot_resource),
        Amount::from_integer(1u64)
    );
    assert_eq!(
        tari_balance(&mut test, &low),
        low_start - Amount::from_integer(100u64) - bond()
            + Amount::from_integer(100u64)
    );
}

#[test]
fn copied_commitment_cannot_be_replayed_by_another_signer() {
    let mut test = TemplateTest::my_crate();
    let seller = bidder(&mut test);
    let (auction, _) = create_auction(&mut test, &seller, None);

    let victim = bidder(&mut test);
    let thief = bidder(&mut test);
    // The thief seals the VICTIM's commitment under the THIEF's own key.
    let victim_blinding = vec![5u8; 32];
    let victim_amount = Amount::from_integer(77u64);
    let stolen = commitment_for(auction, &victim.pk, &victim_blinding, victim_amount);
    place_bid(&mut test, auction, &thief, stolen);

    // The thief cannot open it: the recomputed hash binds the signer's pk.
    set_epoch(&mut test, BIDDING_EPOCHS + 1);
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(
                thief.component,
                "withdraw",
                args![TARI_TOKEN, victim_amount],
            )
            .put_last_instruction_output_on_workspace("payment")
            .call_method(
                auction,
                "reveal",
                args![victim_blinding.clone(), victim_amount, Workspace("payment")],
            )
            .build_and_seal(&thief.key),
        vec![thief.owner_token.clone()],
    );
    assert_reject_reason(reason, "The reveal does not match the seal");
}

#[test]
fn ghost_cannot_claim_refund_and_no_winner_claim_lot_is_rejected() {
    let mut test = TemplateTest::my_crate();
    let seller = bidder(&mut test);
    // Reserve nobody meets -> no winner at settle.
    let (auction, _) = create_auction(
        &mut test,
        &seller,
        Some(Amount::from_integer(1_000u64)),
    );
    let ghost = bidder(&mut test);
    place_bid(
        &mut test,
        auction,
        &ghost,
        commitment_for(auction, &ghost.pk, &[3u8; 32], Amount::from_integer(5u64)),
    );
    set_epoch(&mut test, BIDDING_EPOCHS + REVEAL_EPOCHS + 1);
    settle(&mut test, auction, &seller);

    // The ghost never revealed: no refund path.
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(auction, "claim_refund", args![ghost.component])
            .build_and_seal(&ghost.key),
        vec![ghost.owner_token.clone()],
    );
    assert_reject_reason(reason, "No revealed bid for this signer");

    // With no winner, claim_lot is rejected with a clear reason.
    let honest = bidder(&mut test);
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(auction, "claim_lot", args![honest.component])
            .build_and_seal(&honest.key),
        vec![honest.owner_token.clone()],
    );
    assert_reject_reason(reason, "No winner: the lot goes to the seller via claim_proceeds");
}

#[test]
fn reserve_not_met_returns_lot_to_seller() {
    let mut test = TemplateTest::my_crate();
    let seller = bidder(&mut test);
    let (auction, lot_resource) = create_auction(
        &mut test,
        &seller,
        Some(Amount::from_integer(1_000u64)),
    );

    let low = bidder(&mut test);
    let low_start = tari_balance(&mut test, &low);
    place_bid(
        &mut test,
        auction,
        &low,
        commitment_for(auction, &low.pk, &[1u8; 32], Amount::from_integer(100u64)),
    );
    set_epoch(&mut test, BIDDING_EPOCHS + 1);
    reveal(
        &mut test,
        auction,
        &low,
        vec![1u8; 32],
        Amount::from_integer(100u64),
    );

    set_epoch(&mut test, BIDDING_EPOCHS + REVEAL_EPOCHS + 1);
    settle(&mut test, auction, &seller);
    claim_proceeds(&mut test, auction, &seller);
    claim_refund(&mut test, auction, &low);

    assert_eq!(
        balance(&mut test, &seller, &lot_resource),
        Amount::from_integer(1u64)
    );
    // The below-reserve bid was accepted and refunded, but the bond is
    // forfeited (audit N2): the reserve was public during sealing.
    assert_eq!(tari_balance(&mut test, &low), low_start - bond());
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
        commitment_for(auction, &b1.pk, &[1u8; 32], Amount::from_integer(100u64)),
    );

    set_epoch(&mut test, BIDDING_EPOCHS + 1);
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(
                b1.component,
                "withdraw",
                args![TARI_TOKEN, Amount::from_integer(100u64)],
            )
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
fn commitments_are_bound_to_this_auction() {
    let mut test = TemplateTest::my_crate();
    let seller = bidder(&mut test);
    let (auction, _) = create_auction(&mut test, &seller, None);
    let (other_auction, _) = create_auction(&mut test, &seller, None);

    let b1 = bidder(&mut test);
    // A commitment computed for a different auction component must not open here.
    place_bid(
        &mut test,
        auction,
        &b1,
        commitment_for(other_auction, &b1.pk, &[1u8; 32], Amount::from_integer(100u64)),
    );

    set_epoch(&mut test, BIDDING_EPOCHS + 1);
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(
                b1.component,
                "withdraw",
                args![TARI_TOKEN, Amount::from_integer(100u64)],
            )
            .put_last_instruction_output_on_workspace("payment")
            .call_method(
                auction,
                "reveal",
                args![vec![1u8; 32], Amount::from_integer(100u64), Workspace("payment")],
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
            .call_method(
                b1.component,
                "withdraw",
                args![TARI_TOKEN, Amount::from_integer(1u64)],
            )
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

    // Boundary: a bid inside the window, then a reveal exactly at bidding_end.
    place_bid(
        &mut test,
        auction,
        &b1,
        commitment_for(auction, &b1.pk, &[1u8; 32], Amount::from_integer(1u64)),
    );
    set_epoch(&mut test, BIDDING_EPOCHS);
    reveal(
        &mut test,
        auction,
        &b1,
        vec![1u8; 32],
        Amount::from_integer(1u64),
    );

    // A bid sealed at exactly bidding_end_epoch is too late.
    let b2 = bidder(&mut test);
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(b2.component, "withdraw", args![TARI_TOKEN, bond()])
            .put_last_instruction_output_on_workspace("bond")
            .call_method(
                auction,
                "place_bid",
                args![
                    commitment_for(auction, &b2.pk, &[2u8; 32], Amount::from_integer(1u64)),
                    b2.component,
                    Workspace("bond")
                ],
            )
            .build_and_seal(&b2.key),
        vec![b2.owner_token.clone()],
    );
    assert_reject_reason(reason, "The sealing phase is closed");

    // Too late to reveal at exactly reveal_end.
    let b3 = bidder(&mut test);
    set_epoch(&mut test, 0);
    place_bid(
        &mut test,
        auction,
        &b3,
        commitment_for(auction, &b3.pk, &[3u8; 32], Amount::from_integer(1u64)),
    );
    set_epoch(&mut test, BIDDING_EPOCHS + REVEAL_EPOCHS);
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(
                b3.component,
                "withdraw",
                args![TARI_TOKEN, Amount::from_integer(1u64)],
            )
            .put_last_instruction_output_on_workspace("payment")
            .call_method(
                auction,
                "reveal",
                args![vec![3u8; 32], Amount::from_integer(1u64), Workspace("payment")],
            )
            .build_and_seal(&b3.key),
        vec![b3.owner_token.clone()],
    );
    assert_reject_reason(reason, "The reveal window is closed");

    // Boundary: settle exactly at reveal_end succeeds.
    settle(&mut test, auction, &seller);
}

#[test]
fn rejects_double_reveal_wrong_bond_and_full_auction() {
    let mut test = TemplateTest::my_crate();
    let seller = bidder(&mut test);
    let (auction, _) = open_auction(&mut test, &seller, None, 1);

    let b1 = bidder(&mut test);
    place_bid(
        &mut test,
        auction,
        &b1,
        commitment_for(auction, &b1.pk, &[1u8; 32], Amount::from_integer(100u64)),
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
                    commitment_for(auction, &b1.pk, &[1u8; 32], Amount::from_integer(100u64)),
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
            .call_method(
                b2.component,
                "withdraw",
                args![TARI_TOKEN, bond() + Amount::from_integer(1u64)],
            )
            .put_last_instruction_output_on_workspace("bond")
            .call_method(
                auction,
                "place_bid",
                args![
                    commitment_for(auction, &b2.pk, &[2u8; 32], Amount::from_integer(100u64)),
                    b2.component,
                    Workspace("bond")
                ],
            )
            .build_and_seal(&b2.key),
        vec![b2.owner_token.clone()],
    );
    assert_reject_reason(reason, "The bond must exactly match the required bond amount");

    // The capacity cap rejects one more bidder.
    let b3 = bidder(&mut test);
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(b3.component, "withdraw", args![TARI_TOKEN, bond()])
            .put_last_instruction_output_on_workspace("bond")
            .call_method(
                auction,
                "place_bid",
                args![
                    commitment_for(auction, &b3.pk, &[3u8; 32], Amount::from_integer(100u64)),
                    b3.component,
                    Workspace("bond")
                ],
            )
            .build_and_seal(&b3.key),
        vec![b3.owner_token.clone()],
    );
    assert_reject_reason(reason, "The auction is full");

    // Reveal, then a second reveal is rejected.
    set_epoch(&mut test, BIDDING_EPOCHS + 1);
    reveal(
        &mut test,
        auction,
        &b1,
        vec![1u8; 32],
        Amount::from_integer(100u64),
    );
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(
                b1.component,
                "withdraw",
                args![TARI_TOKEN, Amount::from_integer(100u64)],
            )
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
fn rejects_payment_that_does_not_match_the_revealed_amount() {
    let mut test = TemplateTest::my_crate();
    let seller = bidder(&mut test);
    let (auction, _) = create_auction(&mut test, &seller, None);

    let b1 = bidder(&mut test);
    place_bid(
        &mut test,
        auction,
        &b1,
        commitment_for(auction, &b1.pk, &[4u8; 32], Amount::from_integer(100u64)),
    );
    set_epoch(&mut test, BIDDING_EPOCHS + 1);
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(
                b1.component,
                "withdraw",
                args![TARI_TOKEN, Amount::from_integer(99u64)],
            )
            .put_last_instruction_output_on_workspace("payment")
            .call_method(
                auction,
                "reveal",
                args![vec![4u8; 32], Amount::from_integer(100u64), Workspace("payment")],
            )
            .build_and_seal(&b1.key),
        vec![b1.owner_token.clone()],
    );
    assert_reject_reason(reason, "The payment must exactly match the revealed amount");
}

#[test]
fn rejects_short_blinding_zero_bid_and_foreign_signer() {
    let mut test = TemplateTest::my_crate();
    let seller = bidder(&mut test);
    let (auction, _) = create_auction(&mut test, &seller, None);

    // The blinding must be 32 bytes.
    let b1 = bidder(&mut test);
    place_bid(
        &mut test,
        auction,
        &b1,
        commitment_for(auction, &b1.pk, &[1u8; 31], Amount::from_integer(100u64)),
    );

    // A zero bid is sealed too (its rejection happens at reveal time).
    let b2 = bidder(&mut test);
    place_bid(
        &mut test,
        auction,
        &b2,
        commitment_for(auction, &b2.pk, &[2u8; 32], Amount::from_integer(0u64)),
    );

    set_epoch(&mut test, BIDDING_EPOCHS + 1);
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(
                b1.component,
                "withdraw",
                args![TARI_TOKEN, Amount::from_integer(100u64)],
            )
            .put_last_instruction_output_on_workspace("payment")
            .call_method(
                auction,
                "reveal",
                args![vec![1u8; 31], Amount::from_integer(100u64), Workspace("payment")],
            )
            .build_and_seal(&b1.key),
        vec![b1.owner_token.clone()],
    );
    assert_reject_reason(reason, "The blinding factor must be exactly 32 bytes");

    // A zero bid is rejected even with a valid commitment.
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(b2.component, "withdraw", args![TARI_TOKEN, Amount::from_integer(0u64)])
            .put_last_instruction_output_on_workspace("payment")
            .call_method(
                auction,
                "reveal",
                args![vec![2u8; 32], Amount::from_integer(0u64), Workspace("payment")],
            )
            .build_and_seal(&b2.key),
        vec![b2.owner_token.clone()],
    );
    assert_reject_reason(reason, "A bid must be positive");

    // A reveal from a signer with no sealed bid is rejected.
    let stranger = bidder(&mut test);
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(
                stranger.component,
                "withdraw",
                args![TARI_TOKEN, Amount::from_integer(7u64)],
            )
            .put_last_instruction_output_on_workspace("payment")
            .call_method(
                auction,
                "reveal",
                args![vec![9u8; 32], Amount::from_integer(7u64), Workspace("payment")],
            )
            .build_and_seal(&stranger.key),
        vec![stranger.owner_token.clone()],
    );
    assert_reject_reason(reason, "No sealed bid found for this signer");
}

#[test]
fn constructor_rejects_bad_arguments() {
    let mut test = TemplateTest::my_crate();
    let seller = bidder(&mut test);
    let template = test.get_template_address("SealedBidAuction");

    fn try_new(
        test: &mut TemplateTest,
        seller: &Bidder,
        template: tari_template_lib::types::TemplateAddress,
        seller_addr: ComponentAddress,
        bond_amt: Amount,
        bidding: u64,
        max_cap: u64,
        nft_count: u64,
    ) -> RejectReason {
        test.execute_expect_failure(
            test.transaction()
                .call_method(
                    NFT_FAUCET_COMPONENT_ADDRESS,
                    "mint",
                    args![Amount::from_integer(nft_count), tari_bor::Value::Null],
                )
                .put_last_instruction_output_on_workspace("lot")
                .call_function(
                    template,
                    "new",
                    args![
                        Workspace("lot"),
                        seller_addr,
                        Option::<Amount>::None,
                        bond_amt,
                        bidding,
                        REVEAL_EPOCHS,
                        max_cap,
                    ],
                )
                .build_and_seal(&seller.key),
            vec![seller.owner_token.clone()],
        )
    }

    // Seller address is not an account component.
    let reason = try_new(
        &mut test,
        &seller,
        template,
        NFT_FAUCET_COMPONENT_ADDRESS,
        bond(),
        BIDDING_EPOCHS,
        MAX_BIDDERS,
        1,
    );
    assert_reject_reason(reason, "The address is not an account component");

    // Zero bond.
    let reason = try_new(
        &mut test,
        &seller,
        template,
        seller.component,
        Amount::from_integer(0u64),
        BIDDING_EPOCHS,
        MAX_BIDDERS,
        1,
    );
    assert_reject_reason(reason, "The bond must be positive");

    // Zero epoch window.
    let reason = try_new(&mut test, &seller, template, seller.component, bond(), 0u64, MAX_BIDDERS, 1);
    assert_reject_reason(reason, "The bidding period must be positive");

    // Overflowing epoch window cannot wrap the reveal deadline.
    let reason = try_new(
        &mut test,
        &seller,
        template,
        seller.component,
        bond(),
        u64::MAX,
        MAX_BIDDERS,
        1,
    );
    assert_reject_reason(reason, "reveal_end_epoch overflows");

    // Zero capacity.
    let reason = try_new(&mut test, &seller, template, seller.component, bond(), BIDDING_EPOCHS, 0u64, 1);
    assert_reject_reason(reason, "max_bidders must be positive");

    // The lot must be a single NFT: two faucet NFTs are rejected.
    let reason = try_new(
        &mut test,
        &seller,
        template,
        seller.component,
        bond(),
        BIDDING_EPOCHS,
        MAX_BIDDERS,
        2,
    );
    assert_reject_reason(reason, "Can only auction a single NFT");
}

#[test]
fn claims_are_rejected_for_wrong_callers_and_twice() {
    let mut test = TemplateTest::my_crate();
    let seller = bidder(&mut test);
    let (auction, _) = create_auction(&mut test, &seller, None);

    let b1 = bidder(&mut test);
    let b2 = bidder(&mut test);
    place_bid(
        &mut test,
        auction,
        &b1,
        commitment_for(auction, &b1.pk, &[1u8; 32], Amount::from_integer(100u64)),
    );
    place_bid(
        &mut test,
        auction,
        &b2,
        commitment_for(auction, &b2.pk, &[2u8; 32], Amount::from_integer(200u64)),
    );
    set_epoch(&mut test, BIDDING_EPOCHS + 1);
    reveal(
        &mut test,
        auction,
        &b1,
        vec![1u8; 32],
        Amount::from_integer(100u64),
    );
    reveal(
        &mut test,
        auction,
        &b2,
        vec![2u8; 32],
        Amount::from_integer(200u64),
    );

    // Settle is rejected before the window.
    set_epoch(&mut test, BIDDING_EPOCHS + 1);
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(auction, "settle", args![])
            .build_and_seal(&seller.key),
        vec![seller.owner_token.clone()],
    );
    assert_reject_reason(reason, "Settlement is not open yet");

    // Claims are rejected before settlement.
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(auction, "claim_lot", args![b2.component])
            .build_and_seal(&b2.key),
        vec![b2.owner_token.clone()],
    );
    assert_reject_reason(reason, "The auction is not settled yet");

    set_epoch(&mut test, BIDDING_EPOCHS + REVEAL_EPOCHS + 1);
    settle(&mut test, auction, &seller);
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(auction, "settle", args![])
            .build_and_seal(&seller.key),
        vec![seller.owner_token.clone()],
    );
    assert_reject_reason(reason, "The auction is already settled");

    // Only the winner may claim the lot.
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(auction, "claim_lot", args![b1.component])
            .build_and_seal(&b1.key),
        vec![b1.owner_token.clone()],
    );
    assert_reject_reason(reason, "Only the winner may claim the lot");
    claim_lot(&mut test, auction, &b2);
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(auction, "claim_lot", args![b2.component])
            .build_and_seal(&b2.key),
        vec![b2.owner_token.clone()],
    );
    assert_reject_reason(reason, "The lot was already claimed");

    // Proceeds are claimed once.
    claim_proceeds(&mut test, auction, &seller);
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(auction, "claim_proceeds", args![])
            .build_and_seal(&seller.key),
        vec![seller.owner_token.clone()],
    );
    assert_reject_reason(reason, "The proceeds were already claimed");

    // The winner has no refund; the loser claims once.
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(auction, "claim_refund", args![b2.component])
            .build_and_seal(&b2.key),
        vec![b2.owner_token.clone()],
    );
    assert_reject_reason(reason, "The winner's payment goes to the seller");
    claim_refund(&mut test, auction, &b1);
    let reason = test.execute_expect_failure(
        test.transaction()
            .call_method(auction, "claim_refund", args![b1.component])
            .build_and_seal(&b1.key),
        vec![b1.owner_token.clone()],
    );
    assert_reject_reason(reason, "The refund was already claimed");
}

#[test]
fn mint_lot_mints_a_single_nft() {
    let mut test = TemplateTest::my_crate();
    let template = test.get_template_address("SealedBidAuction");
    let seller = bidder(&mut test);

    test.execute_expect_success(
        test.transaction()
            .call_function(
                template,
                "mint_lot",
                args!["Haunted Portrait".to_string(), "SPOOK".to_string()],
            )
            .put_last_instruction_output_on_workspace("lot")
            .call_method(seller.component, "deposit", args![Workspace("lot")])
            .build_and_seal(&seller.key),
        vec![seller.owner_token.clone()],
    );
}

#[test]
fn twenty_bidders_settle_and_claim_at_scale() {
    let mut test = TemplateTest::my_crate();
    let seller = bidder(&mut test);
    let (auction, lot_resource) = open_auction(&mut test, &seller, None, 20);

    // 20 bidders, each bidding their index; bidder 19 must win.
    let mut bidders = Vec::new();
    for i in 0..20u64 {
        let b = bidder(&mut test);
        place_bid(
            &mut test,
            auction,
            &b,
            commitment_for(
                auction,
                &b.pk,
                &vec![i as u8 + 1; 32],
                Amount::from_integer(10 * (i + 1)),
            ),
        );
        bidders.push(b);
    }

    set_epoch(&mut test, BIDDING_EPOCHS + 1);
    for (i, b) in bidders.iter().enumerate() {
        reveal(
            &mut test,
            auction,
            b,
            vec![i as u8 + 1; 32],
            Amount::from_integer(10 * (i as u64 + 1)),
        );
    }

    set_epoch(&mut test, BIDDING_EPOCHS + REVEAL_EPOCHS + 1);
    settle(&mut test, auction, &seller);
    let winner = bidders.last().unwrap();
    claim_lot(&mut test, auction, winner);
    claim_proceeds(&mut test, auction, &seller);
    assert_eq!(
        balance(&mut test, winner, &lot_resource),
        Amount::from_integer(1u64)
    );

    // Every loser pulls their own refund.
    for b in &bidders[..19] {
        claim_refund(&mut test, auction, b);
    }
}
