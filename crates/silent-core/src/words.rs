//! 256 short, distinct, easy-to-say English words. Each word encodes one byte,
//! used for both invite codes and safety words.

pub const WORDS: [&str; 256] = [
    "acid", "acorn", "actor", "adapt", "agent", "alarm", "album", "alpine",
    "amber", "angle", "ankle", "apple", "april", "arena", "armor", "arrow",
    "atlas", "attic", "audio", "autumn", "bacon", "badge", "bagel", "baker",
    "bamboo", "banjo", "barn", "basil", "beach", "bean", "bear", "bell",
    "berry", "bison", "blade", "blank", "blaze", "bloom", "board", "boat",
    "bolt", "bonus", "boots", "brave", "bread", "brick", "bridge", "brook",
    "brush", "bubble", "bucket", "buddy", "cabin", "cable", "cactus", "camel",
    "camera", "candle", "canoe", "canyon", "cargo", "carrot", "castle", "cedar",
    "chalk", "charm", "cherry", "chess", "chief", "cider", "cinema", "circus",
    "citrus", "clay", "cliff", "clock", "cloud", "clover", "cobra", "cocoa",
    "comet", "coral", "cotton", "cowboy", "crane", "crayon", "creek", "crown",
    "cube", "curtain", "daisy", "dance", "delta", "denim", "desert", "diamond",
    "dingo", "disco", "dolphin", "donut", "dragon", "dream", "drift", "drum",
    "eagle", "earth", "echo", "elbow", "ember", "engine", "falcon", "fern",
    "fiddle", "fig", "flame", "flute", "forest", "fossil", "fox", "frost",
    "galaxy", "garden", "garlic", "gecko", "giant", "ginger", "glacier", "globe",
    "goat", "gold", "gopher", "grape", "gravel", "guitar", "hammer", "harbor",
    "hazel", "helmet", "hero", "honey", "hornet", "hotel", "husky", "igloo",
    "indigo", "iris", "island", "ivory", "jacket", "jaguar", "jazz", "jelly",
    "jewel", "jungle", "kayak", "kettle", "kiwi", "koala", "ladder", "lagoon",
    "lamp", "lantern", "laser", "lemon", "lily", "lime", "lizard", "locket",
    "lotus", "lunar", "magnet", "mango", "maple", "marble", "meadow", "melon",
    "meteor", "mint", "mirror", "moose", "mosaic", "motor", "muffin", "nectar",
    "needle", "nickel", "noodle", "novel", "oak", "oasis", "ocean", "olive",
    "onion", "opal", "orbit", "otter", "owl", "oyster", "paddle", "panda",
    "paper", "parrot", "peach", "pearl", "pebble", "pepper", "piano", "pickle",
    "pilot", "pine", "pirate", "pixel", "planet", "plum", "polar", "pony",
    "poppy", "puzzle", "quartz", "quest", "quilt", "rabbit", "radar", "radio",
    "raven", "reef", "rhino", "ribbon", "river", "robot", "rocket", "ruby",
    "saddle", "salmon", "satin", "scarf", "shadow", "shark", "shell", "silver",
    "sketch", "sky", "sloth", "snow", "solar", "spark", "spider", "spoon",
    "storm", "sugar", "summer", "sunset", "tiger", "tulip", "velvet", "walnut",
];

#[cfg(test)]
mod tests {
    use super::WORDS;
    use std::collections::HashSet;

    #[test]
    fn words_are_unique_lowercase_and_short() {
        let unique: HashSet<_> = WORDS.iter().collect();
        assert_eq!(unique.len(), WORDS.len());
        for w in WORDS {
            assert!(w.chars().all(|c| c.is_ascii_lowercase()), "{w}");
            assert!((3..=7).contains(&w.len()), "{w}");
        }
    }

    #[test]
    fn no_word_is_a_prefix_of_another() {
        for a in WORDS {
            for b in WORDS {
                if a != b {
                    assert!(!b.starts_with(a), "{a} is a prefix of {b}");
                }
            }
        }
    }
}
