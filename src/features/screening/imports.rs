//! CSV imports of marketing contacts, checked before any row is written: rules on the file name,
//! the column names and statistics over the addresses, then one Claude call with only those (never
//! an address, a name or a row). An import of people as unsubscribed is never checked.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::json;
use uuid::Uuid;

use super::ai::{self, NoJudgment};
use super::fetch;
use super::rules::{self, ImportStats};
use super::standing::{self, New, Standing};
use super::Verdict;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// The most distinct domains whose mail servers are looked up.
pub const MX_DOMAINS: usize = 200;
const MX_PARALLEL: usize = 10;
const MX_DEADLINE: Duration = Duration::from_secs(10);
const AI_DEADLINE: Duration = Duration::from_secs(45);
/// Files larger than the import accepts are refused by it, unchecked.
const MAX_BYTES: usize = 50 * 1024 * 1024;
const MAX_ROWS: u64 = 200_000;

/// Column names that look like a consent or sign-up record.
fn consent_column(h: &str) -> bool {
    let w = rules::words(h).join(" ");
    ["consent", "opt in", "optin", "opted", "signup", "sign up", "signed up", "subscribed", "subscription", "joined", "source", "date added", "created"].iter().any(|k| w.contains(k))
}

fn email_column(h: &str) -> bool {
    let k: String = h.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect();
    matches!(k.as_str(), "email" | "emailaddress" | "email1" | "mail")
}

/// Headers and statistics, in one pass over the file, without keeping any address.
pub fn stats(csv_text: &str) -> (Vec<String>, ImportStats, Vec<String>) {
    let mut rdr = csv::ReaderBuilder::new().flexible(true).trim(csv::Trim::All).from_reader(csv_text.as_bytes());
    let headers: Vec<String> = match rdr.headers() {
        Ok(h) => h.iter().map(|h| h.trim_start_matches('\u{feff}').chars().take(80).collect()).collect(),
        Err(_) => return (Vec::new(), ImportStats::default(), Vec::new()),
    };
    let col = headers.iter().position(|h| email_column(h));
    let mut s = ImportStats { consent_columns: headers.iter().filter(|h| consent_column(h)).take(10).cloned().collect(), ..Default::default() };
    let mut domains: HashMap<String, u64> = HashMap::new();
    for rec in rdr.records() {
        let Ok(rec) = rec else { continue };
        s.rows += 1;
        if s.rows > MAX_ROWS {
            break;
        }
        let email = col.and_then(|c| rec.get(c)).and_then(crate::models::normalize_email);
        let Some(email) = email else {
            s.no_address += 1;
            continue;
        };
        let Some((_, domain)) = email.rsplit_once('@') else {
            s.no_address += 1;
            continue;
        };
        s.with_address += 1;
        match crate::deliverability::risky_address(&email) {
            Some("role") => s.role += 1,
            Some(_) => s.disposable += 1,
            None => {}
        }
        if rules::TYPO_DOMAINS.contains(&domain) {
            s.typo += 1;
        } else if rules::is_consumer(domain) {
            s.consumer += 1;
        } else {
            s.company += 1;
        }
        *domains.entry(domain.to_string()).or_default() += 1;
    }
    let mut by_count: Vec<(String, u64)> = domains.into_iter().collect();
    by_count.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    s.distinct_domains = by_count.len() as u64;
    s.top_domains = by_count.iter().take(20).cloned().collect();
    // Domains to look up, most common first, as `count\tdomain` (only domains leave this function).
    let picked: Vec<String> = by_count
        .iter()
        .filter(|(d, _)| !rules::is_consumer(d) && !rules::TYPO_DOMAINS.contains(&d.as_str()))
        .take(MX_DOMAINS)
        .map(|(d, n)| format!("{n}\t{d}"))
        .collect();
    s.mx_checked = by_count.iter().filter(|(d, _)| rules::is_consumer(d)).map(|(_, n)| n).sum();
    (headers, s, picked)
}

/// Look up mail servers for the picked domains (`count\tdomain`), within a deadline, and add them
/// to the statistics. Consumer domains already count as having one.
async fn add_mail_servers(state: &AppState, s: &mut ImportStats, picked: &[String]) {
    use futures_util::stream::{self, StreamExt};
    let items: Vec<(u64, String)> = picked.iter().filter_map(|l| l.split_once('\t')).filter_map(|(n, d)| Some((n.parse().ok()?, d.to_string()))).collect();
    let work = stream::iter(items).map(|(n, d)| async move { (n, fetch::has_mail_server(state, &d).await) }).buffer_unordered(MX_PARALLEL).collect::<Vec<_>>();
    let Ok(found) = tokio::time::timeout(MX_DEADLINE, work).await else { return };
    for (n, has) in found {
        if let Some(has) = has {
            s.mx_checked += n;
            if !has {
                s.no_mx += n;
            }
        }
    }
}

