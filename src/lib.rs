use std::collections::HashMap;

use tari_template_lib::{intrinsics, prelude::*};

/// The account builtin template. Used to validate refund/payout addresses.
pub const ACCOUNT_TEMPLATE_ADDRESS: TemplateAddress = TemplateAddress::from_array([0u8; 32]);

/// A sealed-bid auction: bids are cryptographic commitments until midnight,
/// when the reveal window opens and every bid is proven against its seal.
/// Ghost bids — sealed but never revealed — forfeit their bonds to the seller.
///
/// Privacy model: while the auction is sealed, the only public facts are the
/// bidder's public key, a Blake2b commitment and a bond that is identical for
/// every bidder. The bid amount does not exist on-chain until the bidder
/// reveals it themselves, after the sealing phase has closed, so late bidding
/// and bid-sniping off the mempool are both impossible.
#[template]
mod sealed_bid_auction {
    use super::*;

    pub struct SealedBidAuction {
        /// The lot under auction: exactly one NFT.
        lot: Vault,
        /// The seller's account, paid the winning bid at settlement.
        seller_account: ComponentAddress,
        /// Optional reserve. Checked at reveal time, never at seal time.
        min_bid: Option<Amount>,
        /// Anti-spam bond, identical for every bidder so it leaks nothing
        /// about the sealed amount. Forfeited to the seller for ghost bids.
        bond_amount: Amount,
        /// The epoch at which the sealing phase ends ("midnight").
        bidding_end_epoch: u64,
        /// The epoch at which the reveal window closes and settlement opens.
        reveal_end_epoch: u64,
        /// Sealed commitments, keyed by the bidder's transaction signer.
        commitments: HashMap<RistrettoPublicKeyBytes, Hash32>,
        /// Where each bidder's bond refund, losing payment or lot goes.
        refund_accounts: HashMap<RistrettoPublicKeyBytes, ComponentAddress>,
        /// Bond escrow, one vault per bidder. Vaults are emptied on refund
        /// or forfeiture but deliberately stay in state: removing a vault substate
        /// mid-life orphans it and fails the transaction.
        bonds: HashMap<RistrettoPublicKeyBytes, Vault>,
        /// Revealed amounts, keyed like the commitments.
        bids: HashMap<RistrettoPublicKeyBytes, Amount>,
        /// Payment escrow for revealed bids.
        payments: HashMap<RistrettoPublicKeyBytes, Vault>,
        /// Reveal order; the earliest reveal wins exact ties.
        reveal_seq: HashMap<RistrettoPublicKeyBytes, u64>,
        next_seq: u64,
        settled: bool,
    }

