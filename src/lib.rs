use std::collections::{BTreeMap, BTreeSet};

use tari_template_lib::{intrinsics, prelude::*};

/// The account builtin template. Used to validate refund/payout addresses.
/// Defined locally, as in the official nft_marketplace starter template.
pub const ACCOUNT_TEMPLATE_ADDRESS: TemplateAddress = TemplateAddress::from_array([0u8; 32]);

/// Domain separator for bid commitments. Binds a commitment to this template
/// (and, together with the component address in the hash, to one auction).
pub const COMMITMENT_DOMAIN: &[u8] = b"sealed_bid/v1";

/// A sealed-bid auction: bids are cryptographic commitments until midnight,
/// when the reveal window opens and every bid is proven against its seal.
///
/// Privacy model: while the auction is sealed, the only public facts are the
/// bidder's public key, a Blake2b commitment and a bond that is identical for
/// every bidder. The bid amount does not exist on-chain until the bidder
/// reveals it themselves, after the sealing phase has closed, so late bidding
/// and bid-sniping off the mempool are both impossible during sealing.
///
/// Settlement is pull-based: `settle()` only records the winner, and every
/// party then claims their own funds (`claim_lot`, `claim_proceeds`,
/// `claim_refund`). No single failed deposit can ever lock someone else's
/// funds. Ghost bonds — sealed but never revealed — are locked in the
/// component forever: they are paid to no one, so a seller gains nothing from
/// shill bids that stay sealed.
#[template]
mod sealed_bid_auction {
    use super::*;

    pub struct SealedBidAuction {
        /// The lot under auction: exactly one NFT.
        lot: Vault,
        /// The seller's account, paid the winning bid on `claim_proceeds`.
        seller_account: ComponentAddress,
        /// Optional reserve. Reveals below it are accepted but cannot win.
        min_bid: Option<Amount>,
        /// Anti-spam bond, identical for every bidder so it leaks nothing
        /// about the sealed amount. Forfeit (locked) for ghost bids.
        bond_amount: Amount,
        /// The epoch at which the sealing phase ends ("midnight").
        bidding_end_epoch: u64,
        /// The epoch at which the reveal window closes and settlement opens.
        reveal_end_epoch: u64,
        /// Maximum number of sealed bids, to bound state growth and the
        /// cost of settlement.
        max_bidders: u64,
        /// Sealed commitments, keyed by the bidder's transaction signer.
        commitments: BTreeMap<RistrettoPublicKeyBytes, Hash32>,
        /// Where each bidder's bond refund or payment goes.
        refund_accounts: BTreeMap<RistrettoPublicKeyBytes, ComponentAddress>,
        /// Bond escrow, one vault per bidder. Vaults stay in state: removing
        /// a vault substate mid-life orphans it and fails the transaction.
        bonds: BTreeMap<RistrettoPublicKeyBytes, Vault>,
        /// Revealed amounts, keyed like the commitments.
        bids: BTreeMap<RistrettoPublicKeyBytes, Amount>,
        /// Payment escrow for revealed bids.
        payments: BTreeMap<RistrettoPublicKeyBytes, Vault>,
        /// Reveal order; the earliest reveal wins exact ties.
        reveal_seq: BTreeMap<RistrettoPublicKeyBytes, u64>,
        next_seq: u64,
        /// Settlement outcome, set once by `settle`.
        winner: Option<RistrettoPublicKeyBytes>,
        winning_bid: Option<Amount>,
        settled: bool,
        lot_claimed: bool,
        proceeds_claimed: bool,
        refunds_claimed: BTreeSet<RistrettoPublicKeyBytes>,
    }

