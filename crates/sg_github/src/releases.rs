//! A repository's latest release, for SuperGIT's own update checks.

use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

/// A published release: not a draft or a pre-release.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    /// The tag, such as `v0.4.0`.
    pub tag: String,
    /// The release page.
    pub url: String,
}

#[derive(Deserialize)]
struct Payload {
    tag_name: String,
    html_url: String,
}

/// The latest release of `repository` (`owner/name`) on github.com. `gh`
/// first (the user's auth and rate limit); when it is missing or signed
/// out, the public API through `curl`.
pub fn latest_release(repository: &str) -> Result<Release> {
    let path = format!("repos/{repository}/releases/latest");
    let json = match crate::gh(&std::env::temp_dir(), ["api", &path]) {
        Ok(json) => json,
        Err(gh_error) => curl(&format!("https://api.github.com/{path}"))
            .with_context(|| format!("{gh_error:#}"))?,
    };
    parse(&json)
}

fn parse(json: &[u8]) -> Result<Release> {
    let payload: Payload = serde_json::from_slice(json).context("unexpected release JSON")?;
    Ok(Release {
        tag: payload.tag_name,
        url: payload.html_url,
    })
}

fn curl(url: &str) -> Result<Vec<u8>> {
    let out = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--fail",
            "--location",
            "--max-time",
            "30",
            "--header",
            "Accept: application/vnd.github+json",
            url,
        ])
        .stdin(Stdio::null())
        .output()
        .context("cannot run curl")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("curl {url}: {}", stderr.trim());
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_tag_and_page_from_the_api_payload() {
        let json = br#"{"tag_name":"v0.3.0","name":"v0.3.0","draft":false,
            "prerelease":false,"html_url":"https://github.com/Aitem/SuperGIT/releases/tag/v0.3.0",
            "assets":[{"name":"SHA256SUMS"}]}"#;
        assert_eq!(
            parse(json).unwrap(),
            Release {
                tag: "v0.3.0".into(),
                url: "https://github.com/Aitem/SuperGIT/releases/tag/v0.3.0".into(),
            }
        );
        assert!(parse(br#"{"message":"Not Found"}"#).is_err());
    }
}

/// Against github.com: `cargo test -p sg_github live_latest_release -- --ignored`.
/// Without `gh` on PATH it exercises the `curl` fallback.
#[cfg(test)]
mod live {
    #[test]
    #[ignore = "network"]
    fn live_latest_release() {
        let release = super::latest_release("Aitem/SuperGIT").unwrap();
        assert!(release.tag.starts_with('v'), "{release:?}");
        assert!(
            release
                .url
                .starts_with("https://github.com/Aitem/SuperGIT/releases/tag/")
        );
    }
}