/// Check a marketing import before anything is written. Err: refused, with the reasons.
pub async fn check_import(state: &AppState, account_id: Uuid, csv_text: &str, file_name: Option<&str>, as_unsubscribed: bool) -> AppResult<()> {
    if !super::on(state) || as_unsubscribed || csv_text.len() > MAX_BYTES {
        return Ok(());
    }
    let file_name = file_name.map(|f| crate::deliverability::plain(f, 200)).filter(|f| !f.is_empty());
    let (headers, mut s, picked) = stats(csv_text);
    add_mail_servers(state, &mut s, &picked).await;
    let (rules_verdict, mut reasons) = rules::judge_import(file_name.as_deref(), &headers, &s);
    let mut signals = json!({"file_name": file_name, "headers": headers, "stats": s, "rules": rules_verdict.as_str()});
    let mut judged = None;
    let verdict = if rules_verdict == Verdict::Block {
        Verdict::Block
    } else {
        let material = json!({"file_name": file_name, "column_names": headers, "statistics": s});
        let task = "Task: decide whether this contact file, which the customer is importing to send marketing email to, looks like people who signed up with this business. \
                    You get only the file name, the column names and statistics over the addresses (shares of shared inboxes, throwaway and misspelled addresses, \
                    domains without mail servers, the most common address domains, and whether there's a consent or sign-up column); you never see a person's details. \
                    Pass an ordinary customer or subscriber list. Review a file that looks like it could be a prospect list or scraped. Block a file that is clearly a bought, scraped or prospect list.";
        match ai::judge(state, account_id, task, &material, AI_DEADLINE).await {
            Ok(j) => {
                signals["model"] = json!({"verdict": j.verdict.as_str(), "notes": j.notes});
                reasons.extend(j.reasons.iter().cloned());
                let v = j.verdict.max(rules_verdict);
                judged = Some(j);
                v
            }
            Err(NoJudgment::OverDailyLimit) => {
                signals["model"] = json!({"unavailable": NoJudgment::OverDailyLimit.describe()});
                reasons.push("A person will check this file before your next send (too many automatic checks today).".into());
                rules_verdict.max(Verdict::Review)
            }
            Err(e) => {
                // An outage doesn't stop imports: the rules decide (and a pending account's check runs later).
                signals["model"] = json!({"unavailable": e.describe()});
                if e == NoJudgment::Refused {
                    reasons.push("This file needs a person's check.".into());
                    rules_verdict.max(Verdict::Review)
                } else {
                    rules_verdict
                }
            }
        }
    };
    let mut row = New::rules("import", verdict, &reasons, signals);
    row.subject = Some("import");
    row.subject_name = file_name.clone();
    if let Some(j) = &judged {
        row = row.judged(j);
    }
    standing::record(&state.db, account_id, row).await?;
    let label = file_name.as_deref().unwrap_or("a CSV import");
    match verdict {
        Verdict::Pass => Ok(()),
        Verdict::Review => {
            let why = format!("Import {label} needs a look: {}", reasons.join(" "));
            standing::move_to(state, account_id, &[Standing::Approved], Standing::Review, "A file you imported needs a person's check before your next send.", &why).await.map_err(AppError::Internal)?;
            Ok(())
        }
        Verdict::Block => {
            let why = format!("Import {label} was refused: {}", reasons.join(" "));
            standing::move_to(state, account_id, &[Standing::Pending], Standing::Blocked, &reasons.join(" "), &why).await.map_err(AppError::Internal)?;
            standing::move_to(state, account_id, &[Standing::Approved], Standing::Review, "A file you tried to import needs a person's check.", &why).await.map_err(AppError::Internal)?;
            Err(AppError::bad(format!("This file wasn't imported: {}", reasons.join(" "))))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_count_without_keeping_addresses() {
        let csv = "Email,First Name,Signup Date\ninfo@shop.test,A,2026-01-01\njane@gmail.com,J,\nbob@gmial.com,B,\nnope,N,\n,E,\n";
        let (h, s, picked) = stats(csv);
        assert_eq!(h, vec!["Email", "First Name", "Signup Date"]);
        assert_eq!((s.rows, s.with_address, s.no_address, s.role, s.typo, s.consumer, s.company), (5, 3, 2, 1, 1, 1, 1));
        assert_eq!(s.consent_columns, vec!["Signup Date"]);
        assert_eq!(s.mx_checked, 1, "gmail.com has a mail server without a lookup");
        assert_eq!(picked, vec!["1\tshop.test"]);
        assert_eq!(s.top_domains.len(), 3);
    }
}
