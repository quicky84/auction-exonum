// Copyright 2018 The Exonum Team
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The time oracle service for Exonum.
//!
//! See [the Exonum documentation][docs:time] for a high-level overview of the service,
//! in particular, its design rationale and the proof of correctness.
//!
//! [docs:time]: https://exonum.com/doc/advanced/time

// #![deny(missing_debug_implementations, missing_docs)]

extern crate bodyparser;
extern crate chrono;
#[macro_use]
extern crate exonum;
#[macro_use]
extern crate failure;
extern crate iron;
extern crate router;
extern crate serde;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate serde_json;

use iron::prelude::*;
use iron::Handler;
use router::Router;
use chrono::{DateTime, Duration, TimeZone, Utc};

use std::sync::{Arc, RwLock};

use exonum::blockchain::{ApiContext, Blockchain, ExecutionError, ExecutionResult, Schema, Service,
                         ServiceContext, Transaction, TransactionSet};
use exonum::messages::{Message, RawTransaction};
use exonum::encoding::serialize::json::reexport::Value;
use exonum::storage::{Entry, Fork, ProofMapIndex, Snapshot};
use exonum::crypto::{Hash, PublicKey};
use exonum::encoding;
use exonum::helpers::fabric::{Context, ServiceFactory};
use exonum::api::Api;

// use std::collections::HashMap;

/// Time service id.
const SERVICE_ID: u16 = 4;
/// Time service name.
const SERVICE_NAME: &str = "exonum_time";

/// `Exonum-time` service database schema.
#[derive(Debug)]
pub struct TimeSchema<T> {
    view: T,
}

impl<T: AsRef<Snapshot>> TimeSchema<T> {
    /// Constructs schema for the given `snapshot`.
    pub fn new(view: T) -> Self {
        TimeSchema { view }
    }

    /// Returns the table that stores `SystemTime` for every validator.
    pub fn validators_times(&self) -> ProofMapIndex<&Snapshot, PublicKey, DateTime<Utc>> {
        ProofMapIndex::new(
            format!("{}.validators_times", SERVICE_NAME),
            self.view.as_ref(),
        )
    }

    /// Returns stored time.
    pub fn time(&self) -> Entry<&Snapshot, DateTime<Utc>> {
        Entry::new(format!("{}.time", SERVICE_NAME), self.view.as_ref())
    }

    /// Returns hashes for stored tables.
    pub fn state_hash(&self) -> Vec<Hash> {
        vec![self.validators_times().merkle_root(), self.time().hash()]
    }
}

impl<'a> TimeSchema<&'a mut Fork> {
    /// Mutable reference to the ['validators_times'][1] index.
    ///
    /// [1]: struct.TimeSchema.html#method.validators_times
    pub fn validators_times_mut(&mut self) -> ProofMapIndex<&mut Fork, PublicKey, DateTime<Utc>> {
        ProofMapIndex::new(format!("{}.validators_times", SERVICE_NAME), self.view)
    }

    /// Mutable reference to the ['time'][1] index.
    ///
    /// [1]: struct.TimeSchema.html#method.time
    pub fn time_mut(&mut self) -> Entry<&mut Fork, DateTime<Utc>> {
        Entry::new(format!("{}.time", SERVICE_NAME), self.view)
    }
}

transactions! {
    TimeTransactions {
        const SERVICE_ID = SERVICE_ID;

        /// Transaction that is sent by the validator after the commit of the block.
        struct TxTime {
            /// Time of the validator.
            time: DateTime<Utc>,
            /// Public key of the validator.
            pub_key: &PublicKey,
        }
    }
}

/// Common errors emitted by transactions during execution.
#[derive(Debug, Fail)]
#[repr(u8)]
pub enum Error {
    /// The sender of the transaction is not among the active validators.
    #[fail(display = "Not authored by a validator")]
    UnknownSender = 0,

    /// The validator time that is stored in storage is greater than the proposed one.
    #[fail(display = "The validator time is greater than the proposed one")]
    ValidatorTimeIsGreater = 1,
}

