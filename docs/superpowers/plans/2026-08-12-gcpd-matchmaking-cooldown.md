# GCPD Matchmaking Cooldown Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Read an account's active CS2 competitive cooldown from its GCPD page, as a fallback for when the Game Coordinator route is unavailable.

**Architecture:** Two pieces. A pure function that extracts the cooldown from GCPD HTML, fully testable offline against fixtures captured from real pages. Then a thin async wrapper on `WebSession` that fetches the page and calls it. No new dependencies: the extraction is a bounded scan for one specific table, not general HTML parsing.

**Tech Stack:** Rust 1.86, the `web::WebSession` added on this branch.

## Background: everything here was captured from live pages, do not re-derive it

A browser session against an account with an active cooldown, plus live fetches through `WebSession::get`, established all of the following. Treat it as fact.

**The URL must use the `/profiles/<steamid64>/` form.** Measured on a real account:

| URL | `<table>` count | GCPD content |
|---|---|---|
| `https://steamcommunity.com/me/gcpd/730/?tab=matchmaking` | 0 | no |
| `https://steamcommunity.com/profiles/<steamid64>/gcpd/730?tab=matchmaking` | 4 | yes |

`/me/` is a browser-side alias our HTTP client cannot resolve; it silently returns the Steam Community shell with no GCPD content and HTTP 200. A `sessionid` cookie made no difference to either form.

**The page is server-rendered.** The tables arrive in the document response. The only XHR the page fires is `login.steampowered.com/jwt/ajaxrefresh`, which is unrelated token upkeep. Scraping the response body is correct; no browser or JS execution is needed.

**The cooldown table is absent when there is no cooldown.** Confirmed by comparing two accounts. A clean account's four tables are: mode stats, per-map stats, Danger Zone, and last-match-per-mode. An account with a cooldown has the cooldown table plus mode stats, per-map, and last-match. So presence of the table IS the signal.

**Every table shares `class="generic_kv_table"`** with no id, no wrapper element, and no heading. They are separated by bare `<br><br><br>`. The only way to identify the cooldown table is matching its header row. Do not identify it by position.

**Exact markup of the cooldown table**, captured verbatim from a live page (note the tab characters and the `&nbsp;` empty cell):

```html
<table class="generic_kv_table"><tbody><tr>
					<th>Competitive Cooldown Expiration</th>
					<th>Competitive Cooldown Level</th>
					<th>Acknowledged</th>
			</tr>
				<tr>
	<td>2026-08-17 23:54:16 GMT</td><td>&nbsp;</td><td>No</td>		</tr>
		</tbody></table>
```

Observed field behaviour:
- Expiration is `YYYY-MM-DD HH:MM:SS GMT`, always UTC.
- Level was **blank** (`&nbsp;`) on a real active cooldown, so it must be optional. Do not assume it is always a number.
- Acknowledged was `No`. Assume `Yes` is the other value; treat anything else as `false` rather than erroring.

**Headers of the other tables** (so tests can prove the matcher does not pick the wrong one): `Matchmaking Mode / Wins / Ties / Losses / Skill Group / Last Match / Region`, the same plus `Map`, `Matchmaking Mode / Solo Wins / Squad Wins / Matches Played / Last Match`, and `Matchmaking Mode / Last Match`.

## Global Constraints

- Rust edition 2021, rust-version 1.86.
- `#![forbid(unsafe_code)]`. `clippy::all` and `clippy::pedantic` are `warn`, `missing_docs` is `warn`. CI runs clippy `-D warnings` and rustdoc `-D warnings`.
- **Add no new dependencies.** No HTML parser, no date crate. The extraction is a bounded scan for one table; a general parser would not make it less fragile to Valve changing the markup, which is the only real risk.
- **`reqwest` cannot be named in `#[cfg(test)]` code or doctests** (regular dependency, not re-exported). Fine in non-test implementation code.
- Comments CAVEMAN-MINIMAL: terse lowercase fragments, no prose, no articles. Rustdoc is the exception, proper prose matching the crate's voice.
- **No em-dashes anywhere**, including commit messages.
- TDD: write the failing test, run it and SEE it fail, then implement, then see it pass.
- Run `cargo fmt` before committing.

---

### Task 1: Extract the cooldown from GCPD HTML

Pure, synchronous, no network. This is where all the real logic lives, so it gets all the tests.

**Files:**
- Create: `src/gcpd.rs`
- Create: `tests/fixtures/gcpd_cooldown.html`
- Create: `tests/fixtures/gcpd_no_cooldown.html`
- Modify: `src/lib.rs` (add `pub mod gcpd;` plus a module-list bullet, matching the style of the neighbouring bullets and using a colon, not an em-dash)

