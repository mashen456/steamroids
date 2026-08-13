//! Account wallet balance, read from Steam's unprompted push.
//!
//! Steam sends `CMsgClientWalletInfoUpdate` (`k_EMsgClientWalletInfoUpdate` =
//! 5528, `protos/steam/enums_clientserver.proto`) unprompted after every logon
//! and again whenever the balance changes. There is no request message for
//! it, so [`wallet`] only ever reads the most recently cached push and never
//! blocks waiting for one to arrive.

use prost::Message as _;

use crate::proto::CMsgClientWalletInfoUpdate;
use crate::session::SessionHandle;

/// `k_EMsgClientWalletInfoUpdate`, `protos/steam/enums_clientserver.proto`.
const EMSG_CLIENT_WALLET_INFO_UPDATE: u32 = 5528;

/// Account wallet balance, from the most recent `CMsgClientWalletInfoUpdate`
/// push.
///
/// Steam ships the balance as both a 32-bit field and a wider 64-bit field
/// carrying the same figure (the proto's own comment on `balance64` marks it
/// `php_output_always_number`, i.e. added so large balances survive JSON
/// round-tripping); this struct exposes the wider one, falling back to the
/// 32-bit field on the rare push that only sets that. Same pairing for the
/// pending amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Wallet {
    /// Current balance, in the wallet currency's minor unit (e.g. cents for
    /// USD). Not every currency has a minor unit the same size (or one at
    /// all), so treat this as "whatever unit Steam's `balance` field counts
    /// in" rather than assuming cents specifically.
    pub balance_minor_units: i64,
    /// Balance not yet available, e.g. a refund or top-up still clearing.
    /// Same unit as `balance_minor_units`.
    pub pending_minor_units: i64,
    /// `ECurrencyCode` numeric value (USD = 1, GBP = 2, ...).
    pub currency: i32,
}

/// The account's wallet balance, as of the most recent
/// `CMsgClientWalletInfoUpdate` push this logon.
///
/// This is a method, not a subscription: it reads the cache
/// [`SessionHandle::cached_snapshot`] keeps for this emsg and returns
/// immediately. It never blocks, and it never asks Steam for anything since
/// Steam has no request message for wallet info.
///
/// Returns `None` if no push has arrived yet on this logon, or if the push
/// says the account has no wallet at all.
pub fn wallet(session: &SessionHandle) -> Option<Wallet> {
    let body = session.cached_snapshot(EMSG_CLIENT_WALLET_INFO_UPDATE)?;
    let update = CMsgClientWalletInfoUpdate::decode(body.as_slice()).ok()?;
    wallet_from(update)
}

/// `None` when Steam explicitly reported no wallet (`has_wallet = false`).
fn wallet_from(update: CMsgClientWalletInfoUpdate) -> Option<Wallet> {
    if update.has_wallet == Some(false) {
        return None;
    }
    Some(Wallet {
        balance_minor_units: update
            .balance64
            .or_else(|| update.balance.map(i64::from))
            .unwrap_or(0),
        pending_minor_units: update
            .balance64_delayed
            .or_else(|| update.balance_delayed.map(i64::from))
            .unwrap_or(0),
        currency: update.currency.unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionHandle;

    const SELF_ID: u64 = 76_561_198_000_000_000;

    #[test]
    fn wallet_from_prefers_the_64_bit_balance() {
        let update = CMsgClientWalletInfoUpdate {
            has_wallet: Some(true),
            balance: Some(100),
            balance64: Some(9_999_999_999),
            currency: Some(1),
            ..Default::default()
        };
        let w = wallet_from(update).expect("has_wallet true");
        assert_eq!(w.balance_minor_units, 9_999_999_999);
        assert_eq!(w.currency, 1);
    }

    #[test]
    fn wallet_from_falls_back_to_the_32_bit_fields() {
        let update = CMsgClientWalletInfoUpdate {
            has_wallet: Some(true),
            balance: Some(500),
            balance_delayed: Some(50),
            currency: Some(2),
            ..Default::default()
        };
        let w = wallet_from(update).expect("has_wallet true");
        assert_eq!(w.balance_minor_units, 500);
        assert_eq!(w.pending_minor_units, 50);
    }

    #[test]
    fn wallet_from_is_none_when_steam_says_no_wallet() {
        let update = CMsgClientWalletInfoUpdate {
            has_wallet: Some(false),
            balance: Some(500),
            ..Default::default()
        };
        assert!(wallet_from(update).is_none());
    }

    #[test]
    fn wallet_is_none_before_any_push() {
        let (handle, _commands, _events, _snapshots) = SessionHandle::for_test(SELF_ID);
        assert!(wallet(&handle).is_none());
    }

    #[test]
    fn wallet_reads_the_cached_push() {
        let (handle, _commands, _events, snapshots) = SessionHandle::for_test(SELF_ID);
        let body = CMsgClientWalletInfoUpdate {
            has_wallet: Some(true),
            balance64: Some(1_000),
            currency: Some(1),
            ..Default::default()
        }
        .encode_to_vec();
        snapshots
            .lock()
            .expect("snapshot cache mutex")
            .insert(EMSG_CLIENT_WALLET_INFO_UPDATE, body);

        let w = wallet(&handle).expect("cached push answers");
        assert_eq!(w.balance_minor_units, 1_000);
        assert_eq!(w.currency, 1);
    }
}
