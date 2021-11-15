use crate::error::{OneYoctoAttachError, PrivateCallError};
use aurora_engine_types::account_id::AccountId;

/// Timestamp represented by the number of nanoseconds since the Unix Epoch.
pub struct Timestamp(u64);

impl Timestamp {
    pub fn new(ns: u64) -> Self {
        Self(ns)
    }

    pub fn nanos(&self) -> u64 {
        self.0
    }

    pub fn millis(&self) -> u64 {
        self.0 / 1_000_000
    }

    pub fn secs(&self) -> u64 {
        self.0 / 1_000_000_000
    }
}

/// Returns information about the NEAR context in which the
/// transaction is executing. In the case of a standalone binary,
/// independent of NEAR these values would need to be mocked or otherwise
/// passed in from an external source.
pub trait Env {
    /// Account ID that signed the transaction.
    fn signer_account_id(&self) -> AccountId;
    /// Account ID of the currently executing contract.
    fn current_account_id(&self) -> AccountId;
    /// Account ID which called the current contract.
    fn predecessor_account_id(&self) -> AccountId;
    /// Height of the current block.
    fn block_height(&self) -> u64;
    /// Timestamp (in ns) of the current block.
    fn block_timestamp(&self) -> Timestamp;
    /// Amount of NEAR attached to current call
    fn attached_deposit(&self) -> u128;

    fn assert_private_call(&self) -> Result<(), PrivateCallError> {
        if self.predecessor_account_id() == self.current_account_id() {
            Ok(())
        } else {
            Err(PrivateCallError)
        }
    }

    fn assert_one_yocto(&self) -> Result<(), OneYoctoAttachError> {
        if self.attached_deposit() == 1 {
            Ok(())
        } else {
            Err(OneYoctoAttachError)
        }
    }
}
