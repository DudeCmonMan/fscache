use std::path::{Path, PathBuf};

use crate::preset::{CacheAction, CachePreset, ProcessInfo, RuleContext};
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
    pub blocklist: Vec<String>,
    /// If true, on_hit() also triggers lookahead — keeps the next N episodes always loaded.
    pub rolling_buffer: bool,
}

impl PlexEpisodePrediction {
    pub fn new(lookahead: usize, blocklist: Vec<String>, rolling_buffer: bool) -> Self {
        Self { lookahead, blocklist, rolling_buffer }
    }
}

const STREAMING_FORMATS: &[&[u8]] = &[b"dash", b"ssegment", b"mpegts"];
const ANALYSIS_FORMATS:  &[&[u8]] = &[b"null", b"chromaprint", b"flac", b"image2"];

/// Real playback always uses one of these streaming container formats.
fn has_streaming_output(args: &[&[u8]]) -> bool {
    args.windows(2).any(|pair| pair[0] == b"-f" && STREAMING_FORMATS.contains(&pair[1]))
}

/// Detection transcoders use non-streaming output formats.
fn has_analysis_output(args: &[&[u8]]) -> bool {
    args.windows(2).any(|pair| pair[0] == b"-f" && ANALYSIS_FORMATS.contains(&pair[1]))
}

/// Detection transcoders write to /Transcode/Detection/ — catches future analysis
/// modes that may use formats not yet in ANALYSIS_FORMATS.
fn has_detection_output_path(cmdline: &[u8]) -> bool {
    cmdline.windows(b"/Transcode/Detection/".len())
        .any(|w| w == b"/Transcode/Detection/")
}

/// Returns true if this cmdline belongs to a Plex Transcoder doing analysis, not playback.
fn is_plex_detection_cmdline(cmdline: &[u8]) -> bool {
    let args: Vec<&[u8]> = cmdline.split(|&b| b == 0).collect();
    if has_streaming_output(&args) {
        return false;
    }
    has_analysis_output(&args) || has_detection_output_path(cmdline)
}

impl CachePreset for PlexEpisodePrediction {
    fn name(&self) -> &str {
        "plex_episode_prediction"
    }