    impl SealedBidAuction {
        pub fn new(
            lot: Bucket,
            seller_account: ComponentAddress,
            min_bid: Option<Amount>,
            bond_amount: Amount,
            bidding_epochs: u64,
            reveal_epochs: u64,
        ) -> Component<Self> {
            assert!(
                lot.resource_type() == ResourceType::NonFungible,
                "The lot must be an NFT"
            );
            assert_eq!(lot.amount(), Amount::from_integer(1u64), "Can only auction a single NFT");
            assert!(!bond_amount.is_zero(), "The bond must be positive");
            assert!(bidding_epochs > 0, "The bidding period must be positive");
            assert!(reveal_epochs > 0, "The reveal period must be positive");
            Self::assert_is_account(seller_account);

            let now = Consensus::current_epoch();
            Component::new(Self {
                lot: Vault::from_bucket(lot),
                seller_account,
                min_bid,
                bond_amount,
                bidding_end_epoch: now + bidding_epochs,
                reveal_end_epoch: now + bidding_epochs + reveal_epochs,
                commitments: HashMap::new(),
                refund_accounts: HashMap::new(),
                bonds: HashMap::new(),
                bids: HashMap::new(),
                payments: HashMap::new(),
                reveal_seq: HashMap::new(),
                next_seq: 0,
                settled: false,
            })
            .with_access_rules(AccessRules::allow_all())
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
        /// `blake2b(bidder_pk || blinding || amount_le_bytes)`, computed by
        /// the bidder's client; the auction learns nothing about the amount.
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
        /// bidders; ghosts forfeit theirs to the seller at settlement.
        pub fn reveal(&mut self, blinding: Vec<u8>, amount: Amount, payment: Bucket) {
            let now = Consensus::current_epoch();
            assert!(
                now >= self.bidding_end_epoch,
                "The reveal window is not open yet"
            );
            assert!(now < self.reveal_end_epoch, "The reveal window is closed");

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
            if let Some(min_bid) = self.min_bid {
                assert!(amount >= min_bid, "The revealed bid is below the reserve");
            }

            let computed = intrinsics::hash_blake2b_parts(&[
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

        /// Midnight. Award the lot, pay the seller, refund losing bids and
        /// forfeit the bonds of ghosts to the seller.
        pub fn settle(&mut self) {
            let now = Consensus::current_epoch();
            assert!(now >= self.reveal_end_epoch, "Settlement is not open yet");
            assert!(!self.settled, "The auction is already settled");

            let winner = self
                .bids
                .iter()
                .max_by(|(pk_a, amount_a), (pk_b, amount_b)| {
                    // Highest bid wins; on an exact tie the earlier reveal wins.
                    amount_a
                        .cmp(amount_b)
                        .then_with(|| self.reveal_seq[pk_b].cmp(&self.reveal_seq[pk_a]))
                })
                .map(|(pk, amount)| (*pk, *amount));

            match winner {
                Some((winner_pk, winning_bid)) => {
                    // The lot goes to the winner.
                    let lot = self.lot.withdraw_all();
                    let winner_account = self.refund_accounts[&winner_pk];
                    ComponentManager::get(winner_account).invoke("deposit", args![lot]);

                    // The seller is paid the winning bid.
                    let payment = self
                        .payments
                        .get_mut(&winner_pk)
                        .expect("Winner payment vault missing")
                        .withdraw_all();
                    ComponentManager::get(self.seller_account)
                        .invoke("deposit", args![payment]);

                    // Losing bids are refunded in full.
                    let losers: Vec<RistrettoPublicKeyBytes> = self
                        .payments
                        .keys()
                        .filter(|pk| **pk != winner_pk)
                        .copied()
                        .collect();
                    for pk in losers {
                        let refund = self
                            .payments
                            .get_mut(&pk)
                            .expect("Loser payment vault missing")
                            .withdraw_all();
                        let account = self.refund_accounts[&pk];
                        ComponentManager::get(account).invoke("deposit", args![refund]);
                    }

                    // Ghosts forfeit their bonds to the seller: sealed but
                    // never revealed. Tari cannot be burned by templates, so
                    // the penalty is paid out instead of destroyed.
                    let ghosts: Vec<RistrettoPublicKeyBytes> = self
                        .bonds
                        .keys()
                        .filter(|pk| !self.bids.contains_key(*pk))
                        .copied()
                        .collect();
                    let ghost_count = ghosts.len();
                    for pk in ghosts {
                        let forfeit = self
                            .bonds
                            .get_mut(&pk)
                            .expect("Ghost bond vault missing")
                            .withdraw_all();
                        ComponentManager::get(self.seller_account)
                            .invoke("deposit", args![forfeit]);
                    }

                    emit_event(
                        "AuctionSettled",
                        metadata![
                            "winner" => winner_pk.to_string(),
                            "winning_bid" => winning_bid.to_decimal_string(6),
                            "ghost_bonds_forfeited" => ghost_count.to_string(),
                        ],
                    );
                },
                None => {
                    // Nobody revealed: the lot returns to the seller and
                    // every bond is forfeited to them at midnight.
                    let ghosts = self.bonds.len();
                    for bond in self.bonds.values_mut() {
                        let forfeit = bond.withdraw_all();
                        ComponentManager::get(self.seller_account)
                            .invoke("deposit", args![forfeit]);
                    }
                    let lot = self.lot.withdraw_all();
                    ComponentManager::get(self.seller_account)
                        .invoke("deposit", args![lot]);

                    emit_event(
                        "AuctionSettled",
                        metadata![
                            "winner" => "none".to_string(),
                            "winning_bid" => "0".to_string(),
                            "ghost_bonds_forfeited" => ghosts.to_string(),
                        ],
                    );
                },
            }

            self.settled = true;
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

        /// The current phase: "Sealing", "Reveal", "AwaitingSettlement" or
        /// "Settled".
        pub fn phase(&self) -> String {
            let now = Consensus::current_epoch();
            if self.settled {
                "Settled".to_string()
            } else if now < self.bidding_end_epoch {
                "Sealing".to_string()
            } else if now < self.reveal_end_epoch {
                "Reveal".to_string()
            } else {
                "AwaitingSettlement".to_string()
            }
        }

        /// The winning bid, but only once the auction is settled: before
        /// settlement the amounts of sealed bids do not exist on-chain.
        pub fn winning_bid(&self) -> Option<Amount> {
            if !self.settled {
                return None;
            }
            self.bids.values().copied().max()
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
