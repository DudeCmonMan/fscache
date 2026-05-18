use std::path::{Path, PathBuf};

use crate::preset::{CacheAction, CachePreset, ProcessFilterPolicy, ProcessInfo, RuleContext};
use crate::prediction_utils;

/// SxxExx episode prediction with Plex Transcoder awareness.
///
/// On every cache miss, scans the backing store for the next `lookahead` episodes
/// using SxxExx regex logic (same-season and cross-season, both structured and flat
/// directory layouts). Additionally distinguishes Plex Transcoder invocations doing
/// real user playback from those doing background analysis (intro/credit detection,
/// thumbnail gen, etc.) — the Plex-specific check only fires when the opener is
/// "Plex Transcoder", so non-Plex users get pure SxxExx prediction.
///
/// Plex Transcoder serves two purposes:
///   - Playback   → always uses a streaming container format (-f dash, ssegment, mpegts)
///   - Detection  → uses non-streaming formats (-f flac, null, chromaprint, image2)
///                  and/or writes output to /Transcode/Detection/
///
/// A playback transcode may include a secondary -f null stream (e.g. subtitle extraction);
/// the presence of any streaming format takes precedence and keeps the open un-filtered.
pub struct PlexEpisodePrediction {
    pub lookahead: usize,
    pub process_policy: ProcessFilterPolicy,
    /// If true, on_hit() also triggers lookahead — keeps the next N episodes always loaded.
    pub rolling_buffer: bool,
}

impl PlexEpisodePrediction {
    pub fn new(lookahead: usize, blocklist: Vec<String>, rolling_buffer: bool) -> Self {
        Self::new_with_process_policy(lookahead, Vec::new(), blocklist, rolling_buffer)
    }

    pub fn new_with_process_policy(
        lookahead: usize,
        process_allowlist: Vec<String>,
        process_blocklist: Vec<String>,
        rolling_buffer: bool,
    ) -> Self {
        Self {
            lookahead,
            process_policy: ProcessFilterPolicy::new(process_allowlist, process_blocklist),
            rolling_buffer,
        }
    }
}

const STREAMING_FORMATS: &[&[u8]] = &[b"dash", b"ssegment", b"mpegts"];

/// Playback always uses a streaming container format (-f dash, ssegment, or mpegts).
/// Background analysis and detection tasks use non-streaming formats.
fn is_plex_playback_cmdline(cmdline: &[u8]) -> bool {
    let mut prev: &[u8] = b"";
    for tok in cmdline.split(|&b| b == 0) {
        if prev == b"-f" && STREAMING_FORMATS.contains(&tok) {
            return true;
        }
        prev = tok;
    }
    false
}

impl CachePreset for PlexEpisodePrediction {
    fn name(&self) -> &str {
        "plex_episode_prediction"
    }

    fn should_filter(&self, process: &ProcessInfo) -> bool {
        // Check explicit process policy (allowlist first, then blocklist).
        if self.process_policy.should_filter(process) {
            return true;
        }
        // Plex Transcoder: cache only if the cmdline positively proves streamed playback.
        // No cmdline (unreadable /proc) or no streaming muxer means not playback → filter.
        if process.name.as_deref() == Some("Plex Transcoder") {
            return match process.cmdline {
                Some(ref c) => !is_plex_playback_cmdline(c),
                None => true,
            };
        }
        false
    }

    fn on_hit(&self, path: &Path, ctx: &RuleContext) -> Vec<CacheAction> {
        if !self.rolling_buffer { return vec![]; }
        // Reuse on_miss logic. ActionEngine already skips files that are cached/in-flight,
        // so returning the full lookahead list is safe — only gaps get queued.
        self.on_miss(path, ctx)
    }

    fn on_miss(&self, path: &Path, ctx: &RuleContext) -> Vec<CacheAction> {
        // Include the current episode — viewers often stop midway and resume later.
        let mut to_cache = vec![path.to_path_buf()];
        to_cache.extend(prediction_utils::find_next_episodes(path, ctx.backing_store, self.lookahead));
        vec![CacheAction::Cache(to_cache)]
    }

