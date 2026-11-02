//! Durable historical usage ledger.
//!
//! The ledger is a rebuildable projection of session journals. Each model
//! request has a local request id; repeated records for the same request and
//! generation merge, while distinct request ids remain distinct billable
//! attempts. Costs use exact scaled integer arithmetic and missing values stay
//! unknown.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use t8ful_core::journal::UsageRecord;

use crate::session::SessionId;
use crate::state::{StatePaths, ensure_private_dir, ensure_private_file, open_private_append};

const SCALE: i128 = 1_000_000_000_000_000_000;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Decimal {
    units: i128,
}

impl Decimal {
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        let (negative, value) = match value.strip_prefix('-') {
            Some(value) => (true, value),
            None => (false, value),
        };
        let (value, exponent) = match value.find(['e', 'E']) {
            Some(position) => {
                let (value, exponent) = value.split_at(position);
                (value, exponent[1..].parse::<i32>().ok()?)
            }
            None => (value, 0),
        };
        let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
        let whole = whole.trim();
        let fraction = fraction.trim();

        if whole.is_empty() && fraction.is_empty() {
            return None;
        }

        let mut digits = String::with_capacity(whole.len() + fraction.len());

        digits.push_str(whole);
        digits.push_str(fraction);

        if digits.is_empty() || !digits.chars().all(|character| character.is_ascii_digit()) {
            return None;
        }

        let mut units = digits.parse::<i128>().ok()?;
        let fraction_len = fraction.len() as i32;
        let shift = 18 + exponent - fraction_len;

        if shift > 0 {
            units = units.checked_mul(10_i128.checked_pow(shift as u32)?)?;
        } else if shift < 0 {
            let divisor = 10_i128.checked_pow((-shift) as u32)?;

            units /= divisor;
        }

        Some(Self {
            units: if negative { -units } else { units },
        })
    }

    pub fn add(self, other: Self) -> Self {
        Self {
            units: self.units.saturating_add(other.units),
        }
    }

    pub fn zero() -> Self {
        Self { units: 0 }
    }

    pub fn is_zero(self) -> bool {
        self.units == 0
    }

    pub fn render(self) -> String {
        let negative = self.units < 0;
        let units = self.units.unsigned_abs();
        let whole = units / SCALE as u128;
        let fraction = units % SCALE as u128;
        let mut rendered = format!("{whole}.{fraction:018}");
        let trimmed = rendered.trim_end_matches('0').to_owned();

        rendered = if trimmed.ends_with('.') {
            trimmed.trim_end_matches('.').to_owned()
        } else {
            trimmed
        };

        if negative {
            rendered.insert(0, '-');
        }

        rendered
    }
}

impl std::fmt::Display for Decimal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.render())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerEntry {
    #[serde(default = "ledger_version")]
    pub v: u64,
    pub session_id: String,
    pub request_id: String,
    #[serde(default)]
    pub generation_id: Option<String>,
    pub model: String,
    #[serde(default = "root_agent")]
    pub agent: String,
    #[serde(default = "turn_kind")]
    pub kind: String,
    #[serde(default)]
    pub input_tokens: Option<u64>,
    #[serde(default)]
    pub output_tokens: Option<u64>,
    #[serde(default)]
    pub cost_usd: Option<String>,
    #[serde(default)]
    pub recorded_at: u64,
}

fn ledger_version() -> u64 {
    1
}

fn root_agent() -> String {
    "root".to_owned()
}

