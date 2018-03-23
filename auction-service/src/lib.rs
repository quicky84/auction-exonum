extern crate bodyparser;
#[macro_use]
extern crate exonum;
#[macro_use]
extern crate failure;
extern crate iron;
extern crate router;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;

// Import necessary types from crates.

use exonum::blockchain::{ApiContext, Blockchain, ExecutionError, ExecutionResult, Service,
                         Transaction, TransactionSet};
use exonum::encoding::serialize::FromHex;
use exonum::node::{ApiSender, TransactionSend};
use exonum::messages::RawTransaction;
use exonum::storage::{Fork, ListIndex, MapIndex, Snapshot};
use exonum::crypto::{Hash, PublicKey};
use exonum::encoding;
use exonum::api::{Api, ApiError};
use iron::prelude::*;
use iron::Handler;
use iron::status::Status;
use iron::headers::ContentType;
use iron::modifiers::Header;
use router::Router;

// // // // // // // // // // CONSTANTS // // // // // // // // // //
const SERVICE_NAME: &str = "auction";

/// Service ID for the `Service` trait.
const SERVICE_ID: u16 = 1;

/// Initial funds available for every new participant.
const INIT_BALANCE: u64 = 100;

// // // // // // // // // // PERSISTENT DATA // // // // // // // // // //
encoding_struct!{
    struct Participant {
        /// Public key of the participant.
        pub_key: &PublicKey,
        /// Name of the participant.
        name: &str,
        /// Current balance.
        balance: u64,
        /// Reserved money that participate in the auction.
        reserved: u64,
    }
}

encoding_struct! {
    struct Item {
        id: u64,
        // This sruct is saved in the storage, thus no rust's lifetimes will not help.
        // When deleting a participants, make sure that either the relevant participant is deleted
        // or item is denoted as deserted.

        // Owner of the item is a participant identified by their public key.
        owner_key: &PublicKey,
        // Description of the item.
        desc: &str,
        // URL of the item image.
        url: &str,
    }
}

encoding_struct! {
    struct Bid {
        // Bidder is some participant identified by their public key.
        bidder_key: &PublicKey,
        // Value of the bid.
        value: u64,
    }
}

encoding_struct! {
    struct Auction {
        id: u64,
        // Participant selling the item.
        auctioner_key: &PublicKey,
        // Item with item_id is auctioned.
        item_id: u64,
        start_price: u64,
        // History of bids. Last bid wins.
        bidding: Vec<Bid>,
        // If closed => no auctions are accepted.
        closed: bool,
    }
}

impl Participant {
    /// Returns a copy of this participant with the reserve increased by the specified amount.
    pub fn reserve(self, amount: u64) -> Self {
        let reserved = self.reserved() + amount;
        Self::new(self.pub_key(), self.name(), self.balance(), reserved)
    }

    /// Returns a copy of this participant with the reserve decreased by the specified amount.
    pub fn release(self, amount: u64) -> Self {
        let reserved = self.reserved() - amount;
        Self::new(self.pub_key(), self.name(), self.balance(), reserved)
    }

    /// Returns a copy of this participant with the balance increased by the specified amount.
    pub fn increase(self, amount: u64) -> Self {
        let balance = self.balance() + amount;
        Self::new(self.pub_key(), self.name(), balance, self.reserved())
    }

    /// Returns a copy of this participant with the balance descreased by the specified amount.
    pub fn decrease(self, amount: u64) -> Self {
        let balance = self.balance() - amount;
        Self::new(self.pub_key(), self.name(), balance, self.reserved())
    }
}

impl Auction {
    /// Returns a copy of this auction with a new bid.
    pub fn bid(self, bid: Bid) -> Self {
        let mut bidding = self.bidding();
        bidding.push(bid);

        Self::new(
            self.id(),
            self.auctioner_key(),
            self.item_id(),
            self.start_price(),
            bidding,
            self.closed(),
        )
    }

    pub fn close(self) -> Self {
        Self::new(
            self.id(),
            self.auctioner_key(),
            self.item_id(),
            self.start_price(),
            self.bidding(),
            true,
        )
    }
}
// // // // // // // // // // // DATA LAYOUT // // // // // // // // // //

