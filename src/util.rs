use serde::Serialize;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

pub fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

pub fn iso_now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| format!("{}Z", now_epoch()))
}

pub fn iso_from_epoch(epoch: i64) -> String {
    OffsetDateTime::from_unix_timestamp(epoch)
        .ok()
        .and_then(|value| value.format(&Rfc3339).ok())
        .unwrap_or_else(|| format!("{epoch}Z"))
}

pub fn write_private_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("cannot secure {}: {error}", parent.display()))?;

    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|error| format!("cannot create {}: {error}", temporary.display()))?;
    serde_json::to_writer_pretty(&mut file, value)
        .map_err(|error| format!("cannot serialize {}: {error}", path.display()))?;
    file.write_all(b"\n")
        .map_err(|error| format!("cannot finish {}: {error}", temporary.display()))?;
    file.sync_all()
        .map_err(|error| format!("cannot sync {}: {error}", temporary.display()))?;
    fs::rename(&temporary, path)
        .map_err(|error| format!("cannot replace {}: {error}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("cannot secure {}: {error}", path.display()))?;
    Ok(())
}

pub fn scrub_log_line(line: &str) -> String {
    let mut scrubbed = line.chars().take(4096).collect::<String>();
    for prefix in [
        "board_",
        "ghp_",
        "github_pat_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
    ] {
        while let Some(start) = scrubbed.find(prefix) {
            let end = scrubbed[start..]
                .find(|character: char| {
                    character.is_whitespace() || matches!(character, '\'' | '"' | ',' | '}')
                })
                .map(|offset| start + offset)
                .unwrap_or(scrubbed.len());
            scrubbed.replace_range(start..end, "[redacted]");
        }
    }
    scrubbed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubs_known_secret_prefixes() {
        let input = "token board_abcdef and github_pat_deadbeef";
        let output = scrub_log_line(input);
        assert!(!output.contains("board_abcdef"));
        assert!(!output.contains("github_pat_deadbeef"));
    }
}