fn turn_kind() -> String {
    "turn".to_owned()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageTotals {
    pub requests: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cost_usd: Decimal,
    pub unknown_input: u64,
    pub unknown_output: u64,
    pub unknown_cost: u64,
    pub merged_records: u64,
    pub duplicate_generations: u64,
}

impl Default for UsageTotals {
    fn default() -> Self {
        Self {
            requests: 0,
            input_tokens: None,
            output_tokens: None,
            cost_usd: Decimal::zero(),
            unknown_input: 0,
            unknown_output: 0,
            unknown_cost: 0,
            merged_records: 0,
            duplicate_generations: 0,
        }
    }
}

impl UsageTotals {
    pub fn input_label(&self) -> String {
        match self.input_tokens {
            Some(value) if self.unknown_input == 0 => value.to_string(),
            Some(value) => format!("{value}+"),
            None => "unknown".to_owned(),
        }
    }

    pub fn output_label(&self) -> String {
        match self.output_tokens {
            Some(value) if self.unknown_output == 0 => value.to_string(),
            Some(value) => format!("{value}+"),
            None => "unknown".to_owned(),
        }
    }

    pub fn cost_label(&self) -> String {
        if self.unknown_cost > 0 {
            if self.cost_usd.is_zero() && self.requests > 0 && self.unknown_cost == self.requests {
                "unknown".to_owned()
            } else {
                format!("{} (partial)", self.cost_usd.render())
            }
        } else {
            self.cost_usd.render()
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct UsageSummary {
    pub totals: UsageTotals,
    pub by_model: BTreeMap<String, UsageTotals>,
    pub by_agent: BTreeMap<String, UsageTotals>,
    pub by_session: BTreeMap<String, UsageTotals>,
}

#[derive(Debug)]
pub struct UsageLedger {
    path: PathBuf,
    lock: PathBuf,
}

impl UsageLedger {
    /// Point the ledger at the state directory.
    ///
    /// Nothing is created here: like the session journal and the diagnostic log,
    /// the usage directory appears with the first record it holds, so launching
    /// and quitting leaves no empty directory behind. Both writers create it
    /// under the lock they already take.
    pub fn open(paths: &StatePaths) -> Self {
        Self {
            path: paths.usage_ledger(),
            lock: paths.usage_lock(),
        }
    }

    pub async fn append(
        &self,
        session: &SessionId,
        usage: &UsageRecord,
    ) -> Result<(), std::io::Error> {
        let entry = entry_from_record(session, usage);
        let lock_path = self.lock.clone();
        let path = self.path.clone();

        tokio::task::spawn_blocking(move || append_entry(&lock_path, &path, &entry))
            .await
            .map_err(std::io::Error::other)??;

        Ok(())
    }

    pub fn entries(&self) -> Result<Vec<LedgerEntry>, String> {
        read_entries(&self.path)
    }

    /// Replace the projection atomically under the same writer lock.
    pub fn rebuild_from(&self, entries: &[LedgerEntry]) -> Result<(), String> {
        let lock_path = self.lock.clone();
        let path = self.path.clone();
        let entries = entries.to_vec();

        std::thread::spawn(move || replace_entries(&lock_path, &path, &entries))
            .join()
            .map_err(|_| "the usage ledger rebuild task panicked".to_owned())?
    }
}

fn append_entry(lock_path: &Path, path: &Path, entry: &LedgerEntry) -> std::io::Result<()> {
    ensure_private_dir(lock_path.parent().unwrap_or_else(|| Path::new(".")))?;

    let lock_file = open_private_append(lock_path)?;
    let _lock = nix::fcntl::Flock::lock(lock_file, nix::fcntl::FlockArg::LockExclusive)
        .map_err(|(_, error)| std::io::Error::other(format!("usage ledger lock: {error}")))?;
    let mut file = open_private_append(path)?;
    let line = serde_json::to_vec(entry)
        .map_err(|error| std::io::Error::other(format!("usage entry: {error}")))?;

    file.write_all(&line)?;
    file.write_all(b"\n")?;
    file.sync_data()?;
    ensure_private_file(path)?;

    Ok(())
}

fn replace_entries(lock_path: &Path, path: &Path, entries: &[LedgerEntry]) -> Result<(), String> {
    ensure_private_dir(lock_path.parent().unwrap_or_else(|| Path::new(".")))
        .map_err(|error| format!("cannot create the usage directory: {error}"))?;

    let lock_file = open_private_append(lock_path)
        .map_err(|error| format!("cannot open the usage lock: {error}"))?;
    let _lock = nix::fcntl::Flock::lock(lock_file, nix::fcntl::FlockArg::LockExclusive)
        .map_err(|(_, error)| format!("usage ledger lock: {error}"))?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = std::fs::File::create(&temporary)
        .map_err(|error| format!("cannot create a temporary usage ledger: {error}"))?;

    for entry in entries {
        let line = serde_json::to_vec(entry)
            .map_err(|error| format!("cannot serialize a usage entry: {error}"))?;

        use std::io::Write;
        file.write_all(&line)
            .and_then(|()| file.write_all(b"\n"))
            .map_err(|error| format!("cannot write the usage ledger: {error}"))?;
    }

    file.sync_data()
        .map_err(|error| format!("cannot flush the usage ledger: {error}"))?;
    drop(file);
    ensure_private_file(&temporary)
        .map_err(|error| format!("cannot secure the usage ledger: {error}"))?;
    std::fs::rename(&temporary, path)
        .map_err(|error| format!("cannot replace the usage ledger: {error}"))?;
    ensure_private_file(path)
        .map_err(|error| format!("cannot secure the usage ledger: {error}"))?;

    Ok(())
}

fn read_entries(path: &Path) -> Result<Vec<LedgerEntry>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
    };
    let mut entries = Vec::new();

    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }

        let entry = serde_json::from_str::<LedgerEntry>(line).map_err(|error| {
            format!(
                "usage ledger {} is malformed at line {}: {error}",
                path.display(),
                index + 1
            )
        })?;

        entries.push(entry);
    }

    Ok(entries)
}

