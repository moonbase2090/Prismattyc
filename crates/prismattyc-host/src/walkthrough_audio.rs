//! Bundled walkthrough narration (PT-197).
//!
//! Clips are generated offline by `scripts/walkthrough-voice.sh`. Runtime
//! never calls ElevenLabs. Missing clips, disabled audio, or no player
//! leave the caption flow unchanged.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

include!(concat!(env!("OUT_DIR"), "/walkthrough_clips.rs"));

const MANIFEST_JSON: &str = include_str!("../assets/walkthrough/manifest.json");
/// One-second minimum gap, matching the host bell-sound path.
pub(crate) const MIN_GAP: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Manifest {
    pub voice: String,
    #[serde(default)]
    pub voice_id: String,
    pub model: String,
    #[serde(default)]
    pub clips: Vec<ManifestClip>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct ManifestClip {
    pub step_id: String,
    pub path: String,
    #[serde(default)]
    pub voice_id: String,
    #[serde(default)]
    pub caption_hash: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlayDecision {
    Play,
    SkipDisabled,
    SkipMissing,
    SkipGap,
}

/// Whether to start a walkthrough clip. No `HostState`.
#[must_use]
pub(crate) fn play_decision(
    enabled: bool,
    clip_present: bool,
    since_last: Option<Duration>,
) -> PlayDecision {
    if !enabled {
        return PlayDecision::SkipDisabled;
    }
    if !clip_present {
        return PlayDecision::SkipMissing;
    }
    if since_last.is_some_and(|gap| gap < MIN_GAP) {
        return PlayDecision::SkipGap;
    }
    PlayDecision::Play
}

/// Linux `aplay` cannot decode OGG. Drop it from the player list for that
/// format so a paplay/pw-play miss does not try aplay.
#[must_use]
pub(crate) fn players_for(players: &[&'static str], path: &Path) -> Vec<&'static str> {
    let ogg = path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("ogg"));
    players
        .iter()
        .copied()
        .filter(|player| !(ogg && *player == "aplay"))
        .collect()
}

pub(crate) fn load_manifest() -> Result<Manifest, serde_json::Error> {
    serde_json::from_str(MANIFEST_JSON)
}

#[must_use]
pub(crate) fn clip_lookup(step_id: &str) -> Option<(&'static [u8], &'static str)> {
    let bytes = CLIPS
        .iter()
        .find(|(id, _)| *id == step_id)
        .map(|(_, bytes)| *bytes)?;
    let path = load_manifest()
        .ok()?
        .clips
        .into_iter()
        .find(|clip| clip.step_id == step_id)
        .map(|clip| clip.path)?;
    let ext = if path
        .rsplit('.')
        .next()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("mp3"))
    {
        "mp3"
    } else {
        "ogg"
    };
    Some((bytes, ext))
}

pub(crate) fn materialize_clip(step_id: &str, bytes: &[u8], ext: &str) -> Option<PathBuf> {
    let safe: String = step_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '.' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let path = std::env::temp_dir().join(format!("prismattyc-walkthrough-{safe}.{ext}"));
    let fresh = std::fs::metadata(&path).is_ok_and(|meta| meta.len() == bytes.len() as u64);
    if !fresh && std::fs::write(&path, bytes).is_err() {
        return None;
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_manifest_parses_and_has_no_clips() {
        let manifest = load_manifest().expect("bundled manifest");
        assert_eq!(manifest.voice, "Russ");
        assert_eq!(manifest.model, "eleven_multilingual_v2");
        assert!(manifest.clips.is_empty());
        assert!(clip_lookup("window.split-right").is_none());
        assert!(clip_lookup("intro").is_none());
        assert!(CLIPS.is_empty());
    }

    #[test]
    fn play_decision_gap_and_skip_table() {
        assert_eq!(play_decision(false, true, None), PlayDecision::SkipDisabled);
        assert_eq!(play_decision(true, false, None), PlayDecision::SkipMissing);
        assert_eq!(
            play_decision(true, true, Some(Duration::from_millis(200))),
            PlayDecision::SkipGap
        );
        assert_eq!(
            play_decision(true, true, Some(Duration::from_secs(1))),
            PlayDecision::Play
        );
        assert_eq!(play_decision(true, true, None), PlayDecision::Play);
    }

    #[test]
    fn players_for_drops_aplay_on_ogg() {
        let players = ["paplay", "pw-play", "aplay"];
        assert_eq!(
            players_for(&players, Path::new("intro.ogg")),
            ["paplay", "pw-play"]
        );
        assert_eq!(
            players_for(&players, Path::new("intro.mp3")),
            ["paplay", "pw-play", "aplay"]
        );
        assert!(players_for(&["aplay"], Path::new("x.ogg")).is_empty());
    }

    #[test]
    fn walkthrough_assets_under_2_mib() {
        const MAX_BUNDLE_BYTES: u64 = 2 * 1024 * 1024;
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/walkthrough");
        let mut sum = 0u64;
        for entry in std::fs::read_dir(&dir).expect("walkthrough assets dir") {
            let path = entry.unwrap().path();
            let Some(ext) = path.extension() else {
                continue;
            };
            if ext.eq_ignore_ascii_case("ogg") || ext.eq_ignore_ascii_case("mp3") {
                sum += std::fs::metadata(&path).unwrap().len();
            }
        }
        assert!(
            sum <= MAX_BUNDLE_BYTES,
            "walkthrough clips are {sum} bytes; cap is {MAX_BUNDLE_BYTES}"
        );
    }

    #[test]
    fn voice_script_dry_run_lists_intro_and_steps() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let script = root.join("scripts/walkthrough-voice.sh");
        let output = std::process::Command::new("bash")
            .arg(&script)
            .arg("--dry-run")
            .arg("--voice")
            .arg("Russ")
            .current_dir(&root)
            .output()
            .expect("run walkthrough-voice.sh --dry-run");
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains("voice=Russ"), "{text}");
        assert!(text.contains("source=flag"), "{text}");
        assert!(text.contains("intro\t"), "{text}");
        assert!(text.contains("window.split-right\t"), "{text}");
        assert!(text.contains("power.find\t"), "{text}");
        assert!(text.contains("Welcome to the Prismattyc walkthrough."));
    }
}
