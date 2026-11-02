//! Busy-line animation helpers.
//!
//! Everything here is pure and deterministic so tests can pin frames, verbs,
//! and elapsed formatting. The run loop owns the clock and only advances the
//! frame while a turn is busy.

use std::time::Duration;

/// Frames advance every 120 ms.
pub const FRAME_INTERVAL: Duration = Duration::from_millis(120);

const FRAMES: [&str; 4] = ["|", "/", "-", "\\"];

/// The built-in spinner verb set.
///
/// These are the 185 Claude Code defaults collected in
/// <https://github.com/wynandw87/claude-code-spinner-verbs>, minus `Clauding`,
/// which names the provider directly and so would read as a product
/// announcement rather than a whimsical status. Kept public so frontend tests
/// can assert a rendered verb came from this exact set.
pub const VERBS: &[&str] = &[
    "Accomplishing",
    "Actioning",
    "Actualizing",
    "Architecting",
    "Baking",
    "Beaming",
    "Beboppin'",
    "Befuddling",
    "Billowing",
    "Blanching",
    "Bloviating",
    "Boogieing",
    "Boondoggling",
    "Booping",
    "Bootstrapping",
    "Brewing",
    "Burrowing",
    "Calculating",
    "Canoodling",
    "Caramelizing",
    "Cascading",
    "Catapulting",
    "Cerebrating",
    "Channeling",
    "Channelling",
    "Choreographing",
    "Churning",
    "Coalescing",
    "Cogitating",
    "Combobulating",
    "Composing",
    "Computing",
    "Concocting",
    "Considering",
    "Contemplating",
    "Cooking",
    "Crafting",
    "Creating",
    "Crunching",
    "Crystallizing",
    "Cultivating",
    "Deciphering",
    "Deliberating",
    "Determining",
    "Dilly-dallying",
    "Discombobulating",
    "Doing",
    "Doodling",
    "Drizzling",
    "Ebbing",
    "Effecting",
    "Elucidating",
    "Embellishing",
    "Enchanting",
    "Envisioning",
    "Evaporating",
    "Fermenting",
    "Fiddle-faddling",
    "Finagling",
    "Flambeing",
    "Flibbertigibbeting",
    "Flowing",
    "Flummoxing",
    "Fluttering",
    "Forging",
    "Forming",
    "Frolicking",
    "Frosting",
    "Gallivanting",
    "Galloping",
    "Garnishing",
    "Generating",
    "Germinating",
    "Gitifying",
    "Grooving",
    "Gusting",
    "Harmonizing",
    "Hashing",
    "Hatching",
    "Herding",
    "Honking",
    "Hullaballooing",
    "Hyperspacing",
    "Ideating",
    "Imagining",
    "Improvising",
    "Incubating",
    "Inferring",
    "Infusing",
    "Ionizing",
    "Jitterbugging",
    "Julienning",
    "Kneading",
    "Leavening",
    "Levitating",
    "Lollygagging",
    "Manifesting",
    "Marinating",
    "Meandering",
    "Metamorphosing",
    "Misting",
    "Moonwalking",
    "Moseying",
    "Mulling",
    "Mustering",
    "Musing",
    "Nebulizing",
    "Nesting",
    "Newspapering",
    "Noodling",
    "Nucleating",
    "Orbiting",
    "Orchestrating",
    "Osmosing",
    "Perambulating",
    "Percolating",
    "Perusing",
    "Philosophising",
    "Photosynthesizing",
    "Pollinating",
    "Pondering",
    "Pontificating",
    "Pouncing",
    "Precipitating",
    "Prestidigitating",
    "Processing",
    "Proofing",
    "Propagating",
    "Puttering",
    "Puzzling",
    "Quantumizing",
    "Razzle-dazzling",
    "Razzmatazzing",
    "Recombobulating",
    "Reticulating",
    "Roosting",
    "Ruminating",
    "Sauteing",
    "Scampering",
    "Schlepping",
    "Scurrying",
    "Seasoning",
    "Shenaniganing",
    "Shimmying",
    "Simmering",
    "Skedaddling",
    "Sketching",
    "Slithering",
    "Smooshing",
    "Sock-hopping",
    "Spelunking",
    "Spinning",
    "Sprouting",
    "Stewing",
    "Sublimating",
    "Swirling",
    "Swooping",
    "Symbioting",
    "Synthesizing",
    "Tempering",
    "Thinking",
    "Thundering",
    "Tinkering",
    "Tomfoolering",
    "Topsy-turvying",
    "Transfiguring",
    "Transmuting",
    "Twisting",
    "Undulating",
    "Unfurling",
    "Unravelling",
    "Vibing",
    "Waddling",
    "Wandering",
    "Warping",
    "Whatchamacalliting",
    "Whirlpooling",
    "Whirring",
    "Whisking",
    "Wibbling",
    "Working",
    "Wrangling",
    "Zesting",
    "Zigzagging",
];