/// Schema of the key-value storage used by the auction service.
pub struct AuctionSchema<T> {
    view: T,
}

impl<T: AsRef<Snapshot>> AuctionSchema<T> {
    /// Creates a new schema instance.
    pub fn new(view: T) -> Self {
        AuctionSchema { view }
    }

    /// Returns an immutable version of the participants table.
    pub fn participants(&self) -> MapIndex<&Snapshot, PublicKey, Participant> {
        MapIndex::new("platform.participants", self.view.as_ref())
    }

    /// Gets a specific participant from the storage.
    pub fn participant(&self, pub_key: &PublicKey) -> Option<Participant> {
        self.participants().get(pub_key)
    }

    /// Returns an immutable version of the items table.
    pub fn items(&self) -> ListIndex<&Snapshot, Item> {
        ListIndex::new("platform.items", self.view.as_ref())
    }

    /// Gets a specific item from the storage.
    pub fn item(&self, id: u64) -> Option<Item> {
        self.items().get(id)
    }

    /// Returns an immutable version of the actions table.
    pub fn auctions(&self) -> ListIndex<&Snapshot, Auction> {
        ListIndex::new("platform.auctions", self.view.as_ref())
    }

    /// Gets a specific item from the storage.
    pub fn auction(&self, id: u64) -> Option<Auction> {
        self.auctions().get(id)
    }
}

/// A mutable version of the schema with an additional method to store participants,
/// items, and auctions to the storage.
impl<'a> AuctionSchema<&'a mut Fork> {
    pub fn participants_mut(&mut self) -> MapIndex<&mut Fork, PublicKey, Participant> {
        MapIndex::new("platform.participants", &mut self.view)
    }

    pub fn items_mut(&mut self) -> ListIndex<&mut Fork, Item> {
        ListIndex::new("platform.items", &mut self.view)
    }

    pub fn auctions_mut(&mut self) -> ListIndex<&mut Fork, Auction> {
        ListIndex::new("platform.auctions", &mut self.view)
    }
}
// // // // // // // // // // TRANSACTIONS // // // // // // // // // //

transactions! {
    AuctionTransactions {
        const SERVICE_ID = SERVICE_ID;

        /// Transaction type for adding a new participant.
        struct TxNewParticipant {
            /// Public key of the participant.
            pub_key: &PublicKey,
            /// UTF-8 string with the participant's name.
            name: &str,
        }

        /// Transaction type for adding a new item.
        struct TxNewItem{
            /// Creator/Owner of the item.
            owner_key: &PublicKey,
            /// Description of the item.
            desc: &str,
            /// url to the item's image.
            url: &str,
        }

        /// Transaction type for adding a new item.
        struct TxNewAuction {
            /// Auctioner of the item.
            auctioner_key: &PublicKey,
            /// ID of the item to auction.
            item_id: u64,
            /// Starting price of the item.
            start_price: u64,
        }

        struct TxBid {
            /// Bidder.
            bidder_key: &PublicKey,
            /// Auction ID where a bid must be made.
            auction_id: u64,
            /// Bid value.
            value: u64,
        }

        struct TxCloseAuction {
            /// Auction to close.
            auction_id: u64,
        }
    }
}

// // // // // // // // // // // CONTRACT ERRORS // // // // // // // // // //

/// Error codes emitted by transactions during execution.
#[derive(Debug, Fail)]
#[repr(u8)]
pub enum Error {
    #[fail(display = "Participant is already registered")]
    ///
    ParticipantAlreadyRegistered = 0,

    #[fail(display = "Participant is not registered")]
    ///
    ParticipantIsNotRegistered = 1,

    #[fail(display = "Item doesn't exist")]
    ///
    ItemNotFound = 2,

    #[fail(display = "You do not own of the item")]
    ///
    ItemNotOwned = 3,

    #[fail(display = "Auction does not exist")]
    ///
    AuctionNotFound = 4,

    #[fail(display = "Auction is closed")]
    ///
    AuctionClosed = 5,

