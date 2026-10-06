use axum::http::{HeaderMap, Uri};

use crate::{
    config::Channel,
    constants::{EPG_URL, NOTICE_LOGO_URL, NOTICE_URL},
    prefix::{abs_url, append_recursive_prefix},
};

pub fn m3u_escape(value: impl AsRef<str>) -> String {
    value
        .as_ref()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace(['\r', '\n'], " ")
        .trim()
        .to_string()
}

pub fn build_list_m3u(headers: &HeaderMap, uri: &Uri, channels: &[Channel]) -> String {
    let mut lines = vec![
        format!("#EXTM3U x-tvg-url=\"{}\"", m3u_escape(EPG_URL)),
        format!(
            "#EXTINF:-1 tvg-name=\"注意事项\" tvg-logo=\"{}\" group-title=\"注意事项\",注意事项",
            m3u_escape(NOTICE_LOGO_URL)
        ),
        NOTICE_URL.to_string(),
    ];
    for channel in channels {
        let name = channel.display_name();
        lines.push(format!(
            "#EXTINF:-1 tvg-name=\"{}\" tvg-logo=\"{}\" group-title=\"{}\",{}",
            m3u_escape(name),
            m3u_escape(&channel.logo),
            m3u_escape(&channel.group),
            name
        ));
        let url = abs_url(headers, uri, &format!("/live/{}.m3u8", channel.ch));
        lines.push(append_recursive_prefix(uri, &url));
    }
    format!("{}\n", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use axum::http::{HeaderMap, HeaderValue};

    use super::*;

    #[test]
    fn list_m3u_uses_chinese_name_and_slug_url() {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("127.0.0.1:8787"));
        let uri: Uri = "/list.m3u".parse().unwrap();
        let channels = vec![Channel {
            ch: "shandongws".to_string(),
            logo: "https://cdn/logo/山东卫视.png".to_string(),
            chinese: "山东卫视".to_string(),
            cnlid: "1".to_string(),
            livepid: "2".to_string(),
            group: "卫视".to_string(),
        }];
        let text = build_list_m3u(&headers, &uri, &channels);
        assert!(text.contains("#EXTM3U x-tvg-url=\"https://epg.zsdc.eu.org/t.xml\""));
        assert!(text.contains("tvg-name=\"山东卫视\""));
        assert!(text.contains("group-title=\"卫视\",山东卫视"));
        assert!(text.contains("http://127.0.0.1:8787/live/shandongws.m3u8"));
    }
}