    fn should_filter(&self, process: &ProcessInfo) -> bool {
        // Check the explicit blocklist (Scanner, EAE Service, Fingerprinter, etc.).
        if !self.blocklist.is_empty() && process.is_blocked_by(&self.blocklist) {
            return true;
        }
        // Check if this Plex Transcoder is doing detection work rather than playback.
        if process.name.as_deref() == Some("Plex Transcoder") {
            if let Some(ref cmdline) = process.cmdline {
                return is_plex_detection_cmdline(cmdline);
            }
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

    fn split(cmdline: &[u8]) -> Vec<&[u8]> {
        cmdline.split(|&b| b == 0).collect()
    }

    #[test]
    fn detection_null_output() {
        let cmdline = b"Plex Transcoder\0-i\0input.mkv\0-vn\0-f\0null\0-";
        assert!(has_analysis_output(&split(cmdline)));
        assert!(!has_streaming_output(&split(cmdline)));
    }

    #[test]
    fn detection_chromaprint_output() {
        let cmdline = b"Plex Transcoder\0-i\0input.mkv\0-f\0chromaprint\0-";
        assert!(has_analysis_output(&split(cmdline)));
        assert!(!has_streaming_output(&split(cmdline)));
    }

    // Credits detection transcoder: has -progressurl but is NOT playback.
    // Primary output is -f flac to /dev/shm/Transcode/Detection/, secondary -f null.
    // Previously this slipped through the -progressurl gate — caught by format check now.
    #[test]
    fn detection_flac_with_progressurl() {
        let cmdline = b"Plex Transcoder\0-i\0input.mkv\
            \0-progressurl\0http://127.0.0.1:32400/.../progress\
            \0-codec:0\0flac\0-f\0flac\
            \0/dev/shm/Transcode/Detection/abc123\
            \0-f\0null\0nullfile";
        assert!(!has_streaming_output(&split(cmdline)));
        assert!(has_analysis_output(&split(cmdline)));
        assert!(has_detection_output_path(cmdline));
    }

    #[test]
    fn detection_image2_thumbnails() {
        let cmdline = b"Plex Transcoder\0-i\0input.mkv\
            \0-skip_frame\0noref\0-vf\0fps=0.5,scale=w=320:h=320\
            \0-f\0image2\0thumb-%05d.jpeg";
        assert!(!has_streaming_output(&split(cmdline)));
        assert!(has_analysis_output(&split(cmdline)));
    }

    // Path check catches analysis even when format is unrecognized.
    #[test]
    fn detection_output_path() {
        let cmdline = b"Plex Transcoder\0-i\0input.mkv\
            \0-f\0someunknownformat\
            \0/dev/shm/Transcode/Detection/abc123";
        assert!(!has_streaming_output(&split(cmdline)));
        assert!(!has_analysis_output(&split(cmdline))); // format unknown
        assert!(has_detection_output_path(cmdline));    // but path catches it
    }

    // Bug case: playback with secondary -f null for subtitle extraction.
    // The -f dash streaming format takes precedence.
    #[test]
    fn playback_dash_with_secondary_null_not_detection() {
        let cmdline = b"Plex Transcoder\0-i\0input.mkv\
            \0-progressurl\0http://127.0.0.1:32400/.../progress\
            \0-f\0dash\0manifest.mpd\
            \0-map\x000:2\0-f\0null\0-codec\0ass\0nullfile";
        assert!(has_streaming_output(&split(cmdline)));
        assert!(has_analysis_output(&split(cmdline))); // secondary -f null present
    }

    #[test]
    fn playback_dash_not_detection() {
        let cmdline = b"Plex Transcoder\0-i\0input.mkv\
            \0-progressurl\0http://127.0.0.1:32400/.../progress\
            \0-f\0dash\0manifest.mpd";
        assert!(has_streaming_output(&split(cmdline)));
        assert!(!has_analysis_output(&split(cmdline)));
    }

    #[test]
    fn playback_ssegment_not_detection() {
        let cmdline = b"Plex Transcoder\0-i\0input.mkv\
            \0-progressurl\0http://127.0.0.1:32400/.../progress\
            \0-f\0ssegment\0-segment_format\0mp4\0media-%05d.ts";
        assert!(has_streaming_output(&split(cmdline)));
        assert!(!has_analysis_output(&split(cmdline)));
    }

    #[test]
    fn playback_mpegts_not_detection() {
        let cmdline = b"Plex Transcoder\0-i\0input.ts\
            \0-progressurl\0http://127.0.0.1:32400/.../progress\
            \0-f\0mpegts\0pipe:1";
        assert!(has_streaming_output(&split(cmdline)));
        assert!(!has_analysis_output(&split(cmdline)));
    }

    // Unknown purpose, no recognized signals → not detection (conservative default).
    #[test]
    fn unknown_transcoder_not_detection() {
        let cmdline = b"Plex Transcoder\0-i\0input.mkv\0-codec\0copy\0output.mkv";
        assert!(!has_streaming_output(&split(cmdline)));
        assert!(!has_analysis_output(&split(cmdline)));
        assert!(!has_detection_output_path(cmdline));
    }

    // End-to-end: is_plex_detection_cmdline combines all three signals.
    #[test]
    fn detection_cmdline_null() {
        let cmdline = b"Plex Transcoder\0-i\0input.mkv\0-f\0null\0-";
        assert!(is_plex_detection_cmdline(cmdline));
    }

    #[test]
    fn detection_cmdline_detection_path() {
        let cmdline = b"Plex Transcoder\0-i\0input.mkv\0-f\0unknownfmt\0/dev/shm/Transcode/Detection/xyz";
        assert!(is_plex_detection_cmdline(cmdline));
    }

    #[test]
    fn playback_cmdline_dash_not_detection() {
        let cmdline = b"Plex Transcoder\0-i\0input.mkv\0-f\0dash\0manifest.mpd";
        assert!(!is_plex_detection_cmdline(cmdline));
    }

    #[test]
    fn playback_cmdline_dash_with_null_not_detection() {
        let cmdline = b"Plex Transcoder\0-i\0input.mkv\0-f\0dash\0manifest.mpd\0-f\0null\0-";
        assert!(!is_plex_detection_cmdline(cmdline));
    }
}