    impl SealedBidAuction {
        #[allow(clippy::too_many_arguments)]
        pub fn new(
            lot: Bucket,
            seller_account: ComponentAddress,
            min_bid: Option<Amount>,
            bond_amount: Amount,
            bidding_epochs: u64,
            reveal_epochs: u64,
            max_bidders: u64,
        ) -> Component<Self> {
            assert!(
                lot.resource_type() == ResourceType::NonFungible,
                "The lot must be an NFT"
            );
            assert_eq!(lot.amount(), Amount::from_integer(1u64), "Can only auction a single NFT");
            assert!(!bond_amount.is_zero(), "The bond must be positive");
            assert!(bidding_epochs > 0, "The bidding period must be positive");
            assert!(reveal_epochs > 0, "The reveal period must be positive");
            assert!(max_bidders > 0, "max_bidders must be positive");
            Self::assert_is_account(seller_account);

            let now = Consensus::current_epoch();
            // Checked arithmetic: an overflowing deadline could otherwise wrap
            // around and open settlement immediately (a seller trap).
            let bidding_end_epoch = now
                .checked_add(bidding_epochs)
                .expect("bidding_end_epoch overflows");
            let reveal_end_epoch = bidding_end_epoch
                .checked_add(reveal_epochs)
                .expect("reveal_end_epoch overflows");
            assert!(reveal_end_epoch > bidding_end_epoch, "Invalid epoch windows");

            Component::new(Self {
                lot: Vault::from_bucket(lot),
                seller_account,
                min_bid,
                bond_amount,
                bidding_end_epoch,
                reveal_end_epoch,
                max_bidders,
                commitments: BTreeMap::new(),
                refund_accounts: BTreeMap::new(),
                bonds: BTreeMap::new(),
                bids: BTreeMap::new(),
                payments: BTreeMap::new(),
                reveal_seq: BTreeMap::new(),
                next_seq: 0,
                winner: None,
                winning_bid: None,
                settled: false,
                lot_claimed: false,
                proceeds_claimed: false,
                refunds_claimed: BTreeSet::new(),
            })
            .with_access_rules(AccessRules::allow_all())
            // No owner: nobody can ever rewrite the rules of an auction that
            // already holds other people's escrow.
            .with_owner_rule(OwnerRule::None)
            .create()
        }

        /// Convenience: mint a fresh single-NFT collection that can be used
        /// as a lot. Sellers who do not already own a suitable NFT can mint
        /// one and deposit it straight into a new auction.
        pub fn mint_lot(name: String, symbol: String) -> Bucket {
            ResourceBuilder::non_fungible()
                .metadata("name", &name)
                .with_token_symbol(&symbol)
                .initial_supply_with_data(vec![(
                    NonFungibleId::from_u64(1),
                    (&metadata!["name" => name], &()),
                )])
        }

        /// Seal a bid. The commitment must be
        /// `blake2b("sealed_bid/v1" ‖ component ‖ bidder_pk ‖ blinding ‖ amount_le)`
        /// with a 32-byte blinding, computed by the bidder's client; the
        /// auction learns nothing about the amount. The bond is identical for
        /// all bidders and reveals nothing.
        pub fn place_bid(
            &mut self,
            commitment: Hash32,
            refund_account: ComponentAddress,
            bond: Bucket,
        ) {
            let now = Consensus::current_epoch();
            assert!(now < self.bidding_end_epoch, "The sealing phase is closed");
            assert_eq!(
                bond.resource_address(),
                TARI_TOKEN,
                "Bonds are paid in Tari"
            );
            assert_eq!(
                bond.amount(),
                self.bond_amount,
                "The bond must exactly match the required bond amount"
            );
            Self::assert_is_account(refund_account);

            let bidder = CallerContext::transaction_signer_public_key();
            assert!(
                !self.commitments.contains_key(&bidder),
                "This bidder already sealed a bid"
            );
            assert!(
                (self.commitments.len() as u64) < self.max_bidders,
                "The auction is full"
            );

            self.commitments.insert(bidder, commitment);
            self.refund_accounts.insert(bidder, refund_account);
            self.bonds.insert(bidder, Vault::from_bucket(bond));

            emit_event(
                "BidSealed",
                metadata![
                    "bidder" => bidder.to_string(),
                    "commitment" => commitment.to_string(),
                ],
            );
        }