    fn deduplicate_key(&self, path: &Path) -> PathBuf {
        prediction_utils::show_root(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preset::ProcessInfo;

    fn make_preset() -> PlexEpisodePrediction {
        PlexEpisodePrediction::new(4, vec![], false)
    }

    fn transcoder(cmdline: &[u8]) -> ProcessInfo {
        ProcessInfo {
            pid: 0,
            name: Some("Plex Transcoder".into()),
            cmdline: Some(cmdline.to_vec()),
            ancestors: vec![],
        }
    }

    fn transcoder_no_cmdline() -> ProcessInfo {
        ProcessInfo {
            pid: 0,
            name: Some("Plex Transcoder".into()),
            cmdline: None,
            ancestors: vec![],
        }
    }

    fn other_process(name: &str) -> ProcessInfo {
        ProcessInfo {
            pid: 0,
            name: Some(name.into()),
            cmdline: Some(b"some\0args".to_vec()),
            ancestors: vec![],
        }
    }

    // --- Playback shapes: should NOT be filtered ---

    #[test]
    fn playback_ssegment_not_filtered() {
        let cmdline = b"Plex Transcoder\0-i\0input.mkv\
            \0-f\0ssegment\0-segment_format\0mpegts\0media-%05d.ts";
        assert!(!make_preset().should_filter(&transcoder(cmdline)));
    }

    #[test]
    fn playback_dash_not_filtered() {
        let cmdline = b"Plex Transcoder\0-i\0input.mkv\
            \0-progressurl\0http://127.0.0.1:32400/.../progress\
            \0-f\0dash\0manifest.mpd";
        assert!(!make_preset().should_filter(&transcoder(cmdline)));
    }

    #[test]
    fn playback_mpegts_pipe_not_filtered() {
        // Direct-stream to pipe: real playback mode, no segment manifest.
        let cmdline = b"Plex Transcoder\0-i\0input.ts\
            \0-progressurl\0http://127.0.0.1:32400/.../progress\
            \0-f\0mpegts\0pipe:1";
        assert!(!make_preset().should_filter(&transcoder(cmdline)));
    }

    #[test]
    fn playback_dash_with_secondary_null_not_filtered() {
        // Subtitle extraction adds -f null; streaming format takes precedence.
        let cmdline = b"Plex Transcoder\0-i\0input.mkv\
            \0-f\0dash\0manifest.mpd\
            \0-map\x000:2\0-f\0null\0-codec\0ass\0nullfile";
        assert!(!make_preset().should_filter(&transcoder(cmdline)));
    }

    // --- Detection / analysis shapes: MUST be filtered ---

    #[test]
    fn detection_null_filtered() {
        let cmdline = b"Plex Transcoder\0-i\0input.mkv\0-vn\0-f\0null\0-";
        assert!(make_preset().should_filter(&transcoder(cmdline)));
    }

    #[test]
    fn detection_chromaprint_filtered() {
        let cmdline = b"Plex Transcoder\0-i\0input.mkv\0-f\0chromaprint\0-";
        assert!(make_preset().should_filter(&transcoder(cmdline)));
    }

    #[test]
    fn detection_flac_filtered() {
        // Credits detection: -f flac to Detection path, -progressurl present.
        let cmdline = b"Plex Transcoder\0-i\0input.mkv\
            \0-progressurl\0http://127.0.0.1:32400/.../progress\
            \0-codec:0\0flac\0-f\0flac\
            \0/dev/shm/Transcode/Detection/abc123\
            \0-f\0null\0nullfile";
        assert!(make_preset().should_filter(&transcoder(cmdline)));
    }

    #[test]
    fn detection_image2_thumbnails_filtered() {
        let cmdline = b"Plex Transcoder\0-i\0input.mkv\
            \0-skip_frame\0noref\0-vf\0fps=0.5,scale=w=320:h=320\
            \0-f\0image2\0thumb-%05d.jpeg";
        assert!(make_preset().should_filter(&transcoder(cmdline)));
    }

    #[test]
    fn unknown_format_filtered() {
        // Unknown purpose, no streaming muxer → not playback → filter (fail-safe).
        let cmdline = b"Plex Transcoder\0-i\0input.mkv\0-codec\0copy\0output.mkv";
        assert!(make_preset().should_filter(&transcoder(cmdline)));
    }

    #[test]
    fn detection_output_path_unknown_format_filtered() {
        let cmdline = b"Plex Transcoder\0-i\0input.mkv\
            \0-f\0someunknownformat\
            \0/dev/shm/Transcode/Detection/abc123";
        assert!(make_preset().should_filter(&transcoder(cmdline)));
    }

    // --- Fail-safe: unreadable cmdline → filter ---

    #[test]
    fn no_cmdline_filtered() {
        assert!(make_preset().should_filter(&transcoder_no_cmdline()));
    }

    // --- Other processes are unaffected by the transcoder branch ---

    #[test]
    fn other_process_not_filtered() {
        assert!(!make_preset().should_filter(&other_process("Plex Media Server")));
    }

    // --- Blocklist still applies ---

    #[test]
    fn blocklist_blocks_scanner() {
        let preset = PlexEpisodePrediction::new(4, vec!["Plex Media Scanner".into()], false);
        assert!(preset.should_filter(&other_process("Plex Media Scanner")));
    }

    #[test]
    fn allowlist_beats_blocklist() {
        let preset = PlexEpisodePrediction::new_with_process_policy(
            4,
            vec!["Plex Media Server".into()],
            vec!["Plex Media Server".into()],
            false,
        );
        assert!(!preset.should_filter(&other_process("Plex Media Server")));
        assert!(preset.should_filter(&other_process("Plex Media Scanner")));
    }

    #[test]
    fn allowlist_does_not_bypass_transcoder_safety_filter() {
        let preset = PlexEpisodePrediction::new_with_process_policy(
            4,
            vec!["Plex Transcoder".into()],
            vec![],
            false,
        );
        let cmdline = b"Plex Transcoder\0-i\0input.mkv\0-vn\0-f\0null\0-";
        assert!(preset.should_filter(&transcoder(cmdline)));
    }

}