impl From<Error> for ExecutionError {
    fn from(value: Error) -> ExecutionError {
        ExecutionError::new(value as u8)
    }
}

impl TxTime {
    fn check_signed_by_validator(&self, snapshot: &Snapshot) -> ExecutionResult {
        let keys = Schema::new(&snapshot).actual_configuration().validator_keys;
        let signed = keys.iter().any(|k| k.service_key == *self.pub_key());
        if !signed {
            Err(Error::UnknownSender)?
        } else {
            Ok(())
        }
    }

    fn update_validator_time(&self, fork: &mut Fork) -> ExecutionResult {
        let mut schema = TimeSchema::new(fork);
        match schema.validators_times().get(self.pub_key()) {
            // The validator time in the storage should be less than in the transaction.
            Some(time) if time >= self.time() => Err(Error::ValidatorTimeIsGreater)?,
            // Write the time for the validator.
            _ => {
                schema
                    .validators_times_mut()
                    .put(self.pub_key(), self.time());
                Ok(())
            }
        }
    }

    fn update_consolidated_time(fork: &mut Fork) {
        let keys = Schema::new(&fork).actual_configuration().validator_keys;
        let mut schema = TimeSchema::new(fork);

        // Find all known times for the validators.
        let validator_times = {
            let idx = schema.validators_times();
            let mut times = idx.iter()
                .filter_map(|(public_key, time)| {
                    keys.iter()
                        .find(|validator| validator.service_key == public_key)
                        .map(|_| time)
                })
                .collect::<Vec<_>>();
            // Ordering time from highest to lowest.
            times.sort_by(|a, b| b.cmp(a));
            times
        };

        // The largest number of Byzantine nodes.
        let max_byzantine_nodes = (keys.len() - 1) / 3;
        if validator_times.len() <= 2 * max_byzantine_nodes {
            return;
        }

        match schema.time().get() {
            // Selected time should be greater than the time in the storage.
            Some(current_time) if current_time >= validator_times[max_byzantine_nodes] => {
                return;
            }
            _ => {
                // Change the time in the storage.
                schema.time_mut().set(validator_times[max_byzantine_nodes]);
            }
        }
    }
}

impl Transaction for TxTime {
    fn verify(&self) -> bool {
        self.verify_signature(self.pub_key())
    }

    fn execute(&self, view: &mut Fork) -> ExecutionResult {
        self.check_signed_by_validator(view.as_ref())?;
        self.update_validator_time(view)?;
        Self::update_consolidated_time(view);
        Ok(())
    }
}

/// Implements the node API.
#[derive(Clone)]
struct TimeApi {
    blockchain: Blockchain,
}

/// Structure for saving public key of the validator and last known local time.
#[derive(Debug, Serialize, Deserialize)]
pub struct ValidatorTime {
    /// Public key of the validator.
    pub public_key: PublicKey,
    /// Time of the validator.
    pub time: Option<DateTime<Utc>>,
}

/// Shortcut to get data from storage.
impl TimeApi {
    /// Endpoint for getting value of the time that is saved in storage.
    fn get_current_time(&self, _: &mut Request) -> IronResult<Response> {
        let view = self.blockchain.snapshot();
        let schema = TimeSchema::new(&view);
        self.ok_response(&json!(schema.time().get()))
    }

    /// Endpoint for getting time values for all validators.
    fn get_all_validators_times(&self, _: &mut Request) -> IronResult<Response> {
        let view = self.blockchain.snapshot();
        let schema = TimeSchema::new(&view);
        let idx = schema.validators_times();

        // The times of all validators for which time is known.
        let validators_times = idx.iter()
            .map(|(public_key, time)| ValidatorTime {
                public_key,
                time: Some(time),
            })
            .collect::<Vec<_>>();

        self.ok_response(&serde_json::to_value(validators_times).unwrap())
    }