    // This message depends on the auction.
    #[fail(display = "Bid is below the current highest bid")]
    ///
    BidTooLow = 6,

    // This message depends on the auction.
    #[fail(display = "Insufficient funds")]
    ///
    InsufficientFunds = 7,
}

impl From<Error> for ExecutionError {
    fn from(value: Error) -> ExecutionError {
        let description = format!("{}", &value);
        ExecutionError::with_description(value as u8, description)
    }
}

// // // // // // // // // // // CONTRACTS // // // // // // // // // //

impl Transaction for TxNewParticipant {
    /// Verifies integrity of the transaction by checking the transaction
    /// signature.
    fn verify(&self) -> bool {
        // self.verify_signature(self.pub_key())
        true
    }

    /// If no participant with the specified public key is not registered,
    /// add a participant with the specified public key, name, and an initial balance of INIT_BALANCE.
    /// Otherwise, emit error.
    fn execute(&self, view: &mut Fork) -> ExecutionResult {
        let mut schema = AuctionSchema::new(view);

        let participant = match schema.participant(self.pub_key()) {
            Some(_) => Err(Error::ParticipantAlreadyRegistered)?,
            None => Participant::new(self.pub_key(), self.name(), INIT_BALANCE, 0),
        };

        println!("\nParticipant is registered:\n{:?}", participant);

        schema.participants_mut().put(self.pub_key(), participant);

        let participants = schema.participants();
        println!("\nParticipants");
        for p in participants.iter() {
            println!("{:?}", p);
        }

        Ok(())
    }
}

impl Transaction for TxNewItem {
    /// Verifies integrity of the transaction by checking the transaction
    /// signature.
    fn verify(&self) -> bool {
        // self.verify_signature(self.pub_key())
        true
    }

    /// Adds a new item, id is computed.
    fn execute(&self, view: &mut Fork) -> ExecutionResult {
        let mut schema = AuctionSchema::new(view);
        // We cannot distinguish between the items, so any item is new.

        // Check whether the owner is registered.
        let owner = schema.participant(self.owner_key());
        if owner.is_none() {
            println!("\nOwner of the item does not exist");
            Err(Error::ParticipantIsNotRegistered)?;
        }

        let item = Item::new(
            schema.items().len(),
            self.owner_key(),
            self.desc(),
            self.url(),
        );

        println!("\nItem is created:\n{:?}", item);

        schema.items_mut().push(item);

        let items = schema.items();
        println!("\nItems");
        for i in items.iter() {
            println!("{:?}", i);
        }

        Ok(())
    }
}

impl Transaction for TxNewAuction {
    /// Verifies integrity of the transaction by checking the transaction
    /// signature.
    fn verify(&self) -> bool {
        // self.verify_signature(self.pub_key())
        true
    }

    /// Adds a new item, id is computed.
    fn execute(&self, view: &mut Fork) -> ExecutionResult {
        let mut schema = AuctionSchema::new(view);

        // Check whether the participant (auctioner) is registered.
        let auctioner = match schema.participant(self.auctioner_key()) {
            Some(a) => a,
            None => Err(Error::ParticipantIsNotRegistered)?,
        };

        let item = match schema.items().get(self.item_id()) {
            Some(i) => i,
            None => Err(Error::ItemNotFound)?,
        };

        // Check whether the item is owned by the auctioner.
        if auctioner.pub_key() != item.owner_key() {
            Err(Error::ItemNotOwned)?;
        }

        // Auction is legit.

        let auction = Auction::new(
            schema.auctions().len(),
            self.auctioner_key(),
            item.id(),
            self.start_price(),
            // Initial bid is made by the owner of the item.
            vec![],
            false,
        );

        println!("\nAuction is created:\n{:?}", auction);

        schema.auctions_mut().push(auction);

        let auctions = schema.auctions();
        println!("\nAuctions");
        for a in auctions.iter() {
            println!("{:?}", a);
        }

        Ok(())
    }
}

impl Transaction for TxBid {
    /// Verifies integrity of the transaction by checking the transaction
    /// signature.
    fn verify(&self) -> bool {
        // self.verify_signature(self.pub_key())
        true
    }

