//! Ranked forge search — resolving bare package names across forges.
//!
//! When a user runs `sudo gitfull install <name>` with **no forge prefix**
//! and no `owner/repo` path, gitfull:
//!
//! 1. queries the search API of **every configured forge** that has one
//!    (GitHub, GitLab, Gitea/Forgejo kinds; `cgit`/`generic` forges without
//!    an `api_base` are skipped with a visible note);
//! 2. enriches the top candidates with contributor/commit counts where the
//!    forge's API offers them (GitHub, via `Link`-header pagination);
//! 3. ranks all candidates by **stars, contributors, commit count, and
//!    recency of the last push**;
//! 4. auto-selects the top-ranked repository — and **prints the full
//!    ranked table plus the chosen `forge:owner/repo` before building
//!    anything**, so the choice is visible, never silent.
//!
//! The explicit `forge:owner/repo` form bypasses all of this and goes
//! straight to that repository.
//!
//! # Execution posture
//!
//! All HTTP goes through [`crate::gitproc::http_get`] — `curl` under the
//! `ForgeApi` exec class at the single exec chokepoint: read-only GETs,
//! unauthenticated (public endpoints only), no writes, fully audit-logged.
//! Responses are parsed with the hand-written [`crate::json`] parser, so
//! the crate dependency budget stays exactly `serde` + `toml`.
//!
//! # Scoring formula (documented, deterministic)
//!
//! Each signal is normalized to 0..1 and weighted:
//!
//! | signal | normalization | weight |
//! |---|---|---|
//! | stars | log10(1+n) / 6 (≈1M stars = 1.0) | 0.40 |
//! | contributors | log10(1+n) / 4 (≈10k = 1.0) | 0.25 |
//! | commits | log10(1+n) / 6 (≈1M = 1.0) | 0.25 |
//! | recency | 2^(−days_since_last_push / 365) | 0.10 |
//!
//! Forges that cannot supply a signal (GitLab/Gitea have no cheap
//! contributor/commit counts) are scored on the signals they do provide:
//! the weights of the *present* signals are re-normalized, so a GitLab
//! repository is not structurally punished for its forge's terser API.

use crate::config::Config;
use crate::error::{GitfullError, Result};
use crate::forge::Forge;
use crate::gitproc::{self, ExecCtx, HttpResponse};
use crate::json::Json;
use crate::spec::{PkgSpec, Source};
use crate::util;

/// Candidates fetched per forge before ranking.
pub const CANDIDATES_PER_FORGE: usize = 5;
/// GitHub-only detail queries (contributors + commits) are made for at
/// most this many candidates, to respect unauthenticated rate limits.
pub const ENRICH_TOP_N: usize = 3;
const HTTP_TIMEOUT_SECS: u64 = 20;

/// One repository found by a forge search.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub forge: String,
    pub owner: String,
    pub repo: String,
    pub stars: f64,
    pub contributors: Option<f64>,
    pub commits: Option<f64>,
    /// Epoch seconds of the last push/activity, when known.
    pub last_activity: Option<u64>,
    pub score: f64,
}