    /// Endpoint for getting time values for current validators.
    fn get_current_validators_times(&self, _: &mut Request) -> IronResult<Response> {
        let view = self.blockchain.snapshot();
        let validator_keys = Schema::new(&view).actual_configuration().validator_keys;
        let schema = TimeSchema::new(&view);
        let idx = schema.validators_times();

        // The times of current validators.
        // `None` if the time of the validator is unknown.
        let validators_times = validator_keys
            .iter()
            .map(|validator| ValidatorTime {
                public_key: validator.service_key,
                time: idx.get(&validator.service_key),
            })
            .collect::<Vec<_>>();

        self.ok_response(&serde_json::to_value(validators_times).unwrap())
    }

    fn wire_private(&self, router: &mut Router) {
        let self_ = self.clone();
        let get_current_validators_times =
            move |req: &mut Request| self_.get_current_validators_times(req);

        let self_ = self.clone();
        let get_all_validators_times = move |req: &mut Request| self_.get_all_validators_times(req);

        router.get(
            "v1/validators_times",
            get_current_validators_times,
            "get_current_validators_times",
        );

        router.get(
            "v1/validators_times/all",
            get_all_validators_times,
            "get_all_validators_times",
        );
    }
}

impl Api for TimeApi {
    fn wire(&self, router: &mut Router) {
        let self_ = self.clone();
        let get_current_time = move |req: &mut Request| self_.get_current_time(req);
        router.get("v1/current_time", get_current_time, "get_current_time");
    }
}

/// A helper trait that provides the node with a current time.
pub trait TimeProvider: Send + Sync + ::std::fmt::Debug {
    /// Returns the current time.
    fn current_time(&self) -> DateTime<Utc>;
}

#[derive(Debug)]
struct SystemTimeProvider;