    /// Adds a new item, id is computed.
    fn execute(&self, view: &mut Fork) -> ExecutionResult {
        let mut schema = AuctionSchema::new(view);

        // Check whether the participant (bidder) is registered.
        let bidder = match schema.participant(self.bidder_key()) {
            Some(b) => b,
            None => Err(Error::ParticipantIsNotRegistered)?,
        };

        // Check whether the auction exists.
        let auction = match schema.auctions().get(self.auction_id()) {
            Some(a) => a,
            None => Err(Error::AuctionNotFound)?,
        };

        if auction.closed() {
            Err(Error::AuctionClosed)?;
        }

        // Bidding contain at least 1 element.
        let bidding = auction.bidding() as Vec<Bid>;
        let (prev_bidder_key, prev_highest_bid) = match bidding.last() {
            Some(bid) => (Some(bid.bidder_key()), bid.value()),
            None => (None, auction.start_price()),
        };

        // Funds available for the bidder to bid.
        let funds = bidder.balance() - bidder.reserved();
        if self.value() > funds {
            Err(Error::InsufficientFunds)?;
        }

        if self.value() <= prev_highest_bid {
            Err(Error::BidTooLow)?;
        }

        // Bidder has enough funds and made highest bid.
        // 1. Update the auction.
        {
            let mut auctions = schema.auctions_mut();
            let auction = auctions.get(self.auction_id()).unwrap();
            let auction = auction.bid(Bid::new(bidder.pub_key(), self.value()));

            println!("\nBid {:?}\nupdates auction\n{:?}", &self, &auction);

            auctions.set(auction.id(), auction);
        }

        // 2. Release funds of the previous bidder, reserve funds of the bidder.
        {
            // Release.
            let mut participants = schema.participants_mut();
            if let Some(key) = prev_bidder_key {
                let prev_bidder = participants.get(key).unwrap();
                let prev_bidder = prev_bidder.release(prev_highest_bid);
                participants.put(key, prev_bidder);
            }

            // Reserve.
            let bidder = bidder.reserve(self.value());
            participants.put(self.bidder_key(), bidder);
        }

        Ok(())
    }
}

impl Transaction for TxCloseAuction {
    /// Verifies integrity of the transaction by checking the transaction
    /// signature.
    fn verify(&self) -> bool {
        // self.verify_signature(self.pub_key())
        true
    }

    /// Adds a new item, id is computed.
    fn execute(&self, view: &mut Fork) -> ExecutionResult {
        let mut schema = AuctionSchema::new(view);

        // Check whether the auction exists.
        let auction = match schema.auction(self.auction_id()) {
            Some(a) => a,
            None => Err(Error::AuctionNotFound)?,
        };

        let auctioner_key = auction.auctioner_key();

        if let Some(bid) = auction.bidding().last() {
            // Settle payment between the winner and the auctioner.
            let mut participants = schema.participants_mut();

            let auctioner = participants.get(auctioner_key).unwrap();
            let winner = participants.get(bid.bidder_key()).unwrap();

            // Release appropriate amount from the winner's reserve
            // and withdraw the funds.
            let winner = winner.release(bid.value()).decrease(bid.value());

            // Withdraw funds from the highest bidder in favour of the auctioner.
            let auctioner = auctioner.increase(bid.value());

            participants.put(auctioner_key, auctioner);
            participants.put(bid.bidder_key(), winner);
        }

        // Close the auction.
        let mut auctions = schema.auctions_mut();
        let auction = auctions.get(self.auction_id()).unwrap();
        let auction = auction.close();

        auctions.set(auction.id(), auction);

        Ok(())
    }
}
// // // // // // // // // // REST API // // // // // // // // // //

/// Container for the service API.
#[derive(Clone)]
struct AuctionApi {
    channel: ApiSender,
    blockchain: Blockchain,
}

/// The structure returned by the REST API.
#[derive(Serialize, Deserialize)]
pub struct TransactionResponse {
    /// Hash of the transaction.
    pub tx_hash: Hash,
}