/// The current ASCII frame.
pub fn frame(phase: usize) -> &'static str {
    FRAMES[phase % FRAMES.len()]
}

/// The number of fully elapsed frame phases.
pub fn frame_phase(elapsed: Duration) -> usize {
    (elapsed.as_millis() / FRAME_INTERVAL.as_millis().max(1)) as usize
}

/// One whimsical verb for this turn, changing every 20 seconds.
pub fn verb(seed: u64, bucket: u64) -> &'static str {
    let mixed = mix(seed ^ bucket.wrapping_mul(0x9e37_79b9_7f4a_7c15));

    VERBS[(mixed % VERBS.len() as u64) as usize]
}

/// Which twenty-second bucket a verb belongs to.
pub fn verb_bucket(elapsed: Duration) -> u64 {
    elapsed.as_secs() / 20
}

/// A stable per-turn seed derived from the turn number and session id.
pub fn turn_seed(turn: u64, session_id: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;

    for byte in session_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }

    mix(hash ^ turn.wrapping_mul(0x9e37_79b9_7f4a_7c15))
}

/// `3s` below a minute, `1m04s` above.
pub fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();

    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    }
}

/// A small deterministic mixer; no `rand` dependency is needed.
fn mix(mut value: u64) -> u64 {
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);

    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{format_elapsed, frame, frame_phase, turn_seed, verb, verb_bucket};

    #[test]
    fn the_spinner_advances_frames_verbs_and_elapsed_time() {
        assert_eq!(frame(0), "|");
        assert_eq!(frame(1), "/");
        assert_eq!(frame(2), "-");
        assert_eq!(frame(3), "\\");
        assert_eq!(frame(4), "|");

        assert_eq!(frame_phase(Duration::from_millis(0)), 0);
        assert_eq!(frame_phase(Duration::from_millis(119)), 0);
        assert_eq!(frame_phase(Duration::from_millis(120)), 1);
        assert_eq!(frame_phase(Duration::from_millis(360)), 3);

        assert_eq!(verb_bucket(Duration::from_secs(0)), 0);
        assert_eq!(verb_bucket(Duration::from_secs(19)), 0);
        assert_eq!(verb_bucket(Duration::from_secs(20)), 1);
        assert_eq!(verb_bucket(Duration::from_secs(59)), 2);

        assert_eq!(format_elapsed(Duration::from_secs(0)), "0s");
        assert_eq!(format_elapsed(Duration::from_secs(3)), "3s");
        assert_eq!(format_elapsed(Duration::from_secs(59)), "59s");
        assert_eq!(format_elapsed(Duration::from_secs(64)), "1m04s");
        assert_eq!(format_elapsed(Duration::from_secs(600)), "10m00s");
    }

    #[test]
    fn the_verb_is_stable_per_seed_and_never_empty() {
        let seed = turn_seed(7, "session-1");

        assert_eq!(verb(seed, 0), verb(seed, 0));
        assert_eq!(seed, turn_seed(7, "session-1"));
        assert_ne!(seed, turn_seed(8, "session-1"));
        assert_ne!(seed, turn_seed(7, "session-2"));

        let later = verb(seed, 1);
        let buckets = (0..64).map(|bucket| verb(seed, bucket)).collect::<Vec<_>>();

        assert!(buckets.contains(&later));
        assert!(buckets.iter().all(|entry| !entry.is_empty()));
    }
}