**Interfaces:**
- Consumes: nothing from other tasks.
- Produces:
  ```rust
  pub struct Cs2Cooldown {
      pub expires_at_unix: i64,
      pub expires_at_raw: String,
      pub level: Option<u32>,
      pub acknowledged: bool,
  }
  pub fn parse_cooldown(html: &str) -> Result<Option<Cs2Cooldown>>;
  ```
  `Ok(None)` means no cooldown table was present, which is the normal clean-account case and is NOT an error.

- [ ] **Step 1: Create the fixtures**

`tests/fixtures/gcpd_cooldown.html` gets the verbatim cooldown table from the Background section above, followed by one decoy table so the tests prove the matcher selects by header rather than by position:

```html
<div class="generic_kv_table_container">
					<table class="generic_kv_table"><tbody><tr>
					<th>Competitive Cooldown Expiration</th>
					<th>Competitive Cooldown Level</th>
					<th>Acknowledged</th>
			</tr>
				<tr>
	<td>2026-08-17 23:54:16 GMT</td><td>&nbsp;</td><td>No</td>		</tr>
		</tbody></table>
	<br><br><br>		<table class="generic_kv_table"><tbody><tr>
					<th>Matchmaking Mode</th>
					<th>Wins</th>
					<th>Ties</th>
					<th>Losses</th>
					<th>Skill Group</th>
					<th>Last Match</th>
					<th>Region</th>
			</tr>
				<tr>
	<td>Premier</td><td>8</td><td>0</td><td>1</td><td>&nbsp;</td><td>2026-08-10 00:31:15 GMT</td><td>3</td>		</tr>
		</tbody></table>
</div>
```

`tests/fixtures/gcpd_no_cooldown.html` is the clean-account shape: the same two non-cooldown tables, no cooldown table.

```html
<div class="generic_kv_table_container">
					<table class="generic_kv_table"><tbody><tr>
					<th>Matchmaking Mode</th>
					<th>Wins</th>
					<th>Ties</th>
					<th>Losses</th>
					<th>Skill Group</th>
					<th>Last Match</th>
					<th>Region</th>
			</tr>
				<tr>
	<td>Premier</td><td>8</td><td>0</td><td>1</td><td>&nbsp;</td><td>2026-08-10 00:31:15 GMT</td><td>3</td>		</tr>
		</tbody></table>
	<br><br><br>		<table class="generic_kv_table"><tbody><tr>
					<th>Matchmaking Mode</th>
					<th>Last Match</th>
			</tr>
				<tr>
	<td>Deathmatch</td><td>2026-08-09 16:17:43 GMT</td>		</tr>
		</tbody></table>
</div>
```

- [ ] **Step 2: Write the failing tests**

In `src/gcpd.rs`'s `#[cfg(test)] mod tests`. `include_str!` reaches the fixtures from `src/` with a relative path.

```rust
const COOLDOWN: &str = include_str!("../tests/fixtures/gcpd_cooldown.html");
const NO_COOLDOWN: &str = include_str!("../tests/fixtures/gcpd_no_cooldown.html");

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
    // cooldown headers on the SECOND table must still be found
    let swapped = format!(
        "{}{}",
        NO_COOLDOWN.trim_end_matches("</div>\n"),
        COOLDOWN[COOLDOWN.find("<table").unwrap()..].to_string()
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
```

Verify the `expires_at_unix` constant yourself before trusting it: `2026-08-17T23:54:16Z`. Compute it independently (a throwaway `python -c "import calendar,time; print(calendar.timegm(time.strptime('2026-08-17 23:54:16','%Y-%m-%d %H:%M:%S')))"` is fine) and if it disagrees with `1787010856`, trust your computation and fix the test.

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --lib gcpd::`

Expected: FAIL, module `gcpd` does not exist.

- [ ] **Step 4: Implement**

Write `src/gcpd.rs`. Required behaviour, however you structure it:

- Scan for each `<table class="generic_kv_table">` ... `</table>` span.
- For each, collect the `<th>` texts. Select the table whose headers are exactly `Competitive Cooldown Expiration`, `Competitive Cooldown Level`, `Acknowledged`. Compare on trimmed text.
- If no table matches, return `Ok(None)`.
- From the matched table's first data row, read the three `<td>` values in order.
- Trim each, and treat `&nbsp;` (and empty) as absent.
- Parse the expiration as `YYYY-MM-DD HH:MM:SS GMT` into a Unix timestamp. A value that does not parse is `Err(Error::Codec(...))` with the offending text included, since a silently-wrong expiry is worse than a loud failure.
- `level` parses as `u32` when present, `None` when blank.
- `acknowledged` is `true` only for `Yes`, case-insensitive; anything else is `false`.

For the timestamp, convert civil date to days since epoch with the standard algorithm rather than inventing one:

```rust
// days from civil, Howard Hinnant's algorithm. valid for any gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}
```

Add a unit test pinning `days_from_civil` against two known dates (`1970-01-01` is 0, `2000-03-01` is 11017) so a regression in the date maths is caught separately from the HTML parsing.

Rustdoc on `parse_cooldown` must state that `Ok(None)` means no cooldown rather than a parse failure, and that the input is expected to be the `?tab=matchmaking` GCPD page.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib gcpd::`