        /// Open a sealed bid: prove the commitment and pay the revealed
        /// amount up front. The bond is returned immediately to honest
        /// bidders. A valid opening below the reserve is accepted (the bond
        /// is returned) but cannot win the auction.
        pub fn reveal(&mut self, blinding: Vec<u8>, amount: Amount, payment: Bucket) {
            let now = Consensus::current_epoch();
            assert!(
                now >= self.bidding_end_epoch,
                "The reveal window is not open yet"
            );
            assert!(now < self.reveal_end_epoch, "The reveal window is closed");
            assert!(
                blinding.len() == 32,
                "The blinding factor must be exactly 32 bytes"
            );
            assert!(amount > Amount::zero(), "A bid must be positive");

            let bidder = CallerContext::transaction_signer_public_key();
            let commitment = self
                .commitments
                .get(&bidder)
                .expect("No sealed bid found for this signer");
            assert!(
                !self.bids.contains_key(&bidder),
                "This bid was already revealed"
            );

            assert_eq!(
                payment.resource_address(),
                TARI_TOKEN,
                "Bids are paid in Tari"
            );
            assert_eq!(
                payment.amount(),
                amount,
                "The payment must exactly match the revealed amount"
            );

            let computed = intrinsics::hash_blake2b_parts(&[
                COMMITMENT_DOMAIN,
                CallerContext::current_component_address().as_ref(),
                bidder.as_ref(),
                blinding.as_slice(),
                &amount.to_le_bytes(),
            ]);
            assert_eq!(computed, *commitment, "The reveal does not match the seal");

            // Return the bond to the honest bidder.
            let bond = self.bonds.get_mut(&bidder).expect("Bond vault missing");
            let refund = bond.withdraw_all();
            let refund_account = self.refund_accounts[&bidder];
            ComponentManager::get(refund_account).invoke("deposit", args![refund]);

            self.bids.insert(bidder, amount);
            self.payments.insert(bidder, Vault::from_bucket(payment));
            self.reveal_seq.insert(bidder, self.next_seq);
            self.next_seq += 1;

            emit_event(
                "BidRevealed",
                metadata![
                    "bidder" => bidder.to_string(),
                    "amount" => amount.to_decimal_string(6),
                ],
            );
        }

        /// Midnight. Record the winner; pay nothing here. Every party then
        /// pulls their own funds: `claim_lot`, `claim_proceeds`,
        /// `claim_refund`. Ghost bonds stay locked in the component forever
        /// and are paid to no one.
        pub fn settle(&mut self) {
            let now = Consensus::current_epoch();
            assert!(now >= self.reveal_end_epoch, "Settlement is not open yet");
            assert!(!self.settled, "The auction is already settled");

            let min_bid = self.min_bid.unwrap_or(Amount::from_integer(1u64));
            let winner = self
                .bids
                .iter()
                .filter(|(_, amount)| **amount >= min_bid)
                .max_by(|(pk_a, amount_a), (pk_b, amount_b)| {
                    // Highest bid wins; on an exact tie the earlier reveal wins.
                    amount_a
                        .cmp(amount_b)
                        .then_with(|| self.reveal_seq[pk_b].cmp(&self.reveal_seq[pk_a]))
                })
                .map(|(pk, amount)| (*pk, *amount));

            let ghosts = self
                .bonds
                .keys()
                .filter(|pk| !self.bids.contains_key(*pk))
                .count();

            match winner {
                Some((winner_pk, winning_bid)) => {
                    self.winner = Some(winner_pk);
                    self.winning_bid = Some(winning_bid);
                    emit_event(
                        "AuctionSettled",
                        metadata![
                            "winner" => winner_pk.to_string(),
                            "winning_bid" => winning_bid.to_decimal_string(6),
                            "ghost_bonds_locked" => ghosts.to_string(),
                        ],
                    );
                },
                None => {
                    // No bid met the reserve (or nobody revealed).
                    emit_event(
                        "AuctionSettled",
                        metadata![
                            "winner" => "none".to_string(),
                            "winning_bid" => "0".to_string(),
                            "ghost_bonds_locked" => ghosts.to_string(),
                        ],
                    );
                },
            }

            self.settled = true;
        }