impl Candidate {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

// ---------------------------------------------------------------------------
// URL building & response parsing (per forge kind)
// ---------------------------------------------------------------------------

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn search_url(forge: &Forge, term: &str, limit: usize) -> Option<String> {
    let api = forge.api_base()?;
    let q = urlencode(term);
    Some(match forge.kind() {
        "github" => format!(
            "{api}/search/repositories?q={q}+in:name&sort=stars&order=desc&per_page={limit}"
        ),
        "gitlab" => format!("{api}/projects?search={q}&order_by=last_activity_at&sort=desc&per_page={limit}"),
        "gitea" | "forgejo" => format!("{api}/repos/search?q={q}&limit={limit}"),
        _ => return None,
    })
}

/// Parse one forge's search response into candidates.
pub fn parse_search_response(forge_name: &str, kind: &str, body: &str) -> Result<Vec<Candidate>> {
    let v = Json::parse(body)?;
    let rows: Vec<&Json> = match kind {
        "github" => v
            .get("items")
            .and_then(|i| i.as_arr())
            .map(|a| a.iter().collect())
            .ok_or_else(|| GitfullError::Unsupported("github search response has no items[]".into()))?,
        "gitlab" => v
            .as_arr()
            .map(|a| a.iter().collect())
            .ok_or_else(|| GitfullError::Unsupported("gitlab projects response is not an array".into()))?,
        "gitea" | "forgejo" => v
            .get("data")
            .and_then(|d| d.as_arr())
            .map(|a| a.iter().collect())
            .ok_or_else(|| GitfullError::Unsupported("gitea repos/search response has no data[]".into()))?,
        _ => return Ok(Vec::new()),
    };

    let mut out = Vec::new();
    for row in rows {
        // full_name (github/gitea) / path_with_namespace (gitlab)
        let full = row
            .str_of("full_name")
            .or_else(|| row.str_of("path_with_namespace"));
        let Some(full) = full else { continue };
        let Some((owner, repo)) = full.rsplit_once('/') else {
            continue;
        };
        if owner.is_empty() || repo.is_empty() {
            continue;
        }
        let stars = row
            .num_of("stargazers_count")
            .or_else(|| row.num_of("star_count"))
            .or_else(|| row.num_of("stars_count"))
            .unwrap_or(0.0);
        let last = row
            .str_of("pushed_at")
            .or_else(|| row.str_of("last_activity_at"))
            .or_else(|| row.str_of("updated_at"))
            .and_then(parse_rfc3339);
        out.push(Candidate {
            forge: forge_name.to_string(),
            owner: owner.to_string(),
            repo: repo.to_string(),
            stars,
            contributors: None,
            commits: None,
            last_activity: last,
            score: 0.0,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Link-header pagination (GitHub counts without extra JSON)
// ---------------------------------------------------------------------------

/// Extract `rel="last"` page number from a `Link` header value.
pub fn link_last_page(link: &str) -> Option<u64> {
    for part in link.split(',') {
        if !part.contains("last") {
            continue;
        }
        // only the rel="last" segment
        let rel_ok = part.contains("rel=\"last\"") || part.contains("rel=last");
        if !rel_ok {
            continue;
        }
        // URL is between '<' and '>'
        let start = part.find('<')? + 1;
        let end = part[start..].find('>')? + start;
        let url = &part[start..end];
        // page=N in the query string
        for kv in url.split('?').nth(1)?.split('&') {
            if let Some(v) = kv.strip_prefix("page=") {
                if let Ok(n) = v.parse::<u64>() {
                    return Some(n);
                }
            }
        }
    }
    None
}

fn count_from_response(resp: &HttpResponse) -> Option<f64> {
    // Paginated endpoints expose the total via Link rel="last";
    // without a Link header the single page IS the whole list.
    if let Some(n) = resp.header("link").and_then(link_last_page) {
        return Some(n as f64);
    }
    let v = Json::parse(&resp.body).ok()?;
    v.as_arr().map(|a| a.len() as f64)
}

// ---------------------------------------------------------------------------
// RFC 3339 (subset) date parsing — no chrono, no crates
// ---------------------------------------------------------------------------

/// Parse `YYYY-MM-DDTHH:MM:SSZ` (what the forges return) to epoch seconds.
pub fn parse_rfc3339(s: &str) -> Option<u64> {
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }
    let b = s.as_bytes();
    if b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') || b[13] != b':' || b[16] != b':'
    {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> { s[from..to].parse().ok() };
    let y = num(0, 4)?;
    let mo = num(5, 7)?;
    let d = num(8, 10)?;
    let h = num(11, 13)?;
    let mi = num(14, 16)?;
    let se = num(17, 19)?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    // Howard Hinnant's days_from_civil
    let (y, m) = if mo <= 2 { (y - 1, mo + 12) } else { (y, mo) };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m - 3) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + h * 3600 + mi * 60 + se;
    if secs >= 0 {
        Some(secs as u64)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

/// Weights: stars, contributors, commits, recency.
pub const W_STARS: f64 = 0.40;
pub const W_CONTRIB: f64 = 0.25;
pub const W_COMMITS: f64 = 0.25;
pub const W_RECENCY: f64 = 0.10;

fn log_norm(v: f64, full_at: f64) -> f64 {
    if v <= 0.0 {
        return 0.0;
    }
    (v.log10() / full_at).min(1.0)
}

/// Days since `epoch_ts` (never negative).
fn days_since(epoch_ts: u64, now: u64) -> f64 {
    let d = now.saturating_sub(epoch_ts) as f64;
    d / 86400.0
}

/// Rank score in 0..1; see module docs for the formula. Weights of absent
/// signals are re-normalized across the present ones.
pub fn score(c: &Candidate, now: u64) -> f64 {
    let stars_s = log_norm(1.0 + c.stars, 6.0);
    let contrib_s = c.contributors.map(|v| log_norm(1.0 + v, 4.0));
    let commits_s = c.commits.map(|v| log_norm(1.0 + v, 6.0));
    let recency_s = c
        .last_activity
        .map(|t| 2.0f64.powf(-days_since(t, now) / 365.0));

    let mut got = 0.0;
    let mut total = 0.0;
    got += W_STARS * stars_s;
    total += W_STARS;
    if let Some(s) = contrib_s {
        got += W_CONTRIB * s;
        total += W_CONTRIB;
    }
    if let Some(s) = commits_s {
        got += W_COMMITS * s;
        total += W_COMMITS;
    }
    if let Some(s) = recency_s {
        got += W_RECENCY * s;
        total += W_RECENCY;
    }
    if total == 0.0 {
        0.0
    } else {
        got / total
    }
}

/// Deterministic ordering: score desc, then stars desc, then full name asc.
pub fn rank(candidates: &mut [Candidate], now: u64) {
    for c in candidates.iter_mut() {
        c.score = score(c, now);
    }
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                b.stars
                    .partial_cmp(&a.stars)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then_with(|| a.full_name().cmp(&b.full_name()))
    });
}

// ---------------------------------------------------------------------------
// Output formatting
// ---------------------------------------------------------------------------

fn format_count(v: f64) -> String {
    if v < 1000.0 {
        format!("{}", v as u64)
    } else if v < 1_000_000.0 {
        format!("{:.1}k", v / 1000.0)
    } else {
        format!("{:.1}M", v / 1_000_000.0)
    }
}

fn format_opt_count(v: Option<f64>) -> String {
    v.map(format_count).unwrap_or_else(|| "-".to_string())
}

fn format_age(epoch_ts: u64, now: u64) -> String {
    let days = (now.saturating_sub(epoch_ts) / 86400) as u64;
    if days == 0 {
        "today".to_string()
    } else if days == 1 {
        "yesterday".to_string()
    } else if days < 31 {
        format!("{days} days ago")
    } else if days < 365 {
        format!("{:.0} months ago", days as f64 / 30.44)
    } else {
        format!("{:.1} years ago", days as f64 / 365.25)
    }
}

/// Print the ranked table + the resolution line. The chosen repository is
/// always shown before anything is cloned or built.
pub fn print_resolution(term: &str, candidates: &[Candidate], scope: &str) {
    println!(
        "gitfull: {n} candidate repository(ies) for `{term}` {scope} — ranked:",
        n = candidates.len()
    );
    println!(
        "  {:<4} {:<38} {:<10} {:>7} {:>9} {:>8}  {}",
        "#", "repository", "forge", "stars", "contribs", "commits", "last push"
    );
    let now = util::epoch();
    for (i, c) in candidates.iter().enumerate() {
        let age = c
            .last_activity
            .map(|t| format_age(t, now))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "  {:<4} {:<38} {:<10} {:>7} {:>9} {:>8}  {}",
            i + 1,
            truncate(&c.full_name(), 38),
            truncate(&c.forge, 10),
            format_count(c.stars),
            format_opt_count(c.contributors),
            format_opt_count(c.commits),
            age
        );
    }
    let top = &candidates[0];
    println!(
        "gitfull: resolved `{term}` -> {}:{} (auto-selected: rank 1 of {})",
        top.forge,
        top.full_name(),
        candidates.len()
    );
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

// ---------------------------------------------------------------------------
// Queries
// ---------------------------------------------------------------------------

fn query_forge(
    cfg: &Config,
    ctx: &ExecCtx,
    forge: &Forge,
    term: &str,
) -> Result<Vec<Candidate>> {
    let url = search_url(forge, term, CANDIDATES_PER_FORGE).ok_or_else(|| {
        GitfullError::Unsupported(format!(
            "forge `{}` (kind `{}`) has no search API",
            forge.name,
            forge.kind()
        ))
    })?;
    let accept = if forge.kind() == "github" {
        Some("application/vnd.github+json")
    } else {
        None
    };
    let resp = gitproc::http_get(ctx, &url, &cfg.host_tool_path, accept, HTTP_TIMEOUT_SECS)?;
    if !resp.ok() {
        return Err(GitfullError::Unsupported(format!(
            "search API returned HTTP {} for {}",
            resp.status, forge.name
        )));
    }
    parse_search_response(&forge.name, forge.kind(), &resp.body)
}

/// GitHub-only detail enrichment: contributors + commit counts via
/// Link-header pagination. Soft-failing: on rate limits or errors the
/// candidate simply keeps `None` and is scored on its other signals
/// (`warned` dedupes the rate-limit notice to once per resolution).
fn enrich_github(cfg: &Config, ctx: &ExecCtx, api: &str, cand: &mut Candidate, warned: &mut bool) {
    let full = cand.full_name();
    let detail = |endpoint: &str, warned: &mut bool| -> Option<f64> {
        let url = format!("{api}/repos/{full}/{endpoint}?per_page=1");
        let resp =
            gitproc::http_get(ctx, &url, &cfg.host_tool_path, None, HTTP_TIMEOUT_SECS).ok()?;
        if !resp.ok() {
            if (resp.status == 403 || resp.status == 429) && !*warned {
                *warned = true;
                eprintln!(
                    "gitfull: warning: github rate-limited detail queries; \
                     ranking continues on stars/recency only"
                );
            }
            return None;
        }
        count_from_response(&resp)
    };
    cand.contributors = detail("contributors", warned);
    cand.commits = detail("commits", warned);
}

// ---------------------------------------------------------------------------
// Entry point: resolve a Search spec to a concrete Forge spec
// ---------------------------------------------------------------------------

/// Resolve `name` / `forge:name` into a concrete `forge:owner/repo` spec,
/// printing the ranked candidates and the chosen resolution first.
///
/// This is the ONLY path that performs forge searches; explicit
/// `forge:owner/repo` specs never come through here.
pub fn resolve(cfg: &Config, ctx: &ExecCtx, spec: &PkgSpec) -> Result<PkgSpec> {
    let (scope, term) = match &spec.source {
        Source::Search { forge, term } => (forge.clone(), term.clone()),
        _ => {
            return Err(GitfullError::Unsupported(
                "internal: search::resolve called with a non-search spec".into(),
            ))
        }
    };

    println!(
        "gitfull: searching {} for `{term}` (matching repositories are ranked by \
         stars, contributors, commits, and recency; the top one is auto-selected)",
        match &scope {
            Some(f) => format!("forge `{f}`"),
            None => "all configured forges".to_string(),
        }
    );

    let forges: Vec<&Forge> = match &scope {
        Some(f) => vec![cfg.forges.get(f)?],
        None => cfg.forges.all().collect(),
    };
    let (mut candidates, searchable, failed) = gather_candidates(cfg, ctx, &forges, &term);

    if candidates.is_empty() {
        let mut err = empty_result_error(&term, searchable, failed);
        if let GitfullError::Unsupported(msg) = &mut err {
            msg.push_str(
                ". Use an explicit form instead: `forge:owner/repo`, `owner/repo`, \
                 a URL, or a local path",
            );
        }
        return Err(err);
    }

    enrich_and_rank(cfg, ctx, &mut candidates);
    print_resolution(
        &term,
        &candidates,
        &match &scope {
            Some(f) => format!("on forge `{f}`"),
            None => "across configured forges".to_string(),
        },
    );

    let top = &candidates[0];
    Ok(PkgSpec {
        source: Source::Forge {
            forge: Some(top.forge.clone()),
            owner: top.owner.clone(),
            repo: top.repo.clone(),
        },
        git_ref: spec.git_ref.clone(),
    })
}

/// Rank forge-search candidates for ONE dependency name (parsed out of
/// a build manifest by [`crate::depgraph`]). Returns the ranked list —
/// **without committing to any of them.**
///
/// This is the *flagged fallback* of dependency-name resolution, used
/// only when the name is in neither the user's `[dep.<name>]` config,
/// the curated upstream map ([`crate::libmap`]), meson wraps, nor the
/// shared library cache. Star/contributor/commit ranking answers "what
/// is a popular repo matching this text" — it has **no reliable
/// correspondence** to "what is the correct upstream source for this
/// pkg-config module" (module names often live inside a parent
/// library's repository; generic names string-match unrelated
/// projects). The caller must therefore treat every candidate as
/// **unconfirmed** and require an interactive confirmation or a config
/// pin before building anything (see `planner::confirm_unconfirmed_…`).
pub fn rank_dep_candidates(cfg: &Config, ctx: &ExecCtx, name: &str) -> Result<Vec<Candidate>> {
    let forges: Vec<&Forge> = cfg.forges.all().collect();
    let (mut candidates, searchable, failed) = gather_candidates(cfg, ctx, &forges, name);

    if candidates.is_empty() {
        let mut msg = empty_result_error(name, searchable, failed).to_string();
        msg.push_str(&format!(
            ". Pin it explicitly with [dep.\"{name}\"] source = \"forge:owner/repo\" \
             in gitfull.conf"
        ));
        return Err(GitfullError::Unsupported(msg));
    }

    enrich_and_rank(cfg, ctx, &mut candidates);
    print_unconfirmed(name, &candidates);
    Ok(candidates)
}

/// Print a dependency-name candidate table with the unconfirmed flag —
/// deliberately nothing like `print_resolution`'s "auto-selected" line,
/// because NONE of these candidates may be auto-built.
pub fn print_unconfirmed(name: &str, candidates: &[Candidate]) {
    println!(
        "gitfull: dep `{name}` is NOT covered by the curated upstream map, \
         nor a [dep] pin — ranked forge search found {} candidate(s), ALL \
         UNCONFIRMED (popularity cannot establish upstream identity for a \
         library module name):",
        candidates.len()
    );
    let now = util::epoch();
    for (i, c) in candidates.iter().enumerate() {
        let age = c
            .last_activity
            .map(|t| format_age(t, now))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "  {:<4} {}:{}  ({} stars, last active {})  <- unconfirmed",
            i + 1,
            c.forge,
            c.full_name(),
            format_count(c.stars),
            age
        );
    }
    if let Some(top) = candidates.first() {
        if top.stars < 10.0 {
            println!(
                "gitfull: WARNING: the top match has near-zero stars — \
                 particularly low signal; treat it as a wrong-repo match \
                 until proven otherwise"
            );
        }
    }
}

/// Query every forge for `term`. Returns (candidates, searchable forge
/// count, failed forge count); per-forge failures are warned, not fatal.
fn gather_candidates(
    cfg: &Config,
    ctx: &ExecCtx,
    forges: &[&Forge],
    term: &str,
) -> (Vec<Candidate>, usize, usize) {
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut searchable = 0;
    let mut failed = 0;
    for forge in forges {
        if !forge.searchable() {
            println!(
                "gitfull: forge `{}` (kind `{}`) has no search API — skipped",
                forge.name,
                forge.kind()
            );
            continue;
        }
        searchable += 1;
        match query_forge(cfg, ctx, forge, term) {
            Ok(mut cs) => {
                if cs.is_empty() {
                    println!("gitfull: forge `{}`: no matches", forge.name);
                }
                candidates.append(&mut cs);
            }
            Err(e) => {
                failed += 1;
                eprintln!("gitfull: warning: forge `{}` search failed: {e}", forge.name);
            }
        }
    }
    (candidates, searchable, failed)
}

fn empty_result_error(term: &str, searchable: usize, failed: usize) -> GitfullError {
    let searched = if searchable == 0 {
        "no configured forge has a search API".to_string()
    } else {
        format!(
            "searched {} searchable forge(s){}",
            searchable,
            if failed > 0 {
                format!(" ({failed} failed — see warnings above)")
            } else {
                String::new()
            }
        )
    };
    GitfullError::Unsupported(format!(
        "no repository matching `{term}` was found ({searched})"
    ))
}

/// GitHub enrichment for the few strongest candidates + final ranking.
fn enrich_and_rank(cfg: &Config, ctx: &ExecCtx, candidates: &mut [Candidate]) {
    let github_api = cfg
        .forges
        .all()
        .find(|f| f.kind() == "github")
        .and_then(|f| f.api_base());
    if let Some(api) = github_api {
        let mut by_stars: Vec<&mut Candidate> = candidates
            .iter_mut()
            .filter(|c| c.forge == "github")
            .collect();
        by_stars.sort_by(|a, b| {
            b.stars
                .partial_cmp(&a.stars)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut rate_warned = false;
        for cand in by_stars.into_iter().take(ENRICH_TOP_N) {
            enrich_github(cfg, ctx, &api, cand, &mut rate_warned);
        }
    }
    rank(candidates, util::epoch());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(
        forge: &str,
        full: &str,
        stars: f64,
        contributors: Option<f64>,
        commits: Option<f64>,
        last: Option<u64>,
    ) -> Candidate {
        let (owner, repo) = full.rsplit_once('/').unwrap();
        Candidate {
            forge: forge.into(),
            owner: owner.into(),
            repo: repo.into(),
            stars,
            contributors,
            commits,
            last_activity: last,
            score: 0.0,
        }
    }

    const NOW: u64 = 1_800_000_000;

    #[test]
    fn ranking_prefers_more_signals_of_strength() {
        // same stars & recency; more contributors + commits must win
        let mut cs = vec![
            cand("github", "a/small", 1000.0, Some(5.0), Some(50.0), Some(NOW)),
            cand("github", "b/big", 1000.0, Some(300.0), Some(9000.0), Some(NOW)),
        ];
        rank(&mut cs, NOW);
        assert_eq!(cs[0].full_name(), "b/big");
        assert!(cs[0].score > cs[1].score);
    }

    #[test]
    fn ranking_stars_dominate() {
        // 40% weight: hugely starred beats modest everything else
        let mut cs = vec![
            cand("github", "a/tiny-pop", 5.0, Some(3.0), Some(10.0), Some(NOW)),
            cand("github", "b/mega", 250_000.0, None, None, None),
        ];
        rank(&mut cs, NOW);
        assert_eq!(cs[0].full_name(), "b/mega");
    }

    #[test]
    fn missing_signals_renormalize() {
        // gitlab candidate with same stars & recency as github one, but no
        // contrib/commit data: score is computed over present signals only
        let gh = cand("github", "a/gh", 100.0, Some(2.0), Some(20.0), Some(NOW));
        let gl = cand("gitlab", "b/gl", 100.0, None, None, Some(NOW));
        let s_gh = score(&gh, NOW);
        let s_gl = score(&gl, NOW);
        // both in 0..1; github's extra *weak* signals barely change the
        // renormalized value, but neither is structurally zero
        assert!(s_gh > 0.0 && s_gh <= 1.0);
        assert!(s_gl > 0.0 && s_gl <= 1.0);
        // and specifically: renormalization means gl is not punished to 0.4x
        assert!(s_gl > W_STARS, "gitlab candidate must keep most of its score");
    }

    #[test]
    fn recency_breaks_ties() {
        let mut cs = vec![
            cand("github", "a/stale", 1000.0, Some(50.0), Some(500.0), Some(NOW - 40 * 365 * 86400)),
            cand("github", "b/fresh", 1000.0, Some(50.0), Some(500.0), Some(NOW)),
        ];
        rank(&mut cs, NOW);
        assert_eq!(cs[0].full_name(), "b/fresh");
    }

    #[test]
    fn deterministic_tiebreak() {
        let mut cs = vec![
            cand("github", "z/z", 100.0, None, None, None),
            cand("github", "a/a", 100.0, None, None, None),
        ];
        rank(&mut cs, NOW);
        assert_eq!(cs[0].full_name(), "a/a");
    }

    #[test]
    fn parse_github_search_response() {
        let body = r#"{"total_count": 2, "items": [
            {"full_name": "octocat/Hello-World", "stargazers_count": 142,
             "pushed_at": "2025-11-30T08:21:00Z"},
            {"full_name": "x/hello", "stargazers_count": 7,
             "pushed_at": "2019-02-01T00:00:00Z"}
        ]}"#;
        let cs = parse_search_response("github", "github", body).unwrap();
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].owner, "octocat");
        assert_eq!(cs[0].repo, "Hello-World");
        assert_eq!(cs[0].stars, 142.0);
        assert!(cs[0].last_activity.is_some());
    }

    #[test]
    fn parse_gitlab_projects_response() {
        let body = r#"[
            {"id": 1, "path_with_namespace": "gnome/libfoo",
             "star_count": 31, "last_activity_at": "2025-10-02T11:00:00Z"}
        ]"#;
        let cs = parse_search_response("gitlab", "gitlab", body).unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].full_name(), "gnome/libfoo");
        assert_eq!(cs[0].stars, 31.0);
    }

    #[test]
    fn parse_gitea_search_response() {
        let body = r#"{"ok": true, "data": [
            {"full_name": "codeberg-org/tool", "stars_count": 12,
             "updated_at": "2026-01-05T09:00:00Z"}
        ]}"#;
        let cs = parse_search_response("codeberg", "forgejo", body).unwrap();
        assert_eq!(cs.len(), 1);
        assert_eq!(cs[0].stars, 12.0);
        assert_eq!(cs[0].forge, "codeberg");
    }

    #[test]
    fn malformed_responses_error() {
        assert!(parse_search_response("github", "github", "not json").is_err());
        assert!(parse_search_response("gitlab", "gitlab", "{\"a\":1}").is_err());
        // rows without a proper full_name are skipped, not fatal
        let cs = parse_search_response(
            "github",
            "github",
            r#"{"items": [{"stargazers_count": 1}, {"full_name": "o/r"}]}"#,
        )
        .unwrap();
        assert_eq!(cs.len(), 1);
    }

    #[test]
    fn link_header_parsing() {
        let h = r#"<https://api.github.com/repos/o/r/contributors?per_page=1&page=2>; rel="next", <https://api.github.com/repos/o/r/contributors?per_page=1&page=89>; rel="last""#;
        assert_eq!(link_last_page(h), Some(89));
        // only next, no last
        let h2 = r#"<https://x?page=2>; rel="next""#;
        assert_eq!(link_last_page(h2), None);
        // unquoted rel
        let h3 = "<https://x?page=7>; rel=last";
        assert_eq!(link_last_page(h3), Some(7));
    }

    #[test]
    fn rfc3339_parsing() {
        // 2025-11-30T08:21:00Z
        assert_eq!(
            parse_rfc3339("2025-11-30T08:21:00Z"),
            Some(1764490860)
        );
        // epoch day 0
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        // leap-year date
        assert_eq!(parse_rfc3339("2024-02-29T12:00:00Z").is_some(), true);
        // junk
        assert_eq!(parse_rfc3339("yesterday"), None);
        assert_eq!(parse_rfc3339("2025-13-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339(""), None);
    }

    #[test]
    fn url_encoding() {
        assert_eq!(urlencode("hello"), "hello");
        assert_eq!(urlencode("a.b-c_d~e"), "a.b-c_d~e");
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("ü"), "%C3%BC");
    }

    #[test]
    fn count_formatting() {
        assert_eq!(format_count(0.0), "0");
        assert_eq!(format_count(142.0), "142");
        assert_eq!(format_count(1400.0), "1.4k");
        assert_eq!(format_count(2_500_000.0), "2.5M");
        assert_eq!(format_opt_count(None), "-");
    }
}
