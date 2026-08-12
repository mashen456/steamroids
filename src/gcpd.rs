//! CS2 competitive matchmaking cooldown from the Steam GCPD.
//!
//! Pure HTML extraction, no network: [`crate::web`] fetches the page, this
//! module reads it. GCPD renders every table on the page (cooldown, casual
//! matchmaking stats, deathmatch stats, ...) with the same `generic_kv_table`
//! class and no id or wrapper to tell them apart, so the cooldown table is
//! found by matching its header row rather than by its position on the page.

use crate::{Error, Result};

/// A CS2 competitive matchmaking cooldown read from a GCPD page.
#[derive(Debug, Clone)]
pub struct Cs2Cooldown {
    /// Cooldown expiry, as a Unix timestamp (UTC).
    pub expires_at_unix: i64,
    /// The expiry exactly as GCPD rendered it, e.g. `"2026-08-17 23:54:16 GMT"`.
    pub expires_at_raw: String,
    /// Cooldown level, when GCPD reports one. A real active cooldown has been
    /// observed with this cell blank, so it is optional.
    pub level: Option<u32>,
    /// Whether the account has acknowledged the cooldown.
    pub acknowledged: bool,
}

const COOLDOWN_HEADERS: [&str; 3] = [
    "Competitive Cooldown Expiration",
    "Competitive Cooldown Level",
    "Acknowledged",
];

/// Parse the CS2 competitive cooldown out of a GCPD page.
///
/// `html` is expected to be the body of an account's GCPD
/// `?tab=matchmaking` page.
///
/// `Ok(None)` means the cooldown table itself is absent from the page. GCPD
/// renders no such table at all for a clean account, so this is the normal
/// case, not a parse failure.
///
/// # Errors
///
/// [`Error::Codec`] if the cooldown table is present but its expiry cell
/// does not parse as `YYYY-MM-DD HH:MM:SS GMT`, or its level cell is
/// non-blank and not a valid number.
pub fn parse_cooldown(html: &str) -> Result<Option<Cs2Cooldown>> {
    let Some(table) = find_cooldown_table(html) else {
        return Ok(None);
    };

    let cells = extract_all(table, "td");

    let raw = cells.first().copied().unwrap_or("").trim();
    let expires_at_unix = parse_gmt_timestamp(raw)?;

    let level = match cell_value(cells.get(1).copied()) {
        Some(v) => Some(
            v.parse::<u32>()
                .map_err(|e| Error::Codec(format!("cooldown level {v:?}: {e}")))?,
        ),
        None => None,
    };

    let acknowledged =
        cell_value(cells.get(2).copied()).is_some_and(|v| v.eq_ignore_ascii_case("yes"));

    Ok(Some(Cs2Cooldown {
        expires_at_unix,
        expires_at_raw: raw.to_string(),
        level,
        acknowledged,
    }))
}

// scan for the table whose header row is exactly the cooldown headers
fn find_cooldown_table(html: &str) -> Option<&str> {
    const OPEN: &str = "<table class=\"generic_kv_table\">";
    const CLOSE: &str = "</table>";

    let mut pos = 0;
    while let Some(rel) = html[pos..].find(OPEN) {
        let start = pos + rel;
        let body_start = start + OPEN.len();
        let close_rel = html[body_start..].find(CLOSE)?;
        let end = body_start + close_rel + CLOSE.len();
        let table = &html[start..end];

        let headers: Vec<&str> = extract_all(table, "th").iter().map(|h| h.trim()).collect();
        if headers.as_slice() == COOLDOWN_HEADERS.as_slice() {
            return Some(table);
        }

        pos = end;
    }
    None
}

// text between every <tag>...</tag> pair, in document order, no nesting
fn extract_all<'a>(html: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");

    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(rel) = html[pos..].find(&open) {
        let start = pos + rel + open.len();
        let Some(close_rel) = html[start..].find(&close) else {
            break;
        };
        let end = start + close_rel;
        out.push(&html[start..end]);
        pos = end + close.len();
    }
    out
}

// trim; &nbsp; and empty are absent
fn cell_value(raw: Option<&str>) -> Option<&str> {
    let v = raw?.trim();
    if v.is_empty() || v == "&nbsp;" {
        None
    } else {
        Some(v)
    }
}