impl AuctionApi {
    /// Endpoint for getting a participant.
    fn get_participant(&self, req: &mut Request) -> IronResult<Response> {
        let path = req.url.path();
        let participant_key = path.last().unwrap();
        let pub_key = PublicKey::from_hex(participant_key).map_err(|e| {
            IronError::new(
                e,
                (
                    Status::BadRequest,
                    Header(ContentType::json()),
                    "\"Invalid request param: `pub_key`\"",
                ),
            )
        })?;

        let participant = {
            let snapshot = self.blockchain.snapshot();
            let schema = AuctionSchema::new(snapshot);
            schema.participant(&pub_key)
        };

        if let Some(participant) = participant {
            self.ok_response(&serde_json::to_value(participant).unwrap())
        } else {
            self.not_found_response(&serde_json::to_value("Participant not found").unwrap())
        }
    }

    /// Endpoint for dumping all participants from the storage.
    fn get_participants(&self, _: &mut Request) -> IronResult<Response> {
        let snapshot = self.blockchain.snapshot();
        let schema = AuctionSchema::new(snapshot);
        let idx = schema.participants();
        let dump: Vec<Participant> = idx.values().collect();
        self.ok_response(&serde_json::to_value(&dump).unwrap())
    }

    /// Endpoint for getting a participant.
    fn get_item(&self, req: &mut Request) -> IronResult<Response> {
        let path = req.url.path();
        let item_id = path.last().unwrap();

        let item_id = item_id.parse::<u64>().map_err(|e| {
            IronError::new(
                e,
                (
                    Status::BadRequest,
                    Header(ContentType::json()),
                    "\"Invalid request param: `item_id`\"",
                ),
            )
        })?;

        let item = {
            let snapshot = self.blockchain.snapshot();
            let schema = AuctionSchema::new(snapshot);
            schema.item(item_id)
        };
        if let Some(item) = item {
            self.ok_response(&serde_json::to_value(item).unwrap())
        } else {
            self.not_found_response(&serde_json::to_value("Item not found").unwrap())
        }
    }

    /// Endpoint for dumping all items from the storage.
    fn get_items(&self, _: &mut Request) -> IronResult<Response> {
        let snapshot = self.blockchain.snapshot();
        let schema = AuctionSchema::new(snapshot);
        let idx = schema.items();
        let items: Vec<Item> = idx.iter().collect();
        self.ok_response(&serde_json::to_value(&items).unwrap())
    }

    /// Endpoint for getting a participant.
    fn get_auction(&self, req: &mut Request) -> IronResult<Response> {
        let path = req.url.path();
        let auction_id = path.last().unwrap();

        let auction_id = auction_id.parse::<u64>().map_err(|e| {
            IronError::new(
                e,
                (
                    Status::BadRequest,
                    Header(ContentType::json()),
                    "\"Invalid request param: `item_id`\"",
                ),
            )
        })?;

        let auction = {
            let snapshot = self.blockchain.snapshot();
            let schema = AuctionSchema::new(snapshot);
            schema.auction(auction_id)
        };
        if let Some(auction) = auction {
            self.ok_response(&serde_json::to_value(auction).unwrap())
        } else {
            self.not_found_response(&serde_json::to_value("Auction not found").unwrap())
        }
    }

    /// Endpoint for dumping all auctions from the storage.
    fn get_auctions(&self, _: &mut Request) -> IronResult<Response> {
        let snapshot = self.blockchain.snapshot();
        let schema = AuctionSchema::new(snapshot);
        let idx = schema.auctions();
        let auctions: Vec<Auction> = idx.iter().collect();
        self.ok_response(&serde_json::to_value(&auctions).unwrap())
    }

    /// Common processing for transaction-accepting endpoints.
    fn post_transaction(&self, req: &mut Request) -> IronResult<Response> {
        match req.get::<bodyparser::Struct<AuctionTransactions>>() {
            Ok(Some(transaction)) => {
                let transaction: Box<Transaction> = transaction.into();
                let tx_hash = transaction.hash();
                self.channel.send(transaction).map_err(ApiError::from)?;
                let json = TransactionResponse { tx_hash };
                self.ok_response(&serde_json::to_value(&json).unwrap())
            }
            Ok(None) => Err(ApiError::BadRequest("Empty request body".into()))?,
            Err(e) => Err(ApiError::BadRequest(e.to_string()))?,
        }
    }
}

