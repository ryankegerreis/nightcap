//! Unsupported controls and shared in-memory backends for deterministic engine tests.

use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};

use super::{Backends, Brightness, Power, Volume};
use crate::settings::EndAction;

pub struct UnsupportedPower;

impl Power for UnsupportedPower {
    fn perform(&mut self, action: EndAction) -> Result<()> {
        bail!("{action:?} is not supported on this platform")
    }
}

pub fn backends() -> Backends {
    Backends {
        brightness: None,
        volume: None,
        power: Box::new(UnsupportedPower),
    }
}

#[derive(Debug, Default)]
pub struct MockLevelState {
    pub level: f32,
    pub get_calls: usize,
    pub set_calls: Vec<f32>,
    pub fail_get: bool,
    pub fail_set: bool,
}

macro_rules! mock_level {
    ($name:ident, $control:ident) => {
        #[derive(Clone, Debug)]
        pub struct $name {
            pub state: Arc<Mutex<MockLevelState>>,
        }

        impl $name {
            pub fn new(level: f32) -> Self {
                Self {
                    state: Arc::new(Mutex::new(MockLevelState {
                        level,
                        ..Default::default()
                    })),
                }
            }
        }

        impl $control for $name {
            fn get(&mut self) -> Result<f32> {
                let mut state = self.state.lock().unwrap();
                state.get_calls += 1;
                if state.fail_get {
                    bail!("mock get failed");
                }
                Ok(state.level)
            }

            fn set(&mut self, level: f32) -> Result<()> {
                let mut state = self.state.lock().unwrap();
                state.set_calls.push(level);
                if state.fail_set {
                    bail!("mock set failed");
                }
                state.level = level;
                Ok(())
            }
        }
    };
}

mock_level!(MockBrightness, Brightness);
mock_level!(MockVolume, Volume);

#[derive(Debug, Default)]
pub struct MockPowerState {
    pub calls: Vec<EndAction>,
    pub fail: bool,
}

#[derive(Clone, Debug, Default)]
pub struct MockPower {
    pub state: Arc<Mutex<MockPowerState>>,
}

impl Power for MockPower {
    fn perform(&mut self, action: EndAction) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        state.calls.push(action);
        if state.fail {
            bail!("mock power action failed");
        }
        Ok(())
    }
}
