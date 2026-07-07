use super::{Metric, Snapshot};

const ID: &str = "kiro";
const NAME: &str = "Kiro";

pub async fn snapshot() -> Snapshot {
    match fetch().await {
        Ok(s) => s,
        Err(e) => Snapshot::error(ID, NAME, e),
    }
}

async fn run_cli(args: &[&str], secs: u64) -> Option<String> {
    // CREATE_NO_WINDOW: this runs on every refresh once kiro-cli is
    // installed — without it, a console flashes over whatever the user is
    // doing every five minutes.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(secs),
        tokio::process::Command::new("cmd")
            .args(["/C", "kiro-cli"])
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).to_string()
        + &String::from_utf8_lossy(&out.stderr);
    (!text.trim().is_empty()).then(|| strip_ansi(&text))
}

fn strip_ansi(s: &str) -> String {
    regex::Regex::new(r"\x1B\[[0-9;?]*[A-Za-z]|\x1B\].*?\x07")
        .unwrap()
        .replace_all(s, "")
        .into_owned()
}

async fn fetch() -> Result<Snapshot, String> {
    // Installed at all? Scan PATH ourselves — spawning `where` via cmd
    // flashed a console window on every refresh for everyone without Kiro.
    let found = std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                ["kiro-cli.exe", "kiro-cli.cmd", "kiro-cli.bat"]
                    .iter()
                    .any(|name| dir.join(name).is_file())
            })
        })
        .unwrap_or(false);
    if !found {
        return Ok(Snapshot::no_credentials(ID, NAME, "Kiro CLI (kiro-cli) is not installed."));
    }

    let Some(text) = run_cli(&["chat", "--no-interactive", "/usage"], 20).await else {
        // The CLI detects a real terminal on macOS and can print nothing
        // through pipes — treat silence as "can't read", not "no account".
        return Err("kiro-cli produced no output — run `kiro-cli chat /usage` in a terminal to check your plan".into());
    };
    let lower = text.to_lowercase();
    if ["not logged in", "login required", "kiro-cli login", "oauth error"]
        .iter()
        .any(|m| lower.contains(m))
    {
        return Ok(Snapshot::no_credentials(ID, NAME, "Sign in with `kiro-cli login` first."));
    }

    let mut used_pct: Option<f64> = None;
    if let Some(c) = regex::Regex::new(r"█+\s*(\d+)%").unwrap().captures(&text) {
        used_pct = c[1].parse().ok();
    }
    let mut detail = None;
    if let Some(c) = regex::Regex::new(r"\((\d+\.?\d*)\s+of\s+(\d+)\s+covered").unwrap().captures(&text)
    {
        let used: f64 = c[1].parse().unwrap_or(0.0);
        let total: f64 = c[2].parse().unwrap_or(50.0);
        if used_pct.is_none() && total > 0.0 {
            used_pct = Some(used / total * 100.0);
        }
        detail = Some(format!("{used:.1} of {total:.0} credits"));
    }

    // "resets on 2026-08-01" or "resets on 08/01"
    let resets_at = regex::Regex::new(r"resets on (\d{4}-\d{2}-\d{2}|\d{2}/\d{2})")
        .unwrap()
        .captures(&text)
        .and_then(|c| {
            use chrono::{Datelike, NaiveDate, Utc};
            let s = &c[1];
            let date = if s.len() == 10 {
                NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
            } else {
                let (m, d) = s.split_once('/')?;
                let (m, d): (u32, u32) = (m.parse().ok()?, d.parse().ok()?);
                let now = Utc::now().date_naive();
                let mut date = NaiveDate::from_ymd_opt(now.year(), m, d)?;
                if date < now {
                    date = NaiveDate::from_ymd_opt(now.year() + 1, m, d)?;
                }
                Some(date)
            };
            date.map(|d| d.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis())
        });

    let plan = regex::Regex::new(r"(?m)Plan:\s*(.+)$")
        .unwrap()
        .captures(&text)
        .map(|c| c[1].trim().to_string())
        .or_else(|| {
            regex::Regex::new(r"\|\s*(KIRO\s+\w+)")
                .unwrap()
                .captures(&text)
                .map(|c| c[1].replace("KIRO", "Kiro").trim().to_string())
        });

    let Some(pct) = used_pct else {
        if lower.contains("managed by admin") || lower.contains("organization") {
            return Ok(Snapshot::ok(
                ID,
                NAME,
                plan,
                vec![Metric::text("Credits", "Managed by your organization".into())],
            ));
        }
        return Err("could not parse kiro-cli usage output".into());
    };

    let mut metrics =
        vec![Metric::progress("Credits", pct.clamp(0.0, 100.0), detail).with_reset(resets_at, None)];

    // "Bonus credits: 12.5/50 … expires in 9 days"
    if let Some(c) = regex::Regex::new(r"Bonus credits:\s*(\d+\.?\d*)/(\d+)").unwrap().captures(&text)
    {
        let used: f64 = c[1].parse().unwrap_or(0.0);
        let total: f64 = c[2].parse().unwrap_or(0.0);
        if total > 0.0 {
            let expiry = regex::Regex::new(r"expires in (\d+) days?")
                .unwrap()
                .captures(&text)
                .and_then(|c| c[1].parse::<i64>().ok())
                .map(|d| chrono::Utc::now().timestamp_millis() + d * 24 * 3600 * 1000);
            metrics.push(
                Metric::progress(
                    "Bonus credits",
                    (used / total * 100.0).clamp(0.0, 100.0),
                    Some(format!("{used:.1} of {total:.0}")),
                )
                .with_reset(expiry, None),
            );
        }
    }

    Ok(Snapshot::ok(ID, NAME, plan, metrics))
}