/// Merge records for the same attempt and generation, then summarize.
pub fn summarize(entries: &[LedgerEntry]) -> UsageSummary {
    let mut merged: BTreeMap<(String, String), LedgerEntry> = BTreeMap::new();
    let mut merged_records = 0_u64;

    for entry in entries {
        let key = (entry.session_id.clone(), entry.request_id.clone());

        match merged.get_mut(&key) {
            Some(existing) => {
                let mut changed = false;

                if existing.input_tokens.is_none() {
                    existing.input_tokens = entry.input_tokens;
                    changed = true;
                }

                if existing.output_tokens.is_none() {
                    existing.output_tokens = entry.output_tokens;
                    changed = true;
                }

                if existing.cost_usd.is_none() {
                    existing.cost_usd.clone_from(&entry.cost_usd);
                    changed = true;
                }

                if existing.generation_id.is_none() {
                    existing.generation_id.clone_from(&entry.generation_id);
                    changed = true;
                }

                if changed {
                    merged_records += 1;
                }
            }
            None => {
                merged.insert(key, entry.clone());
            }
        }
    }

    let mut summary = UsageSummary::default();
    let mut generations: BTreeMap<String, u64> = BTreeMap::new();

    for entry in merged.values() {
        let totals = totals_for(entry);

        add_totals(&mut summary.totals, &totals);
        add_totals(
            summary.by_model.entry(entry.model.clone()).or_default(),
            &totals,
        );
        add_totals(
            summary.by_agent.entry(entry.agent.clone()).or_default(),
            &totals,
        );
        add_totals(
            summary
                .by_session
                .entry(entry.session_id.clone())
                .or_default(),
            &totals,
        );

        if let Some(generation) = &entry.generation_id {
            *generations.entry(generation.clone()).or_default() += 1;
        }
    }

    summary.totals.merged_records = merged_records;
    summary.totals.duplicate_generations =
        generations.values().filter(|count| **count > 1).count() as u64;

    summary
}

fn totals_for(entry: &LedgerEntry) -> UsageTotals {
    let mut totals = UsageTotals {
        requests: 1,
        input_tokens: entry.input_tokens,
        output_tokens: entry.output_tokens,
        ..UsageTotals::default()
    };

    if entry.input_tokens.is_none() {
        totals.unknown_input = 1;
    }

    if entry.output_tokens.is_none() {
        totals.unknown_output = 1;
    }

    if let Some(cost) = &entry.cost_usd {
        match Decimal::parse(cost) {
            Some(cost) => totals.cost_usd = cost,
            None => totals.unknown_cost = 1,
        }
    } else {
        totals.unknown_cost = 1;
    }

    totals
}

fn add_totals(target: &mut UsageTotals, value: &UsageTotals) {
    target.requests += value.requests;
    target.input_tokens = add_optional(target.input_tokens, value.input_tokens);
    target.output_tokens = add_optional(target.output_tokens, value.output_tokens);
    target.cost_usd = target.cost_usd.add(value.cost_usd);
    target.unknown_input += value.unknown_input;
    target.unknown_output += value.unknown_output;
    target.unknown_cost += value.unknown_cost;
}