Expected: all PASS.

- [ ] **Step 6: Verify and commit**

Run: `cargo test --all-features`, `cargo clippy --all-targets --all-features`, `cargo test --doc`, `cargo fmt --check`, `RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features`.

```bash
cargo fmt
git add src/gcpd.rs src/lib.rs tests/fixtures/
git commit -m "feat(gcpd): parse the CS2 competitive cooldown from GCPD html

Extraction only, no network. The cooldown table is absent entirely when
there is no cooldown, so Ok(None) is the normal clean-account answer rather
than a parse failure.

Every table on the page shares class generic_kv_table with no id, no
wrapper and no heading, so the table is selected by matching its header row
and never by position. Fixtures are captured from real pages, including an
active cooldown whose level cell was blank, which is why level is optional."
```

---

### Task 2: Fetch it

**Files:**
- Modify: `src/gcpd.rs`
- Modify: `tests/live_auth.rs`

**Interfaces:**
- Consumes: `parse_cooldown` from Task 1; `WebSession::get` and `WebSession::steam_id` from the branch.
- Produces: `pub async fn request_cs2_cooldown(web: &WebSession) -> Result<Option<Cs2Cooldown>>`.

- [ ] **Step 1: Implement the fetch**

TDD does not apply cleanly here: the function is three lines of composition over two already-tested pieces, and its only real risk is the URL, which no offline test can validate. The live test in Step 3 is its test.

```rust
/// Read this account's active CS2 competitive cooldown from its GCPD page.
///
/// `Ok(None)` means no cooldown is active: Steam omits the table entirely
/// rather than rendering an empty one.
///
/// Uses the `/profiles/<steamid64>/` URL form deliberately. The `/me/` alias
/// is resolved browser-side and returns the Steam Community shell with no
/// GCPD content, under HTTP 200, so it fails silently.
///
/// # Errors
///
/// Any transport error from [`WebSession::get`], or [`Error::Codec`] if the
/// cooldown table is present but its expiry does not parse.
pub async fn request_cs2_cooldown(web: &WebSession) -> Result<Option<Cs2Cooldown>> {
    let url = format!(
        "https://steamcommunity.com/profiles/{}/gcpd/730?tab=matchmaking",
        web.steam_id()
    );
    let html = web.get(&url).await?;
    parse_cooldown(&html)
}
```

- [ ] **Step 2: Verify it builds**

Run: `cargo build --all-targets && cargo clippy --all-targets --all-features`

- [ ] **Step 3: Add the live test**

In `tests/live_auth.rs`, following that file's conventions exactly: `#[ignore]`d, reusing `load_account("PLAIN")`, `sign_in_for_session`, `env_opt` and `skip`, with `eprintln!` progress lines. Model it on `web_session_authenticates_a_community_request`, which is directly above it and does the same spawn-session-then-logoff dance.

The account under test may or may not have a cooldown, so the assertion must be about **reaching and parsing the page**, not about a cooldown existing. Assert the call returns `Ok`, and print which case it hit:

```rust
match steamroids::gcpd::request_cs2_cooldown(&web).await {
    Ok(Some(cd)) => eprintln!(
        "OK gcpd: cooldown until {} (level {:?}, acknowledged {})",
        cd.expires_at_raw, cd.level, cd.acknowledged
    ),
    Ok(None) => eprintln!("OK gcpd: no active cooldown"),
    Err(e) => panic!("gcpd cooldown fetch failed: {e}"),
}
```

That is a real assertion: a wrong URL returns the shell, which parses to `Ok(None)`... which would pass. So **also** assert the fetched page is really the GCPD page before parsing, by checking the raw HTML contains `Competitive Matches` (a tab label present on every GCPD page regardless of cooldown state). Fetch once, assert the marker, then parse, rather than calling `request_cs2_cooldown` blind.

- [ ] **Step 4: Verify it compiles and the suite is green**

Run: `cargo test --test live_auth --no-run`, then `cargo test --all-features`, `cargo clippy --all-targets --all-features`, `cargo fmt --check`.

Do not run the live test; it needs credentials. Confirm it is `#[ignore]`d.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/gcpd.rs tests/live_auth.rs
git commit -m "feat(gcpd): fetch the CS2 cooldown through the web session

Uses the /profiles/<steamid64>/ URL form: the /me/ alias resolves
browser-side and silently returns the community shell with HTTP 200 and no
GCPD content, which would parse as no cooldown.

The live test asserts the page is really GCPD before parsing, because a
wrong URL parses to Ok(None) and would otherwise pass."
```

## Out of scope

Prime status (its own `?tab=primeaccount` tab), match history, and the GC-side `penalty_seconds` / `penalty_reason` fields, which are already arriving in the message `cs2::request_player_profile` decodes and discards. That GC route is the primary source and this is the fallback; wiring them together is a later decision.