impl TimeProvider for SystemTimeProvider {
    fn current_time(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Mock time provider for service testing.
///
/// In terms of use, the mock time provider is similar to [`Arc`]; that is, clones of the provider
/// control the same time record as the original instance. Therefore, to use the mock provider,
/// one may clone its instance and use the clone to construct a [`TimeService`],
/// while keeping the original instance to adjust the time reported to the validators
/// along various test scenarios.
///
/// # Examples
///
/// ```
/// # extern crate exonum;
/// # extern crate exonum_testkit;
/// # extern crate exonum_time;
/// # extern crate chrono;
/// use chrono::{Utc, Duration, TimeZone};
/// use exonum::helpers::Height;
/// use exonum_testkit::TestKitBuilder;
/// use exonum_time::{MockTimeProvider, TimeSchema, TimeService};
///
/// # fn main() {
/// let mock_provider = MockTimeProvider::default();
/// let mut testkit = TestKitBuilder::validator()
///     .with_service(TimeService::with_provider(mock_provider.clone()))
///     .create();
/// mock_provider.add_time(Duration::seconds(15));
/// testkit.create_blocks_until(Height(2));
///
/// // The time reported by the mock time provider is reflected by the service.
/// let snapshot = testkit.snapshot();
/// let schema = TimeSchema::new(snapshot);
/// assert_eq!(
///     Some(Utc.timestamp(15, 0)),
///     schema.time().get().map(|time| time)
/// );
/// # }
/// ```
///
/// [`Arc`]: https://doc.rust-lang.org/std/sync/struct.Arc.html
/// [`TimeService`]: struct.TimeService.html
#[derive(Debug, Clone)]
pub struct MockTimeProvider {
    /// Local time value.
    time: Arc<RwLock<DateTime<Utc>>>,
}

impl Default for MockTimeProvider {
    /// Initializes the provider with the time set to the Unix epoch start.
    fn default() -> Self {
        Self::new(Utc.timestamp(0, 0))
    }
}

impl MockTimeProvider {
    /// Creates a new `MockTimeProvider` with time value equal to `time`.
    pub fn new(time: DateTime<Utc>) -> Self {
        Self {
            time: Arc::new(RwLock::new(time)),
        }
    }

    /// Gets the time value currently reported by the provider.
    pub fn time(&self) -> DateTime<Utc> {
        *self.time.read().unwrap()
    }

    /// Sets the time value to `new_time`.
    pub fn set_time(&self, new_time: DateTime<Utc>) {
        let mut time = self.time.write().unwrap();
        *time = new_time;
    }

    /// Adds `duration` to the value of `time`.
    pub fn add_time(&self, duration: Duration) {
        let mut time = self.time.write().unwrap();
        *time = *time + duration;
    }
}

impl TimeProvider for MockTimeProvider {
    fn current_time(&self) -> DateTime<Utc> {
        self.time()
    }
}

impl From<MockTimeProvider> for Box<TimeProvider> {
    fn from(mock_time_provider: MockTimeProvider) -> Self {
        Box::new(mock_time_provider) as Box<TimeProvider>
    }
}

pub type ConsolidatedTime = DateTime<Utc>;

pub trait TimeSubscriber: Send + Sync {
    fn call(&self, ConsolidatedTime);
}

impl<T> TimeSubscriber for T
where
    T: Fn(ConsolidatedTime) + Send + Sync,
{
    fn call(&self, time: ConsolidatedTime) {
        (*self)(time)
    }
}

/// Define the service.
pub struct TimeService {
    /// Current time.
    time: Box<TimeProvider>,
    subscribers: Vec<Box<TimeSubscriber>>,
}

impl Default for TimeService {
    fn default() -> TimeService {
        TimeService::with_provider(Box::new(SystemTimeProvider) as Box<TimeProvider>)
    }
}

impl TimeService {
    /// Create a new `TimeService`.
    pub fn new() -> TimeService {
        TimeService::default()
    }

    /// Create a new `TimeService` with time provider `T`.
    pub fn with_provider<T: Into<Box<TimeProvider>>>(time_provider: T) -> TimeService {
        TimeService {
            time: time_provider.into(),
            subscribers: vec![],
        }
    }

    pub fn subscribe<T>(&mut self, subscriber: T)
    where
        T: TimeSubscriber + 'static,
    {
        self.subscribers.push(Box::new(subscriber));
    }
}

impl Service for TimeService {
    fn service_name(&self) -> &str {
        SERVICE_NAME
    }

    fn state_hash(&self, snapshot: &Snapshot) -> Vec<Hash> {
        let schema = TimeSchema::new(snapshot);
        schema.state_hash()
    }

    fn service_id(&self) -> u16 {
        SERVICE_ID
    }

    fn tx_from_raw(&self, raw: RawTransaction) -> Result<Box<Transaction>, encoding::Error> {
        let tx = TimeTransactions::tx_from_raw(raw)?;
        Ok(tx.into())
    }

    fn initialize(&self, _fork: &mut Fork) -> Value {
        Value::Null
    }

    /// Creates transaction after commit of the block.
    fn handle_commit(&self, context: &ServiceContext) {
        // The transaction must be created by the validator.
        if context.validator_id().is_none() {
            return;
        }

        // Call subscribers with the current time.
        for s in &self.subscribers {
            s.call(self.time.current_time());
        }

        let (pub_key, sec_key) = (*context.public_key(), context.secret_key().clone());
        context
            .transaction_sender()
            .send(Box::new(TxTime::new(
                self.time.current_time(),
                &pub_key,
                &sec_key,
            )))
            .unwrap();
    }

    fn private_api_handler(&self, ctx: &ApiContext) -> Option<Box<Handler>> {
        let mut router = Router::new();
        let api = TimeApi {
            blockchain: ctx.blockchain().clone(),
        };
        api.wire_private(&mut router);
        Some(Box::new(router))
    }

    fn public_api_handler(&self, ctx: &ApiContext) -> Option<Box<Handler>> {
        let mut router = Router::new();
        let api = TimeApi {
            blockchain: ctx.blockchain().clone(),
        };
        api.wire(&mut router);
        Some(Box::new(router))
    }
}

/// A time service creator for the `NodeBuilder`.
#[derive(Debug)]
pub struct TimeServiceFactory;

impl ServiceFactory for TimeServiceFactory {
    fn make_service(&mut self, _: &Context) -> Box<Service> {
        Box::new(TimeService::new())
    }
}
