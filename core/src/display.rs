//! Display-power transitions for a single active graphical seat.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayPower {
    On,
    Off,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Effect {
    Wake,
    PowerOff,
    Cancel,
}

#[derive(Default)]
pub struct DisplayPolicy {
    previous: Option<(String, DisplayPower)>,
    paused: bool,
    awaiting_display_on: bool,
}

impl DisplayPolicy {
    pub fn pause(&mut self) {
        self.previous = None;
        self.paused = true;
        self.awaiting_display_on = false;
    }

    pub fn reset(&mut self, after_resume: bool) {
        self.previous = None;
        self.paused = false;
        self.awaiting_display_on = after_resume;
    }

    pub fn observe(&mut self, session: String, power: DisplayPower) -> Option<Effect> {
        if self.paused {
            return None;
        }

        let old = self.previous.replace((session, power));
        let (same_session, previous) = match (&old, &self.previous) {
            (Some((old_session, previous)), Some((session, _))) => {
                (old_session == session, *previous)
            }
            _ => (false, DisplayPower::Unknown),
        };
        if power == DisplayPower::On && self.awaiting_display_on {
            self.awaiting_display_on = false;
            return Some(Effect::Wake);
        }

        if !same_session || power == DisplayPower::Unknown {
            return (old.is_some() && (!same_session || previous != power))
                .then_some(Effect::Cancel);
        }

        match (previous, power) {
            (DisplayPower::Off, DisplayPower::On) => Some(Effect::Wake),
            (DisplayPower::On, DisplayPower::Off) => Some(Effect::PowerOff),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DisplayPolicy, DisplayPower::*, Effect};

    #[test]
    fn startup_handoff_and_unavailable_evidence_do_not_change_tv_power() {
        let mut policy = DisplayPolicy::default();
        assert_eq!(policy.observe("a".into(), On), None);
        assert_eq!(policy.observe("a".into(), Unknown), Some(Effect::Cancel));
        assert_eq!(policy.observe("a".into(), Off), None);
        assert_eq!(policy.observe("a".into(), On), Some(Effect::Wake));
        assert_eq!(policy.observe("b".into(), Off), Some(Effect::Cancel));
        assert_eq!(policy.observe("b".into(), On), Some(Effect::Wake));
        assert_eq!(policy.observe("b".into(), Off), Some(Effect::PowerOff));
    }

    #[test]
    fn resume_waits_for_display_on_and_ignores_duplicate_samples() {
        let mut policy = DisplayPolicy::default();
        policy.pause();
        assert_eq!(policy.observe("a".into(), On), None);
        policy.reset(true);
        assert_eq!(policy.observe("a".into(), Off), None);
        assert_eq!(policy.observe("a".into(), Unknown), Some(Effect::Cancel));
        assert_eq!(policy.observe("a".into(), On), Some(Effect::Wake));
        assert_eq!(policy.observe("a".into(), On), None);
    }
}