        /// The winner pulls the lot. Only the recorded winner may call this,
        /// exactly once.
        pub fn claim_lot(&mut self, payout_account: ComponentAddress) {
            assert!(self.settled, "The auction is not settled yet");
            let winner = self
                .winner
                .expect("No winner: the lot goes to the seller via claim_proceeds");
            let caller = CallerContext::transaction_signer_public_key();
            assert_eq!(caller, winner, "Only the winner may claim the lot");
            assert!(!self.lot_claimed, "The lot was already claimed");
            Self::assert_is_account(payout_account);

            let lot = self.lot.withdraw_all();
            ComponentManager::get(payout_account).invoke("deposit", args![lot]);
            self.lot_claimed = true;
        }

        /// Anyone may pull the proceeds for the seller: the winning payment,
        /// or — when no bid met the reserve — the lot itself.
        pub fn claim_proceeds(&mut self) {
            assert!(self.settled, "The auction is not settled yet");
            assert!(!self.proceeds_claimed, "The proceeds were already claimed");

            if self.winner.is_some() {
                let winner = self.winner.expect("checked above");
                let payment = self
                    .payments
                    .get_mut(&winner)
                    .expect("Winner payment vault missing")
                    .withdraw_all();
                ComponentManager::get(self.seller_account)
                    .invoke("deposit", args![payment]);
            } else {
                let lot = self.lot.withdraw_all();
                ComponentManager::get(self.seller_account)
                    .invoke("deposit", args![lot]);
            }
            self.proceeds_claimed = true;
        }

        /// A losing bidder pulls their full payment back. Only the bidder may
        /// call this, exactly once.
        pub fn claim_refund(&mut self, payout_account: ComponentAddress) {
            assert!(self.settled, "The auction is not settled yet");
            let caller = CallerContext::transaction_signer_public_key();
            assert!(
                self.bids.contains_key(&caller),
                "No revealed bid for this signer"
            );
            assert!(
                Some(caller) != self.winner,
                "The winner's payment goes to the seller"
            );
            assert!(
                !self.refunds_claimed.contains(&caller),
                "The refund was already claimed"
            );
            Self::assert_is_account(payout_account);

            let refund = self
                .payments
                .get_mut(&caller)
                .expect("Payment vault missing")
                .withdraw_all();
            ComponentManager::get(payout_account).invoke("deposit", args![refund]);
            self.refunds_claimed.insert(caller);
        }

        /// The resource of the lot on auction.
        pub fn lot_resource(&self) -> ResourceAddress {
            self.lot.resource_address()
        }

        /// How many bids are currently sealed. Amounts stay hidden.
        pub fn sealed_bid_count(&self) -> u64 {
            self.commitments.len() as u64
        }

        /// How many bids have been revealed so far.
        pub fn revealed_bid_count(&self) -> u64 {
            self.bids.len() as u64
        }

        /// The current phase: "Sealing", "Reveal", "AwaitingSettlement",
        /// "Settled" or "Claimed".
        pub fn phase(&self) -> String {
            let now = Consensus::current_epoch();
            if !self.settled {
                if now < self.bidding_end_epoch {
                    "Sealing".to_string()
                } else if now < self.reveal_end_epoch {
                    "Reveal".to_string()
                } else {
                    "AwaitingSettlement".to_string()
                }
            } else if self.lot_claimed || self.proceeds_claimed {
                "Claimed".to_string()
            } else {
                "Settled".to_string()
            }
        }

        /// The winning bid, but only once the auction is settled: before
        /// settlement the amounts of sealed bids do not exist on-chain.
        pub fn winning_bid(&self) -> Option<Amount> {
            if !self.settled {
                return None;
            }
            self.winning_bid
        }

        fn assert_is_account(address: ComponentAddress) {
            let component = ComponentManager::get(address);
            assert_eq!(
                component.get_template_address(),
                ACCOUNT_TEMPLATE_ADDRESS,
                "The address is not an account component"
            );
        }
    }
}