/// `Api` trait implementation.
///
/// `Api` facilitates conversion between transactions/read requests and REST
/// endpoints; for example, it parses `POST`ed JSON into the binary transaction
/// representation used in Exonum internally.
impl Api for AuctionApi {
    fn wire(&self, router: &mut Router) {
        let self_ = self.clone();
        let get_participant = move |req: &mut Request| self_.get_participant(req);
        let self_ = self.clone();
        let get_participants = move |req: &mut Request| self_.get_participants(req);
        let self_ = self.clone();
        let post_new_participant = move |req: &mut Request| self_.post_transaction(req);

        let self_ = self.clone();
        let get_item = move |req: &mut Request| self_.get_item(req);
        let self_ = self.clone();
        let get_items = move |req: &mut Request| self_.get_items(req);
        let self_ = self.clone();
        let post_new_item = move |req: &mut Request| self_.post_transaction(req);

        let self_ = self.clone();
        let get_auction = move |req: &mut Request| self_.get_auction(req);
        let self_ = self.clone();
        let get_auctions = move |req: &mut Request| self_.get_auctions(req);
        let self_ = self.clone();
        let post_new_auction = move |req: &mut Request| self_.post_transaction(req);
        let self_ = self.clone();
        let post_new_bid = move |req: &mut Request| self_.post_transaction(req);
        let self_ = self.clone();
        let post_close_auction = move |req: &mut Request| self_.post_transaction(req);

        // Bind handlers to specific routes.
        router.get(
            "/v1/participant/:pub_key",
            get_participant,
            "get_participant",
        );
        router.get("/v1/participants", get_participants, "get_participants");
        router.post(
            "/v1/participants",
            post_new_participant,
            "post_new_participant",
        );

        router.get("/v1/item/:id", get_item, "get_item");
        router.get("/v1/items", get_items, "get_items");
        router.post("/v1/items", post_new_item, "post_new_item");

        router.get("/v1/auction/:id", get_auction, "get_auction");
        router.get("/v1/auctions", get_auctions, "get_auctions");
        router.post("/v1/auctions", post_new_auction, "post_new_auction");
        router.post("/v1/bid", post_new_bid, "post_new_bid");
        router.post(
            "/v1/auction/close",
            post_close_auction,
            "post_close_auction",
        );
    }
}

// // // // // // // // // // SERVICE DECLARATION // // // // // // // // // //

pub struct AuctionService;

impl AuctionService {
    pub fn new() -> AuctionService {
        AuctionService {}
    }
}

impl Service for AuctionService {
    fn service_name(&self) -> &str {
        SERVICE_NAME
    }

    fn service_id(&self) -> u16 {
        SERVICE_ID
    }

    // Implement a method to deserialize transactions coming to the node.
    fn tx_from_raw(&self, raw: RawTransaction) -> Result<Box<Transaction>, encoding::Error> {
        let tx = AuctionTransactions::tx_from_raw(raw)?;
        Ok(tx.into())
    }

    // fn handle_commit(&self, context: &ServiceContext) {
    //     println!("{:#?}\n", context.snapshot().);
    // }
    // Hashes for the service tables that will be included into the state hash.
    // To simplify things, we don't have [Merkelized tables][merkle] in the service storage
    // for now, so we return an empty vector.
    //
    // [merkle]: https://exonum.com/doc/architecture/storage/#merklized-indices
    fn state_hash(&self, _: &Snapshot) -> Vec<Hash> {
        vec![]
    }

    // Create a REST `Handler` to process web requests to the node.
    fn public_api_handler(&self, ctx: &ApiContext) -> Option<Box<Handler>> {
        let mut router = Router::new();
        let api = AuctionApi {
            channel: ctx.node_channel().clone(),
            blockchain: ctx.blockchain().clone(),
        };
        api.wire(&mut router);
        Some(Box::new(router))
    }
}