fn parse_gmt_timestamp(raw: &str) -> Result<i64> {
    fn bad(raw: &str) -> Error {
        Error::Codec(format!(
            "cooldown expiry {raw:?}: expected YYYY-MM-DD HH:MM:SS GMT"
        ))
    }

    let rest = raw.strip_suffix(" GMT").ok_or_else(|| bad(raw))?;
    let (date, time) = rest.split_once(' ').ok_or_else(|| bad(raw))?;

    let mut date_parts = date.split('-');
    let (Some(y), Some(m), Some(d), None) = (
        date_parts.next(),
        date_parts.next(),
        date_parts.next(),
        date_parts.next(),
    ) else {
        return Err(bad(raw));
    };

    let mut time_parts = time.split(':');
    let (Some(hh), Some(mm), Some(ss), None) = (
        time_parts.next(),
        time_parts.next(),
        time_parts.next(),
        time_parts.next(),
    ) else {
        return Err(bad(raw));
    };

    let y: i64 = y.parse().map_err(|_| bad(raw))?;
    let m: i64 = m.parse().map_err(|_| bad(raw))?;
    let d: i64 = d.parse().map_err(|_| bad(raw))?;
    let hh: i64 = hh.parse().map_err(|_| bad(raw))?;
    let mm: i64 = mm.parse().map_err(|_| bad(raw))?;
    let ss: i64 = ss.parse().map_err(|_| bad(raw))?;

    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mm > 59 || ss > 59 {
        return Err(bad(raw));
    }

    let days = days_from_civil(y, m, d);
    Ok(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

// days from civil, Howard Hinnant's algorithm. valid for any gregorian date.
// names (era, yoe, doy, doe) match the reference algorithm, kept for verifiability.
#[allow(clippy::similar_names)]
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    const COOLDOWN: &str = include_str!("../tests/fixtures/gcpd_cooldown.html");
    const NO_COOLDOWN: &str = include_str!("../tests/fixtures/gcpd_no_cooldown.html");

    #[test]
    fn days_from_civil_matches_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
    }

    #[test]
    fn parses_an_active_cooldown() {
        let cd = parse_cooldown(COOLDOWN).unwrap().expect("cooldown present");
        assert_eq!(cd.expires_at_raw, "2026-08-17 23:54:16 GMT");
        // 2026-08-17T23:54:16Z
        assert_eq!(cd.expires_at_unix, 1_787_010_856);
        // real active cooldown had a blank level cell
        assert_eq!(cd.level, None);
        assert!(!cd.acknowledged);
    }

    #[test]
    fn clean_account_has_no_cooldown() {
        assert!(parse_cooldown(NO_COOLDOWN).unwrap().is_none());
    }

    #[test]
    fn does_not_match_a_table_by_position() {
        // cooldown table genuinely in second position: a non-cooldown table
        // first, then the cooldown table, in one well-formed fixture.
        let first_table_end = NO_COOLDOWN
            .find("</table>")
            .expect("no-cooldown fixture has a table")
            + "</table>".len();
        let cooldown_start = COOLDOWN
            .find("<table")
            .expect("cooldown fixture has a table");
        let cooldown_end = COOLDOWN
            .find("</table>")
            .expect("cooldown fixture has a table")
            + "</table>".len();

        let swapped = format!(
            "{}{}</div>\n",
            &NO_COOLDOWN[..first_table_end],
            &COOLDOWN[cooldown_start..cooldown_end]
        );

        assert!(parse_cooldown(&swapped).unwrap().is_some());
    }

    #[test]
    fn reads_a_numeric_level_and_acknowledged_yes() {
        let html = COOLDOWN
            .replace("<td>&nbsp;</td>", "<td>2</td>")
            .replace("<td>No</td>", "<td>Yes</td>");
        let cd = parse_cooldown(&html).unwrap().expect("cooldown present");
        assert_eq!(cd.level, Some(2));
        assert!(cd.acknowledged);
    }

    #[test]
    fn rejects_a_malformed_timestamp() {
        let html = COOLDOWN.replace("2026-08-17 23:54:16 GMT", "not a date");
        assert!(parse_cooldown(&html).is_err());
    }

    #[test]
    fn empty_input_is_no_cooldown_not_an_error() {
        assert!(parse_cooldown("").unwrap().is_none());
    }
}
