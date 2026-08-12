//! The account's package licenses, read from Steam's unprompted post-login push.
//!
//! Steam sends `CMsgClientLicenseList` (`k_EMsgClientLicenseList` = 780,
//! `protos/steam/enums_clientserver.proto`) once, unprompted, right after
//! logon: the full set of packages the account owns. There is no request
//! message for it and it is not re-pushed on change within a session, so
//! [`licenses`] only ever reads the cached post-login push and never blocks
//! waiting for one.

use prost::Message as _;

use crate::proto::CMsgClientLicenseList;
use crate::session::SessionHandle;

/// `k_EMsgClientLicenseList`, `protos/steam/enums_clientserver.proto`.
const EMSG_CLIENT_LICENSE_LIST: u32 = 780;

/// One package license on the account.
///
/// `CMsgClientLicenseList.License` carries eighteen fields; only the three
/// most broadly useful are surfaced here. `#[non_exhaustive]` leaves room to
/// add more later without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct License {
    /// The owned package id (`packageID`, resolvable against Steam's package
    /// data for what it grants).
    pub package_id: u32,
    /// Unix timestamp the license was granted.
    pub time_created: u32,
    /// `EPaymentMethod` numeric value (kept as the proto's own `uint32` rather
    /// than an enum decode).
    pub payment_method: u32,
}

/// The account's package licenses, from the post-login `CMsgClientLicenseList`
/// push.
///
/// This is a method, not a subscription: it reads the cached snapshot
/// [`SessionHandle::cached_snapshot`] keeps for this emsg and returns
/// immediately. It never blocks, and it never asks Steam for anything since
/// there is no request message for the license list.
///
/// Returns `None` only if the push hasn't arrived yet this logon. An account
/// with zero licenses (implausible for a real Steam account, but not
/// impossible) reads as `Some(vec![])`, distinct from "no push yet".
pub fn licenses(session: &SessionHandle) -> Option<Vec<License>> {
    let body = session.cached_snapshot(EMSG_CLIENT_LICENSE_LIST)?;
    let list = CMsgClientLicenseList::decode(body.as_slice()).ok()?;
    Some(licenses_from(list))
}

/// Whether the account owns `package_id`, from the same cached push
/// [`licenses`] reads.
///
/// `None` under the same condition as [`licenses`]: no push has arrived yet.
pub fn owns_package(session: &SessionHandle, package_id: u32) -> Option<bool> {
    let list = licenses(session)?;
    Some(list.iter().any(|l| l.package_id == package_id))
}

fn licenses_from(list: CMsgClientLicenseList) -> Vec<License> {
    list.licenses
        .into_iter()
        .filter_map(|l| {
            Some(License {
                package_id: l.package_id?,
                time_created: l.time_created.unwrap_or(0),
                payment_method: l.payment_method.unwrap_or(0),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::c_msg_client_license_list::License as ProtoLicense;
    use crate::session::SessionHandle;

    const SELF_ID: u64 = 76_561_198_000_000_000;

    fn proto_license(package_id: u32) -> ProtoLicense {
        ProtoLicense {
            package_id: Some(package_id),
            time_created: Some(1_700_000_000),
            payment_method: Some(1),
            ..Default::default()
        }
    }

    #[test]
    fn licenses_from_maps_the_three_surfaced_fields() {
        let list = CMsgClientLicenseList {
            licenses: vec![proto_license(17_906)],
            ..Default::default()
        };
        let mapped = licenses_from(list);
        assert_eq!(mapped.len(), 1);
        assert_eq!(mapped[0].package_id, 17_906);
        assert_eq!(mapped[0].time_created, 1_700_000_000);
        assert_eq!(mapped[0].payment_method, 1);
    }

    #[test]
    fn licenses_from_skips_an_entry_with_no_package_id() {
        // Proto2 optional: a License without even a package id is unusable,
        // unlike a missing time/payment_method which just defaults to 0.
        let list = CMsgClientLicenseList {
            licenses: vec![ProtoLicense::default()],
            ..Default::default()
        };
        assert!(licenses_from(list).is_empty());
    }

    #[test]
    fn licenses_is_none_before_any_push() {
        let (handle, _commands, _events, _snapshots) = SessionHandle::for_test(SELF_ID);
        assert!(licenses(&handle).is_none());
    }

    #[test]
    fn licenses_reads_the_cached_push() {
        let (handle, _commands, _events, snapshots) = SessionHandle::for_test(SELF_ID);
        let body = CMsgClientLicenseList {
            licenses: vec![proto_license(730), proto_license(17_906)],
            ..Default::default()
        }
        .encode_to_vec();
        snapshots
            .lock()
            .expect("snapshot cache mutex")
            .insert(EMSG_CLIENT_LICENSE_LIST, body);

        let list = licenses(&handle).expect("cached push answers");
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn owns_package_is_none_before_any_push() {
        let (handle, _commands, _events, _snapshots) = SessionHandle::for_test(SELF_ID);
        assert!(owns_package(&handle, 730).is_none());
    }

    #[test]
    fn owns_package_true_and_false() {
        let (handle, _commands, _events, snapshots) = SessionHandle::for_test(SELF_ID);
        let body = CMsgClientLicenseList {
            licenses: vec![proto_license(730)],
            ..Default::default()
        }
        .encode_to_vec();
        snapshots
            .lock()
            .expect("snapshot cache mutex")
            .insert(EMSG_CLIENT_LICENSE_LIST, body);

        assert_eq!(owns_package(&handle, 730), Some(true));
        assert_eq!(owns_package(&handle, 999), Some(false));
    }
}
