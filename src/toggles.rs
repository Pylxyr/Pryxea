//! Feature switches stored in `data/toggles.json`. Only radio autoplay is
//! left; it is flipped by `!radio on|off` (mods) and the settings page.

use serde_json::Value;

use crate::store::JsonMap;

pub const RADIO_KEY: &str = "radio_autoplay_enabled";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toggles {
    /// On by default: it closes the "radio silence" gap the feature exists for.
    pub radio_autoplay_enabled: bool,
}

impl Default for Toggles {
    fn default() -> Self {
        Toggles { radio_autoplay_enabled: true }
    }
}

impl Toggles {
    pub fn from_map(map: &JsonMap) -> Toggles {
        let radio = match map.get(RADIO_KEY) {
            Some(Value::Bool(b)) => *b,
            _ => Toggles::default().radio_autoplay_enabled,
        };
        Toggles { radio_autoplay_enabled: radio }
    }

    pub fn to_map(&self) -> JsonMap {
        JsonMap::from_iter([(RADIO_KEY.to_string(), Value::Bool(self.radio_autoplay_enabled))])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_on_and_ignores_non_booleans() {
        assert!(Toggles::from_map(&JsonMap::new()).radio_autoplay_enabled);
        let junk = json!({ RADIO_KEY: "yes" }).as_object().cloned().unwrap();
        assert!(Toggles::from_map(&junk).radio_autoplay_enabled);
        let off = json!({ RADIO_KEY: false }).as_object().cloned().unwrap();
        assert!(!Toggles::from_map(&off).radio_autoplay_enabled);
    }
}
