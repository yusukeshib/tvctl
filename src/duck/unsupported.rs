//! Ducking is macOS-only (Core Audio process taps); elsewhere nothing is tapped.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

#[derive(Debug, Clone)]
pub struct AudioProcess {
    pub object: u32,
    pub pid: i32,
    pub bundle_id: Option<String>,
    pub running_output: bool,
}

pub fn processes() -> Vec<AudioProcess> {
    Vec::new()
}

pub fn running_output(_object: u32) -> bool {
    false
}

pub fn take_process_list_changed() -> bool {
    false
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Include(Vec<u32>),
    Exclude(Vec<u32>),
}

pub struct Tap;

impl Tap {
    pub fn start(_target: &Target) -> Result<Tap> {
        bail!("ducking is only supported on macOS")
    }

    pub fn take_peak(&self) -> f32 {
        0.0
    }
}

pub fn process_name(_pid: i32) -> Option<String> {
    None
}

pub fn process_path(_pid: i32) -> Option<PathBuf> {
    None
}

pub fn parent_pid(_pid: i32) -> Option<i32> {
    None
}

pub fn responsible_pid(_pid: i32) -> Option<i32> {
    None
}

pub fn bundle_id(_path: &Path) -> Option<String> {
    None
}
