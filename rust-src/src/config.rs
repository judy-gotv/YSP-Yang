use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Channel {
    pub ch: String,
    #[serde(default)]
    pub logo: String,
    #[serde(default)]
    pub chinese: String,
    pub cnlid: String,
    pub livepid: String,
    #[serde(default)]
    pub group: String,
}

impl Channel {
    pub fn display_name(&self) -> &str {
        if self.chinese.trim().is_empty() {
            &self.ch
        } else {
            &self.chinese
        }
    }

    pub fn cache_key(&self) -> String {
        format!("{}:{}", self.cnlid, self.livepid)
    }
}

#[derive(Debug, Deserialize)]
struct ChannelFile {
    #[serde(default)]
    channels: Vec<Channel>,
}

#[derive(Debug, Clone)]
pub struct ChannelDirectory {
    pub path: PathBuf,
    pub channels: Vec<Channel>,
}

impl ChannelDirectory {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let text = fs::read_to_string(&path)
            .with_context(|| format!("read channels yaml: {}", path.display()))?;
        let file: ChannelFile = serde_yaml::from_str(&text)
            .with_context(|| format!("parse channels yaml: {}", path.display()))?;
        let channels: Vec<Channel> = file.channels.into_iter().filter(valid_channel).collect();
        anyhow::ensure!(!channels.is_empty(), "channels yaml has no valid channels");
        Ok(Self { path, channels })
    }

    pub fn resolve_ch(&self, ch: &str) -> Option<Channel> {
        let key = ch.trim().to_ascii_lowercase();
        self.channels
            .iter()
            .find(|item| item.ch.eq_ignore_ascii_case(&key))
            .cloned()
    }
}

fn valid_channel(channel: &Channel) -> bool {
    !channel.ch.trim().is_empty()
        && !channel.cnlid.trim().is_empty()
        && !channel.livepid.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_channel_yaml() {
        let file: ChannelFile = serde_yaml::from_str(
            r#"
channels:
  - ch: cctv1
    logo: "https://example/logo.png"
    chinese: "CCTV-1 综合"
    cnlid: "2024078201"
    livepid: "600001859"
    group: "央视"
"#,
        )
        .unwrap();
        assert_eq!(file.channels.len(), 1);
        assert_eq!(file.channels[0].display_name(), "CCTV-1 综合");
        assert_eq!(file.channels[0].cache_key(), "2024078201:600001859");
    }
}
