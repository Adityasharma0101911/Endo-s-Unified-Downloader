//! Filename pattern formatting matching N_m3u8DL-RE `--save-pattern`.

use crate::stream_spec::StreamSpec;

/// Evaluates a save pattern template such as:
/// `<SaveName>_<Resolution>_<Bandwidth>bps`
/// or `<SaveName>_<Language>_<Channels>`
pub fn render_save_pattern(
    pattern: &str,
    save_name: Option<&str>,
    spec: &StreamSpec,
) -> String {
    let mut out = pattern.to_string();

    let save_name_val = save_name.unwrap_or("");
    out = out.replace("<SaveName>", save_name_val);

    let id_val = spec.id.as_deref().unwrap_or("");
    out = out.replace("<Id>", id_val);

    let codecs_val = spec.codecs.as_deref().unwrap_or("");
    out = out.replace("<Codecs>", codecs_val);

    let lang_val = spec.language.as_deref().unwrap_or("");
    out = out.replace("<Language>", lang_val);

    let res_val = spec.resolution.as_deref().unwrap_or("");
    out = out.replace("<Resolution>", res_val);

    let bw_val = if spec.bandwidth > 0 { spec.bandwidth.to_string() } else { String::new() };
    out = out.replace("<Bandwidth>", &bw_val);

    let media_type_val = spec.media_type.map(|t| t.to_string()).unwrap_or_default();
    out = out.replace("<MediaType>", &media_type_val);

    let channels_val = spec.channels.as_deref().unwrap_or("");
    out = out.replace("<Channels>", channels_val);

    let frame_val = spec.frame_rate.map(|f| format!("{f:.2}")).unwrap_or_default();
    out = out.replace("<FrameRate>", &frame_val);

    let range_val = spec.video_range.as_deref().unwrap_or("");
    out = out.replace("<VideoRange>", range_val);

    let group_val = spec.group_id.as_deref().unwrap_or("");
    out = out.replace("<GroupId>", group_val);

    let name_val = spec.name.as_deref().unwrap_or("");
    out = out.replace("<Name>", name_val);

    // Sanitize illegal filesystem characters: \ / : * ? " < > |
    sanitize_filename(&out)
}

/// Sanitize filename characters forbidden on Windows/Linux/macOS.
pub fn sanitize_filename(name: &str) -> String {
    let mut clean = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => {
                clean.push('_');
            }
            '\0'..='\x1f' => {}
            other => clean.push(other),
        }
    }
    let trimmed = clean.trim();
    if trimmed.is_empty() {
        "output".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_render_save_pattern() {
        let mut spec = StreamSpec::new("http://example.com/test.m3u8".into());
        spec.id = Some("v1080".into());
        spec.resolution = Some("1920x1080".into());
        spec.bandwidth = 5_000_000;
        spec.codecs = Some("avc1.640028".into());
        spec.language = Some("en".into());

        let res = render_save_pattern("<SaveName>_<Resolution>_<Bandwidth>bps", Some("movie"), &spec);
        assert_eq!(res, "movie_1920x1080_5000000bps");

        let res2 = render_save_pattern("<SaveName>_<Id>_<Language>", Some("clip"), &spec);
        assert_eq!(res2, "clip_v1080_en");
    }
}