fn add_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (None, None) => None,
        (left, right) => Some(left.unwrap_or(0).saturating_add(right.unwrap_or(0))),
    }
}

pub fn rebuild_entries(paths: &StatePaths) -> Result<Vec<LedgerEntry>, String> {
    let reader = match std::fs::read_dir(&paths.sessions) {
        Ok(reader) => reader,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("cannot list sessions: {error}")),
    };
    let mut ids = reader
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
        .filter(|name| SessionId::parse(name).is_ok())
        .collect::<Vec<_>>();

    ids.sort();

    let mut entries = Vec::new();

    for id in ids {
        let session = SessionId::parse(&id).map_err(|error| error.to_string())?;

        match crate::session::load_read_only(paths, &session) {
            Ok(loaded) => {
                entries.extend(
                    loaded
                        .usage
                        .iter()
                        .map(|usage| entry_from_record(&session, usage)),
                );

                let child_usage =
                    crate::child_journal::load_usage(&paths.session_dir(session.as_str()));

                entries.extend(
                    child_usage
                        .iter()
                        .map(|usage| entry_from_record(&session, usage)),
                );
            }
            Err(error) => {
                tracing::warn!(target: "t8ful::usage", "skipping session {id}: {error}");
            }
        }
    }

    Ok(entries)
}

fn entry_from_record(session: &SessionId, usage: &UsageRecord) -> LedgerEntry {
    LedgerEntry {
        v: 1,
        session_id: session.as_str().to_owned(),
        request_id: usage.request_id.clone(),
        generation_id: usage
            .usage
            .generation_id
            .as_ref()
            .map(|id| id.as_str().to_owned()),
        model: usage.model.clone(),
        agent: usage.agent.clone(),
        kind: usage.kind.clone(),
        input_tokens: usage.usage.input_tokens,
        output_tokens: usage.usage.output_tokens,
        cost_usd: usage.usage.cost_usd.clone(),
        recorded_at: now_seconds(),
    }
}

fn now_seconds() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{Decimal, LedgerEntry, summarize};

    fn entry(
        request: &str,
        generation: Option<&str>,
        input: Option<u64>,
        cost: Option<&str>,
    ) -> LedgerEntry {
        LedgerEntry {
            v: 1,
            session_id: "session".to_owned(),
            request_id: request.to_owned(),
            generation_id: generation.map(str::to_owned),
            model: "model".to_owned(),
            agent: "root".to_owned(),
            kind: "turn".to_owned(),
            input_tokens: input,
            output_tokens: Some(2),
            cost_usd: cost.map(str::to_owned),
            recorded_at: 0,
        }
    }

    #[test]
    fn decimal_arithmetic_is_exact() {
        let one = Decimal::parse("0.000000000000000001").unwrap();
        let two = Decimal::parse("0.000000000000000002").unwrap();

        assert_eq!(one.add(two).render(), "0.000000000000000003");
        assert_eq!(Decimal::parse("1.5").unwrap().render(), "1.5");
        assert_eq!(Decimal::parse("1e-3").unwrap().render(), "0.001");
        assert!(Decimal::parse("not-money").is_none());
    }

    #[test]
    fn unknown_values_are_not_zero() {
        let summary = summarize(&[entry("a", None, None, None)]);

        assert_eq!(summary.totals.unknown_input, 1);
        assert_eq!(summary.totals.unknown_cost, 1);
        assert_eq!(summary.totals.input_label(), "unknown");
        assert_eq!(summary.totals.cost_label(), "unknown");
    }

    #[test]
    fn repeated_records_for_one_attempt_merge_without_losing_attempts() {
        let summary = summarize(&[
            entry("a", Some("gen-1"), Some(10), None),
            entry("a", Some("gen-1"), None, Some("0.25")),
            entry("b", Some("gen-1"), Some(3), Some("0.5")),
        ]);

        assert_eq!(summary.totals.requests, 2);
        assert_eq!(summary.totals.input_tokens, Some(13));
        assert_eq!(summary.totals.cost_usd.render(), "0.75");
        assert_eq!(summary.totals.merged_records, 1);
        assert_eq!(summary.totals.duplicate_generations, 1);
    }
}
